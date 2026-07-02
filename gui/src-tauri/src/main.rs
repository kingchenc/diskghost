// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use diskghost_core::{
    find_duplicates_with_progress, reclaim, scan_with_progress, DupGroup, Options, Progress,
    ReclaimAction, ReclaimReport, ScanReport,
};
use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

/// Holds the `Progress` of the operation currently running, so `cancel` can flag it.
struct AppState {
    current: Mutex<Progress>,
}

/// Payload pushed to the frontend as a scan/search runs.
#[derive(Clone, Serialize)]
struct ProgressPayload {
    files: u64,
    bytes: u64,
}

/// Walk options coming from the frontend.
#[derive(Deserialize, Default)]
#[serde(default)]
struct WalkOpts {
    exclude: Vec<String>,
    max_depth: Option<usize>,
    follow_symlinks: bool,
}

impl WalkOpts {
    fn into_options(self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude,
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

/// Scan a directory (blocking work off-thread; progress emitted live).
#[tauri::command]
async fn scan_dir(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    path: String,
    top: usize,
    opts: WalkOpts,
) -> Result<ScanReport, String> {
    let progress = Progress::default();
    register(&state, &progress);
    let opts = opts.into_options();
    let done = Arc::new(AtomicBool::new(false));
    let _done = DoneGuard(done.clone());
    spawn_emitter(app.clone(), progress.clone(), done);

    let job = progress.clone();
    let res = tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(scan_with_progress(&p, top, &opts, &job))
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
    let opts = opts.into_options();
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
#[tauri::command]
async fn reclaim_dupes(
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
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            find_dupes,
            cancel,
            reclaim_dupes,
            pick_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}
