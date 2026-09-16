// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use diskghost_core::{
    diff, find_duplicates_with_progress, reclaim, remove_path, DiffReport, DupGroup, Options,
    Progress, ReclaimAction, ReclaimReport, RemoveMode, RemoveReport, ScanReport, ScanTree,
    Snapshot,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

/// How many snapshots to keep per scanned root.
const SNAPSHOTS_PER_ROOT: usize = 20;

/// The tree of the last complete walk, plus the options it was walked with.
/// Drilling into any folder below its root is served from here, no I/O.
struct Cached {
    tree: ScanTree,
    opts: WalkOpts,
}

/// Holds the `Progress` of the operation currently running (so `cancel` can
/// flag it) and the cached scan tree.
struct AppState {
    current: Mutex<Progress>,
    cache: Mutex<Option<Cached>>,
}

/// Payload pushed to the frontend as a scan/search runs.
#[derive(Clone, Serialize)]
struct ProgressPayload {
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

/// Emit a `progress` event every ~120 ms until `done` is set.
fn spawn_emitter(app: tauri::AppHandle, progress: Progress, done: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            let _ = app.emit(
                "progress",
                ProgressPayload {
                    files: progress.files(),
                    bytes: progress.bytes(),
                },
            );
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });
}

/// Sets the `done` flag when dropped, so the emitter thread always stops — even
/// if the scan job panics and the command returns early via `?`.
struct DoneGuard(Arc<AtomicBool>);
impl Drop for DoneGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Register `progress` as the cancellable operation, drop the lock before await.
fn register(state: &tauri::State<'_, AppState>, progress: &Progress) {
    if let Ok(mut cur) = state.current.lock() {
        *cur = progress.clone();
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

/// Scan a directory. Served from the cached tree when the path lies below the
/// last complete walk (same options) — otherwise the tree is walked afresh off
/// the async thread with live progress, cached, and recorded as a snapshot.
/// `refresh` forces a fresh walk.
#[tauri::command]
async fn scan_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    top: usize,
    opts: WalkOpts,
    refresh: bool,
) -> Result<ScanReport, String> {
    let requested = PathBuf::from(&path);
    if !refresh {
        if let Ok(cache) = state.cache.lock() {
            if let Some(c) = cache.as_ref() {
                if c.opts == opts {
                    if let Some(report) = c.tree.report(&requested, top) {
                        return Ok(report);
                    }
                }
            }
        }
    }

    let progress = Progress::default();
    register(&state, &progress);
    let done = Arc::new(AtomicBool::new(false));
    let _done = DoneGuard(done.clone());
    spawn_emitter(app.clone(), progress.clone(), done);

    let job = progress.clone();
    let handle = app.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        if !requested.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        let tree = ScanTree::build(&requested, &opts.to_options(), &job);
        let report = tree.root_report(top);
        // A cancelled walk is a partial view: show it, but never cache it or
        // let it pose as a point in the folder's history.
        if !tree.cancelled() {
            let dir = snapshot_dir(&handle, tree.root())?;
            store_snapshot(&dir, &Snapshot::capture(&tree, top))?;
            with_cache(&handle, |cache| *cache = Some(Cached { tree, opts }));
        }
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?;

    let _ = app.emit(
        "progress",
        ProgressPayload {
            files: progress.files(),
            bytes: progress.bytes(),
        },
    );
    res
}

/// Run `f` on the cached-tree slot. A poisoned lock means nothing happens.
fn with_cache<R>(app: &tauri::AppHandle, f: impl FnOnce(&mut Option<Cached>) -> R) -> Option<R> {
    let state = app.state::<AppState>();
    let mut guard = state.cache.lock().ok()?;
    Some(f(&mut guard))
}

/// Forget the cached tree, so the next scan walks the disk again.
fn invalidate(app: &tauri::AppHandle) {
    with_cache(app, |cache| *cache = None);
}

/// Snapshot history of the scanned root that `path` belongs to.
#[derive(Serialize)]
struct SnapshotList {
    /// The scanned root the history is about.
    root: String,
    /// Unix timestamps of the stored snapshots, newest first.
    taken_at: Vec<u64>,
}

/// List the snapshots recorded for the root the cached tree was built from
/// (or, without a cached tree, for `path` itself as a root).
#[tauri::command]
fn list_snapshots(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
) -> Result<SnapshotList, String> {
    let requested = PathBuf::from(&path);
    let root = match state.cache.lock() {
        Ok(cache) => match cache.as_ref() {
            Some(c) if c.tree.contains(&requested) => c.tree.root().to_path_buf(),
            _ => requested,
        },
        Err(_) => requested,
    };
    let dir = snapshot_dir(&app, &root)?;
    Ok(SnapshotList {
        root: root.to_string_lossy().into_owned(),
        taken_at: snapshot_times(&dir),
    })
}

/// What changed under `path` (which must lie in the cached tree) since the
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
        let cache = state.cache.lock().map_err(|e| e.to_string())?;
        let Some(c) = cache.as_ref().filter(|c| c.tree.contains(&at)) else {
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

/// Find duplicate files (blocking work off-thread; progress emitted live).
#[tauri::command]
async fn find_dupes(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    mb: u64,
    opts: WalkOpts,
) -> Result<Vec<DupGroup>, String> {
    let progress = Progress::default();
    register(&state, &progress);
    let opts = opts.to_options();
    let done = Arc::new(AtomicBool::new(false));
    let _done = DoneGuard(done.clone());
    spawn_emitter(app.clone(), progress.clone(), done);

    let job = progress.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(find_duplicates_with_progress(
            &p,
            mb * 1024 * 1024,
            &opts,
            &job,
        ))
    })
    .await
    .map_err(|e| e.to_string())?;

    res
}

/// Request cancellation of the currently running scan/search.
#[tauri::command]
fn cancel(state: tauri::State<'_, AppState>) {
    if let Ok(cur) = state.current.lock() {
        cur.cancel();
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
/// A real reclaim changes files the cached tree may hold, so the cache is dropped.
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
        for j in jobs {
            let keep = PathBuf::from(&j.keep);
            let remove: Vec<PathBuf> = j.remove.iter().map(PathBuf::from).collect();
            let r = reclaim(&keep, &remove, j.size, act, dry_run);
            removed += r.removed;
            reclaimed += r.reclaimed;
            errors.extend(r.errors);
        }
        if !dry_run && removed > 0 {
            invalidate(&app);
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

/// Remove a file or folder — permanently or to the OS trash. `apply=false` is a
/// dry run that only reports what would go. Progress is emitted live and the
/// operation is cancellable. Refuses a filesystem root as a safety net. After a
/// clean removal the cached tree is updated in place (no re-walk); if anything
/// failed the cache is dropped so the next scan reflects the disk.
#[tauri::command]
async fn remove_path_cmd(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    trash: bool,
    apply: bool,
) -> Result<RemoveReport, String> {
    let progress = Progress::default();
    register(&state, &progress);
    let done = Arc::new(AtomicBool::new(false));
    let _done = DoneGuard(done.clone());
    spawn_emitter(app.clone(), progress.clone(), done);

    let job = progress.clone();
    tauri::async_runtime::spawn_blocking(move || {
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
        let report = remove_path(&p, mode, !apply, &job);
        if apply {
            if report.errors.is_empty() {
                with_cache(&app, |cache| {
                    if let Some(c) = cache.as_mut() {
                        c.tree.remove(&p);
                        c.tree.refresh_disk_space();
                    }
                });
            } else {
                invalidate(&app);
            }
        }
        Ok(report)
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Open a native folder picker. Returns the chosen path, or `None` if cancelled.
#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    // Run the blocking picker off the async executor thread (avoids blocking it).
    tauri::async_runtime::spawn_blocking(move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .map(|p| p.to_string())
    })
    .await
    .ok()
    .flatten()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            app.manage(AppState {
                current: Mutex::new(Progress::default()),
                cache: Mutex::new(None),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            find_dupes,
            cancel,
            reclaim_dupes,
            remove_path_cmd,
            pick_folder,
            list_snapshots,
            diff_since
        ])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}
