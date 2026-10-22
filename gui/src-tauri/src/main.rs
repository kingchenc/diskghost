// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use diskghost_core::{
    diff, disk_space, find_duplicates_with_progress, reclaim, remove_path, validate_globs,
    DiffReport, DirSize, DupGroup, FileStats, Options, Progress, ReclaimAction, ReclaimReport,
    RemoveMode, RemoveReport, ScanReport, ScanTree, Snapshot,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, LogicalSize, Manager};
use tauri_plugin_dialog::DialogExt;

/// Sub-folders shown inside each folder on the map's second level.
const NESTED_PER_FOLDER: usize = 12;

/// Snapshots kept per scanned root unless the UI asks otherwise.
const DEFAULT_SNAPSHOTS_PER_ROOT: usize = 10;

/// The tree of one scan job plus what has been derived from it.
struct Cached {
    tree: ScanTree,
    /// Views already computed, by folder and list length — going back to a
    /// folder is answered without touching the tree again.
    views: HashMap<(PathBuf, usize), Arc<View>>,
    /// The tree captured as a snapshot (for comparisons), once taken.
    current: Option<Arc<Snapshot>>,
}

impl Cached {
    /// Forget everything derived from the tree (it was changed).
    fn changed(&mut self) {
        self.views.clear();
        self.current = None;
    }
}

type Shared = Arc<RwLock<Cached>>;

/// The `Progress` of every running operation by job id (so `cancel` can flag
/// one of them), and the tree of every scan job by job id. Each tree has its
/// own lock: reading one scan never waits for another.
struct AppState {
    jobs: Mutex<HashMap<u64, Progress>>,
    trees: Mutex<HashMap<u64, Shared>>,
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
    /// `None` = automatic.
    threads: Option<usize>,
}

impl WalkOpts {
    fn to_options(&self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude.clone(),
            threads: self.threads,
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

/// Run blocking work off the async runtime, so no command ever stalls the UI.
async fn blocking<R: Send + 'static>(
    f: impl FnOnce() -> Result<R, String> + Send + 'static,
) -> Result<R, String> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| e.to_string())?
}

/// The tree of job `job`.
fn tree_of(app: &tauri::AppHandle, job: u64) -> Result<Shared, String> {
    let state = app.state::<AppState>();
    let trees = state.trees.lock().map_err(|e| e.to_string())?;
    trees
        .get(&job)
        .cloned()
        .ok_or_else(|| "this scan is no longer in memory — rescan it".to_string())
}

/// Every cached tree, for operations that touch all of them.
fn all_trees(app: &tauri::AppHandle) -> Vec<Shared> {
    let state = app.state::<AppState>();
    let trees = state.trees.lock();
    trees
        .map(|t| t.values().cloned().collect())
        .unwrap_or_default()
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

/// Stored snapshots in `dir`: (timestamp, file), newest first. Format 2
/// snapshots end in `.dgs`, older JSON ones in `.json`.
fn snapshot_files(dir: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(u64, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let ext = path.extension()?.to_str()?;
            if ext != "dgs" && ext != "json" {
                return None;
            }
            Some((path.file_stem()?.to_str()?.parse().ok()?, path))
        })
        .collect();
    out.sort_unstable_by_key(|x| std::cmp::Reverse(x.0));
    out
}

/// Save `snap` into `dir` and drop the oldest ones beyond `keep`.
fn store_snapshot(dir: &Path, snap: &Snapshot, keep: usize) -> Result<(), String> {
    snap.save(&dir.join(format!("{}.dgs", snap.taken_at)))
        .map_err(|e| e.to_string())?;
    for (_, old) in snapshot_files(dir).into_iter().skip(keep.max(1)) {
        let _ = std::fs::remove_file(old);
    }
    Ok(())
}

/// Capture the tree as a snapshot once and keep it for comparisons.
fn current_snapshot(shared: &Shared) -> Result<Arc<Snapshot>, String> {
    if let Some(s) = shared.read().map_err(|e| e.to_string())?.current.clone() {
        return Ok(s);
    }
    let snap = {
        let c = shared.read().map_err(|e| e.to_string())?;
        Arc::new(Snapshot::capture(&c.tree))
    };
    if let Ok(mut c) = shared.write() {
        c.current = Some(snap.clone());
    }
    Ok(snap)
}

/// Walk `path` as job `job` (live progress, cancellable), keep the tree for
/// the job and return the view of its root. A complete walk is also recorded
/// as a snapshot — in the background, so the result shows up at once.
#[tauri::command]
async fn scan_dir(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    top: usize,
    opts: WalkOpts,
    keep_snapshots: Option<usize>,
) -> Result<View, String> {
    let running = Running::start(&app, job);
    blocking(move || {
        let requested = PathBuf::from(&path);
        if !requested.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        let tree = ScanTree::build(&requested, &opts.to_options(), &running.progress);
        drop(running);
        let view = view_of(&tree, tree.root(), top).ok_or("scan produced no tree")?;
        let complete = !tree.cancelled();
        let root = tree.root().to_path_buf();
        let shared: Shared = Arc::new(RwLock::new(Cached {
            tree,
            views: HashMap::new(),
            current: None,
        }));
        if let Ok(mut trees) = app.state::<AppState>().trees.lock() {
            trees.insert(job, shared.clone());
        }
        // A cancelled walk is a partial view: browsable, but never recorded
        // as a point in the folder's history.
        if complete {
            let app = app.clone();
            std::thread::spawn(move || {
                let Ok(snap) = current_snapshot(&shared) else {
                    return;
                };
                let keep = keep_snapshots.unwrap_or(DEFAULT_SNAPSHOTS_PER_ROOT);
                let saved =
                    snapshot_dir(&app, &root).and_then(|dir| store_snapshot(&dir, &snap, keep));
                let _ = app.emit("snapshot-saved", (job, saved.err()));
            });
        }
        Ok(view)
    })
    .await
}

/// The view of `path` inside job `job`'s tree — never walks the disk.
#[tauri::command]
async fn view(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    top: usize,
) -> Result<Arc<View>, String> {
    blocking(move || {
        let shared = tree_of(&app, job)?;
        let key = (PathBuf::from(&path), top);
        if let Some(v) = shared.read().map_err(|e| e.to_string())?.views.get(&key) {
            return Ok(v.clone());
        }
        let v = {
            let c = shared.read().map_err(|e| e.to_string())?;
            Arc::new(
                view_of(&c.tree, &key.0, top).ok_or_else(|| format!("not in this scan: {path}"))?,
            )
        };
        if let Ok(mut c) = shared.write() {
            c.views.insert(key, v.clone());
        }
        Ok(v)
    })
    .await
}

/// Drop job `job`'s tree (its scan was closed), freeing its memory.
#[tauri::command]
async fn forget_tree(app: tauri::AppHandle, job: u64) {
    // The last reference may hold millions of nodes: free them off-thread.
    let removed = app
        .state::<AppState>()
        .trees
        .lock()
        .ok()
        .and_then(|mut t| t.remove(&job));
    let _ = tauri::async_runtime::spawn_blocking(move || drop(removed)).await;
}

/// One stored snapshot.
#[derive(Serialize)]
struct SnapshotInfo {
    taken_at: u64,
    /// Size of the file on disk.
    bytes: u64,
    /// Whether it records files (format 2) or only folders.
    files: bool,
}

/// Snapshot history of the root job `job` scanned.
#[derive(Serialize)]
struct SnapshotList {
    root: String,
    /// Newest first.
    snapshots: Vec<SnapshotInfo>,
}

#[tauri::command]
async fn list_snapshots(app: tauri::AppHandle, job: u64) -> Result<SnapshotList, String> {
    blocking(move || {
        let root = tree_of(&app, job)?
            .read()
            .map_err(|e| e.to_string())?
            .tree
            .root()
            .to_path_buf();
        let dir = snapshot_dir(&app, &root)?;
        let snapshots = snapshot_files(&dir)
            .into_iter()
            .map(|(taken_at, file)| SnapshotInfo {
                taken_at,
                bytes: std::fs::metadata(&file).map_or(0, |m| m.len()),
                files: file.extension().is_some_and(|e| e == "dgs"),
            })
            .collect();
        Ok(SnapshotList {
            root: root.to_string_lossy().into_owned(),
            snapshots,
        })
    })
    .await
}

/// Delete the snapshot taken at `taken_at` of job `job`'s root.
#[tauri::command]
async fn delete_snapshot(app: tauri::AppHandle, job: u64, taken_at: u64) -> Result<(), String> {
    blocking(move || {
        let root = tree_of(&app, job)?
            .read()
            .map_err(|e| e.to_string())?
            .tree
            .root()
            .to_path_buf();
        let dir = snapshot_dir(&app, &root)?;
        for (ts, file) in snapshot_files(&dir) {
            if ts == taken_at {
                std::fs::remove_file(file).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    })
    .await
}

/// What changed under `path` in job `job`'s tree since the snapshot taken at
/// `taken_at`: folders and — for snapshots that record them — single files.
#[tauri::command]
async fn diff_since(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    taken_at: u64,
    top: usize,
) -> Result<DiffReport, String> {
    blocking(move || {
        let shared = tree_of(&app, job)?;
        let root = shared
            .read()
            .map_err(|e| e.to_string())?
            .tree
            .root()
            .to_path_buf();
        let dir = snapshot_dir(&app, &root)?;
        let file = snapshot_files(&dir)
            .into_iter()
            .find(|(ts, _)| *ts == taken_at)
            .map(|(_, f)| f)
            .ok_or("that snapshot no longer exists")?;
        let old = Snapshot::load(&file).map_err(|e| e.to_string())?;
        let now = current_snapshot(&shared)?;
        let mut report = diff(&old, &now, Path::new(&path)).map_err(|e| e.to_string())?;
        report.truncate(top);
        Ok(report)
    })
    .await
}

/// Find duplicate files as job `job` (progress emitted live).
#[tauri::command]
async fn find_dupes(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    mb: u64,
    opts: WalkOpts,
) -> Result<Vec<DupGroup>, String> {
    let running = Running::start(&app, job);
    blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(find_duplicates_with_progress(
            &p,
            mb * 1024 * 1024,
            &opts.to_options(),
            &running.progress,
        ))
    })
    .await
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

/// Take paths that are gone from disk out of every tree holding them.
fn forget_paths(app: &tauri::AppHandle, paths: &[PathBuf]) {
    for shared in all_trees(app) {
        let Ok(mut c) = shared.write() else { continue };
        let root = c.tree.root().to_path_buf();
        let mut touched = false;
        for p in paths.iter().filter(|p| p.starts_with(&root) && !p.exists()) {
            touched |= c.tree.remove(p);
        }
        if touched {
            c.tree.refresh_disk_space();
            c.changed();
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
/// Deleted copies are taken out of every open scan.
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
    blocking(move || {
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
            forget_paths(&app, &touched);
        }
        Ok(ReclaimReport {
            removed,
            reclaimed,
            errors,
            dry_run,
        })
    })
    .await
}

/// Remove a file or folder as job `job` — permanently or to the OS trash.
/// `apply=false` is a dry run that only reports what would go. Progress is
/// emitted live and the operation is cancellable. Refuses a filesystem root as
/// a safety net. Whatever is gone afterwards is taken out of every open scan
/// in place (no re-walk).
#[tauri::command]
async fn remove_path_cmd(
    app: tauri::AppHandle,
    job: u64,
    path: String,
    trash: bool,
    apply: bool,
) -> Result<RemoveReport, String> {
    let running = Running::start(&app, job);
    blocking(move || {
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
        let report = remove_path(&p, mode, !apply, &running.progress);
        if apply {
            forget_paths(&app, std::slice::from_ref(&p));
        }
        Ok(report)
    })
    .await
}

/// Open a native folder picker that allows several folders. Returns the
/// chosen paths (empty if cancelled).
#[tauri::command]
async fn pick_folders(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    blocking(move || {
        Ok(app
            .dialog()
            .file()
            .blocking_pick_folders()
            .unwrap_or_default()
            .into_iter()
            .map(|p| p.to_string())
            .collect())
    })
    .await
}

/// The exclude patterns that are not valid globs.
#[tauri::command]
async fn check_globs(patterns: Vec<String>) -> Vec<String> {
    validate_globs(&patterns)
}

/// The app's version, for the title bar.
#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
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
/// Off the UI thread: a sleeping network drive can take seconds to answer.
#[tauri::command]
async fn list_drives() -> Result<Vec<Drive>, String> {
    blocking(|| Ok(drives().into_iter().filter(|d| d.total > 0).collect())).await
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

fn read_settings(app: &tauri::AppHandle) -> serde_json::Value {
    settings_file(app)
        .ok()
        .and_then(|f| std::fs::read(f).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// The saved settings as the UI stored them (`null` before the first save).
#[tauri::command]
async fn load_settings(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    blocking(move || Ok(read_settings(&app))).await
}

/// Persist the UI's settings. The backend only reads `window_size` from them.
#[tauri::command]
async fn save_settings(app: tauri::AppHandle, settings: serde_json::Value) -> Result<(), String> {
    blocking(move || {
        let file = settings_file(&app)?;
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(&settings).map_err(|e| e.to_string())?;
        std::fs::write(file, json).map_err(|e| e.to_string())
    })
    .await
}

/// Size the main window for the screen it opens on, then show it. The
/// `window_size` setting picks a size in logical pixels: `auto` (the
/// default) and `1440p` ask for 2560×1440, `1080p` and `4k` for theirs. A size
/// that does not fit the monitor's work area — or `maximized` — maximizes.
fn place_window(app: &tauri::AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let settings = read_settings(app);
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
            view,
            find_dupes,
            cancel,
            reclaim_dupes,
            remove_path_cmd,
            pick_folders,
            list_snapshots,
            delete_snapshot,
            diff_since,
            forget_tree,
            list_drives,
            check_globs,
            load_settings,
            save_settings,
            app_version
        ])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}
