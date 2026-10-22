// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use diskghost_core::{
    diff, disk_space, find_duplicates_with_progress, reclaim, remove_path, validate_globs,
    DiffReport, DirSize, DupGroup, FileStats, Options, Progress, ReclaimAction, ReclaimReport,
    RemoveMode, RemoveReport, ScanReport, ScanTree, Snapshot,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, LogicalSize, Manager};
use tauri_plugin_dialog::DialogExt;

/// How many snapshots to keep per scanned root.
const SNAPSHOTS_PER_ROOT: usize = 20;

/// Sub-folders shown inside each folder on the map's second level.
const NESTED_PER_FOLDER: usize = 12;

/// The tree of a complete walk, plus the options it was walked with.
/// Drilling into any folder below its root is served from here, no I/O.
struct Cached {
    tree: ScanTree,
    opts: WalkOpts,
}

/// The `Progress` of every running operation by job id (so `cancel` can flag
/// one of them), and the trees of every complete walk by root — several scans
/// can run and stay browsable side by side.
struct AppState {
    jobs: Mutex<HashMap<u64, Progress>>,
    trees: Mutex<HashMap<PathBuf, Cached>>,
}

/// Payload pushed to the frontend as a scan/search runs.
#[derive(Clone, Serialize)]
struct ProgressPayload {
    job: u64,
    files: u64,
    bytes: u64,
}

/// Walk options coming from the frontend.
#[derive(Deserialize, Default, Clone, PartialEq, Eq)]
#[serde(default)]
struct WalkOpts {
    exclude: Vec<String>,
    max_depth: Option<usize>,
    follow_symlinks: bool,
}

impl WalkOpts {
    fn to_options(&self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude.clone(),
        }
    }
}

/// A scan report plus what the dashboard draws next to it.
#[derive(Serialize)]
struct View {
    #[serde(flatten)]
    report: ScanReport,
    /// The biggest sub-folders of each of `children`, in the same order — the
    /// second level of the map.
    nested: Vec<Vec<DirSize>>,
    /// Bytes per extension and per size range below the viewed folder.
    stats: FileStats,
}

fn view_of(tree: &ScanTree, path: &Path, top: usize) -> Option<View> {
    let report = tree.report(path, top)?;
    let nested = report
        .children
        .iter()
        .map(|c| {
            tree.children(&c.path, NESTED_PER_FOLDER)
                .unwrap_or_default()
        })
        .collect();
    let stats = tree.file_stats(path)?;
    Some(View {
        report,
        nested,
        stats,
    })
}

/// The cached tree that holds `path` — walked with `opts`, if given. When
/// several do (a drive and a folder on it), the deepest root wins: it is the
/// most specific walk.
fn lookup<'a>(
    trees: &'a HashMap<PathBuf, Cached>,
    path: &Path,
    opts: Option<&WalkOpts>,
) -> Option<&'a Cached> {
    trees
        .values()
        .filter(|c| opts.is_none_or(|o| *o == c.opts) && c.tree.contains(path))
        .max_by_key(|c| c.tree.root().components().count())
}

/// Emit a `progress` event every ~120 ms until `done` is set.
fn spawn_emitter(app: tauri::AppHandle, job: u64, progress: Progress, done: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            let _ = app.emit(
                "progress",
                ProgressPayload {
                    job,
                    files: progress.files(),
                    bytes: progress.bytes(),
                },
            );
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });
}

/// A running operation: registered as cancellable and reporting progress
/// until dropped — even if the job panics and the command returns early.
struct Running {
    app: tauri::AppHandle,
    job: u64,
    progress: Progress,
    done: Arc<AtomicBool>,
}

impl Running {
    fn start(app: &tauri::AppHandle, job: u64) -> Running {
        let progress = Progress::default();
        if let Ok(mut jobs) = app.state::<AppState>().jobs.lock() {
            jobs.insert(job, progress.clone());
        }
        let done = Arc::new(AtomicBool::new(false));
        spawn_emitter(app.clone(), job, progress.clone(), done.clone());
        Running {
            app: app.clone(),
            job,
            progress,
            done,
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        if let Ok(mut jobs) = self.app.state::<AppState>().jobs.lock() {
            jobs.remove(&self.job);
        }
        // A last event with the final counts.
        let _ = self.app.emit(
            "progress",
            ProgressPayload {
                job: self.job,
                files: self.progress.files(),
                bytes: self.progress.bytes(),
            },
        );
    }
}

/// Where snapshots of `root` live: one folder per scanned directory under the
/// app's data dir, files named by their Unix timestamp.
fn snapshot_dir(app: &tauri::AppHandle, root: &Path) -> Result<PathBuf, String> {
    let base = app.path().app_data_dir().map_err(|e| e.to_string())?;
    Ok(base.join("snapshots").join(Snapshot::root_key(root)))
}

/// Timestamps of the snapshots in `dir`, newest first.
fn snapshot_times(dir: &Path) -> Vec<u64> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut times: Vec<u64> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name();
            let stem = name.to_str()?.strip_suffix(".json")?;
            stem.parse().ok()
        })
        .collect();
    times.sort_unstable_by(|a, b| b.cmp(a));
    times
}

fn snapshot_file(dir: &Path, taken_at: u64) -> PathBuf {
    dir.join(format!("{taken_at}.json"))
}

/// Save `snap` into `dir` and drop the oldest ones beyond the retention limit.
fn store_snapshot(dir: &Path, snap: &Snapshot) -> Result<(), String> {
    snap.save(&snapshot_file(dir, snap.taken_at))
        .map_err(|e| e.to_string())?;
    for old in snapshot_times(dir).into_iter().skip(SNAPSHOTS_PER_ROOT) {
        let _ = std::fs::remove_file(snapshot_file(dir, old));
    }
    Ok(())
}

/// Scan a directory as job `job`. Served from a cached tree when the path lies
/// below a complete walk with the same options — otherwise the tree is walked
/// afresh off the async thread with live progress, cached, and recorded as a
/// snapshot. `refresh` forces a fresh walk.
#[tauri::command]
async fn scan_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    job: u64,
    path: String,
    top: usize,
    opts: WalkOpts,
    refresh: bool,
) -> Result<View, String> {
    let requested = PathBuf::from(&path);
    if !refresh {
        if let Ok(trees) = state.trees.lock() {
            if let Some(c) = lookup(&trees, &requested, Some(&opts)) {
                if let Some(view) = view_of(&c.tree, &requested, top) {
                    return Ok(view);
                }
            }
        }
    }

    let running = Running::start(&app, job);
    let progress = running.progress.clone();
    let handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _running = running;
        if !requested.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        let tree = ScanTree::build(&requested, &opts.to_options(), &progress);
        let view = view_of(&tree, tree.root(), top).ok_or("scan produced no tree")?;
        // A cancelled walk is a partial view: show it, but never cache it or
        // let it pose as a point in the folder's history.
        if !tree.cancelled() {
            let dir = snapshot_dir(&handle, tree.root())?;
            store_snapshot(&dir, &Snapshot::capture(&tree, top))?;
            with_trees(&handle, |trees| {
                trees.insert(tree.root().to_path_buf(), Cached { tree, opts })
            });
        }
        Ok(view)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Run `f` on the cached trees. A poisoned lock means nothing happens.
fn with_trees<R>(
    app: &tauri::AppHandle,
    f: impl FnOnce(&mut HashMap<PathBuf, Cached>) -> R,
) -> Option<R> {
    let state = app.state::<AppState>();
    let mut guard = state.trees.lock().ok()?;
    Some(f(&mut guard))
}

/// Forget every cached tree whose root holds one of `paths`, so the next
/// scan there walks the disk again.
fn invalidate(app: &tauri::AppHandle, paths: &[PathBuf]) {
    with_trees(app, |trees| {
        trees.retain(|root, _| !paths.iter().any(|p| p.starts_with(root)))
    });
}

/// Drop the cached tree of `root` (its scan was closed), freeing its memory.
#[tauri::command]
fn forget_tree(app: tauri::AppHandle, root: String) {
    with_trees(&app, |trees| trees.remove(Path::new(&root)));
}

/// Snapshot history of the scanned root that `path` belongs to.
#[derive(Serialize)]
struct SnapshotList {
    /// The scanned root the history is about.
    root: String,
    /// Unix timestamps of the stored snapshots, newest first.
    taken_at: Vec<u64>,
}

/// List the snapshots recorded for the root of the cached tree holding `path`
/// (or, without one, for `path` itself as a root).
#[tauri::command]
fn list_snapshots(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<SnapshotList, String> {
    let requested = PathBuf::from(&path);
    let root = match state.trees.lock() {
        Ok(trees) => match lookup(&trees, &requested, None) {
            Some(c) => c.tree.root().to_path_buf(),
            None => requested,
        },
        Err(_) => requested,
    };
    let dir = snapshot_dir(&app, &root)?;
    Ok(SnapshotList {
        root: root.to_string_lossy().into_owned(),
        taken_at: snapshot_times(&dir),
    })
}

/// What changed under `path` (which must lie in a cached tree) since the
/// snapshot taken at `taken_at`: the current tree is compared against it.
#[tauri::command]
async fn diff_since(
    app: tauri::AppHandle,
    path: String,
    taken_at: u64,
    top: usize,
) -> Result<DiffReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let at = PathBuf::from(&path);
        let state = app.state::<AppState>();
        let trees = state.trees.lock().map_err(|e| e.to_string())?;
        let Some(c) = lookup(&trees, &at, None) else {
            return Err(format!("scan {path} first"));
        };
        let dir = snapshot_dir(&app, c.tree.root())?;
        let old = Snapshot::load(&snapshot_file(&dir, taken_at)).map_err(|e| e.to_string())?;
        let now = Snapshot::capture(&c.tree, top);
        let mut report = diff(&old, &now, &at).map_err(|e| e.to_string())?;
        report.truncate(top);
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Find duplicate files as job `job` (blocking work off-thread; progress
/// emitted live).
#[tauri::command]
async fn find_dupes(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    mb: u64,
    opts: WalkOpts,
) -> Result<Vec<DupGroup>, String> {
    let running = Running::start(&app, job);
    let progress = running.progress.clone();
    let opts = opts.to_options();
    tauri::async_runtime::spawn_blocking(move || {
        let _running = running;
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(find_duplicates_with_progress(
            &p,
            mb * 1024 * 1024,
            &opts,
            &progress,
        ))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Request cancellation of job `job`, if it is still running.
#[tauri::command]
fn cancel(state: tauri::State<'_, AppState>, job: u64) {
    if let Ok(jobs) = state.jobs.lock() {
        if let Some(p) = jobs.get(&job) {
            p.cancel();
        }
    }
}

/// One reclaim job: keep `keep`, act on each path in `remove` (all `size` bytes).
#[derive(Deserialize)]
struct ReclaimJob {
    keep: String,
    remove: Vec<String>,
    size: u64,
}

/// Reclaim space across many duplicate groups. `action` is delete/trash/hardlink.
/// A real reclaim changes files the cached trees may hold, so those are dropped.
#[tauri::command]
async fn reclaim_dupes(
    app: tauri::AppHandle,
    jobs: Vec<ReclaimJob>,
    action: String,
    dry_run: bool,
) -> Result<ReclaimReport, String> {
    let act = match action.as_str() {
        "delete" => ReclaimAction::Delete,
        "trash" => ReclaimAction::Trash,
        "hardlink" => ReclaimAction::Hardlink,
        other => return Err(format!("unknown action: {other}")),
    };
    tauri::async_runtime::spawn_blocking(move || {
        let mut removed = 0usize;
        let mut reclaimed = 0u64;
        let mut errors = Vec::new();
        let mut touched = Vec::new();
        for j in jobs {
            let keep = PathBuf::from(&j.keep);
            let remove: Vec<PathBuf> = j.remove.iter().map(PathBuf::from).collect();
            let r = reclaim(&keep, &remove, j.size, act, dry_run);
            removed += r.removed;
            reclaimed += r.reclaimed;
            errors.extend(r.errors);
            touched.extend(remove);
        }
        if !dry_run && removed > 0 {
            invalidate(&app, &touched);
        }
        Ok(ReclaimReport {
            removed,
            reclaimed,
            errors,
            dry_run,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Remove a file or folder as job `job` — permanently or to the OS trash.
/// `apply=false` is a dry run that only reports what would go. Progress is
/// emitted live and the operation is cancellable. Refuses a filesystem root as
/// a safety net. After a clean removal every cached tree holding the path is
/// updated in place (no re-walk); if anything failed those trees are dropped
/// so the next scan reflects the disk.
#[tauri::command]
async fn remove_path_cmd(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    trash: bool,
    apply: bool,
) -> Result<RemoveReport, String> {
    let running = Running::start(&app, job);
    let progress = running.progress.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _running = running;
        let p = PathBuf::from(&path);
        if !p.exists() {
            return Err(format!("no such path: {path}"));
        }
        if p.parent().is_none() {
            return Err(format!("refusing to remove a filesystem root: {path}"));
        }
        let mode = if trash {
            RemoveMode::Trash
        } else {
            RemoveMode::Delete
        };
        let report = remove_path(&p, mode, !apply, &progress);
        if apply {
            if report.errors.is_empty() {
                with_trees(&app, |trees| {
                    for c in trees.values_mut() {
                        if c.tree.remove(&p) {
                            c.tree.refresh_disk_space();
                        }
                    }
                });
            } else {
                invalidate(&app, std::slice::from_ref(&p));
            }
        }
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Open a native folder picker that allows several folders. Returns the
/// chosen paths (empty if cancelled).
#[tauri::command]
async fn pick_folders(app: tauri::AppHandle) -> Vec<String> {
    // Run the blocking picker off the async executor thread (avoids blocking it).
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folders()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string())
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// The exclude patterns that are not valid globs.
#[tauri::command]
fn check_globs(patterns: Vec<String>) -> Vec<String> {
    validate_globs(&patterns)
}

/// A drive / mounted volume that can be scanned as a whole.
#[derive(Serialize)]
struct Drive {
    /// Root path to scan (`C:\`, `/`, `/Volumes/Backup`).
    path: String,
    /// Volume label, if it has one.
    label: String,
    /// Filesystem (`NTFS`, …), if known.
    fs: String,
    /// `fixed`, `removable`, `network` or `other`.
    kind: String,
    total: u64,
    free: u64,
}

/// Every drive with a readable filesystem, in drive-letter / mount order.
#[tauri::command]
fn list_drives() -> Vec<Drive> {
    drives().into_iter().filter(|d| d.total > 0).collect()
}

#[cfg(windows)]
fn drives() -> Vec<Drive> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetLogicalDrives() -> u32;
        fn GetDriveTypeW(root: *const u16) -> u32;
        fn GetVolumeInformationW(
            root: *const u16,
            name: *mut u16,
            name_len: u32,
            serial: *mut u32,
            max_component: *mut u32,
            flags: *mut u32,
            fs_name: *mut u16,
            fs_name_len: u32,
        ) -> i32;
    }
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    const DRIVE_REMOTE: u32 = 4;
    const DRIVE_CDROM: u32 = 5;

    let mask = unsafe { GetLogicalDrives() };
    let mut out = Vec::new();
    for i in 0..26u8 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let path = format!("{}:\\", (b'A' + i) as char);
        let wide: Vec<u16> = std::ffi::OsStr::new(&path)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let kind = match unsafe { GetDriveTypeW(wide.as_ptr()) } {
            DRIVE_CDROM => continue,
            DRIVE_FIXED => "fixed",
            DRIVE_REMOVABLE => "removable",
            DRIVE_REMOTE => "network",
            _ => "other",
        };
        let mut name = [0u16; 261];
        let mut fs = [0u16; 261];
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                fs.as_mut_ptr(),
                fs.len() as u32,
            )
        } != 0;
        let text = |buf: &[u16]| {
            let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..end])
        };
        let (total, free) = disk_space(Path::new(&path));
        out.push(Drive {
            label: if ok { text(&name) } else { String::new() },
            fs: if ok { text(&fs) } else { String::new() },
            kind: kind.into(),
            path,
            total,
            free,
        });
    }
    out
}

#[cfg(not(windows))]
fn drives() -> Vec<Drive> {
    let mut roots = vec![PathBuf::from("/")];
    let user = std::env::var("USER").unwrap_or_default();
    for parent in [
        PathBuf::from("/Volumes"),
        PathBuf::from("/media"),
        PathBuf::from("/media").join(&user),
        PathBuf::from("/run/media").join(&user),
        PathBuf::from("/mnt"),
    ] {
        let Ok(entries) = std::fs::read_dir(&parent) else {
            continue;
        };
        for e in entries.flatten() {
            // Skip symlinks (macOS links the boot volume back to `/`) and
            // the per-user folders that only hold the real mounts.
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() && e.file_name().to_string_lossy() != user {
                roots.push(e.path());
            }
        }
    }
    roots
        .into_iter()
        .map(|p| {
            let (total, free) = disk_space(&p);
            Drive {
                label: p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                fs: String::new(),
                kind: if p == Path::new("/") {
                    "fixed"
                } else {
                    "other"
                }
                .into(),
                path: p.to_string_lossy().into_owned(),
                total,
                free,
            }
        })
        .collect()
}

/// Where the UI's settings are kept.
fn settings_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("settings.json"))
}

/// The saved settings as the UI stored them (`null` before the first save).
#[tauri::command]
fn load_settings(app: tauri::AppHandle) -> serde_json::Value {
    settings_file(&app)
        .ok()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// Persist the UI's settings. The backend only reads `window_size` from them.
#[tauri::command]
fn save_settings(app: tauri::AppHandle, settings: serde_json::Value) -> Result<(), String> {
    let file = settings_file(&app)?;
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
    std::fs::write(file, json).map_err(|e| e.to_string())
}

/// Size the main window for the screen it opens on, then show it. The
/// `window_size` setting picks a size in logical pixels: `auto` (the
/// default) and `1440p` ask for 2560×1440, `1080p` and `4k` for theirs. A size
/// that does not fit the monitor's work area — or `maximized` — maximizes.
fn place_window(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let settings = load_settings(app.clone());
    let wanted = match settings.get("window_size").and_then(|v| v.as_str()) {
        Some("maximized") => None,
        Some("1080p") => Some((1920.0, 1080.0)),
        Some("4k") => Some((3840.0, 2160.0)),
        _ => Some((2560.0, 1440.0)),
    };
    let monitor = win
        .current_monitor()
        .ok()
        .flatten()
        .or_else(|| win.primary_monitor().ok().flatten());
    if let Some(m) = monitor {
        let scale = m.scale_factor();
        let area = m.work_area().size;
        let (aw, ah) = (area.width as f64 / scale, area.height as f64 / scale);
        match wanted {
            Some((w, h)) if w <= aw && h <= ah => {
                let _ = win.set_size(LogicalSize::new(w, h));
                let _ = win.center();
            }
            _ => {
                let _ = win.maximize();
            }
        }
    }
    let _ = win.show();
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.manage(AppState {
                jobs: Mutex::new(HashMap::new()),
                trees: Mutex::new(HashMap::new()),
            });
            place_window(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            find_dupes,
            cancel,
            reclaim_dupes,
            remove_path_cmd,
            pick_folders,
            list_snapshots,
            diff_since,
            forget_tree,
            list_drives,
            check_globs,
            load_settings,
            save_settings
        ])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}
