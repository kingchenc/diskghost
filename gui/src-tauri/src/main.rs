// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use diskghost_core::{
    find_duplicates_with, reclaim, scan_with, DupGroup, Options, ReclaimAction, ReclaimReport,
    ScanReport,
};
use serde::Deserialize;
use tauri_plugin_dialog::DialogExt;

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

/// Scan a directory (runs on a blocking thread so the UI stays responsive).
#[tauri::command]
async fn scan_dir(path: String, top: usize, opts: WalkOpts) -> Result<ScanReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(scan_with(&p, top, &opts.into_options()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Find duplicate files under a directory (min size in MB).
#[tauri::command]
async fn find_dupes(path: String, mb: u64, opts: WalkOpts) -> Result<Vec<DupGroup>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(find_duplicates_with(&p, mb * 1024 * 1024, &opts.into_options()))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One reclaim job: keep `keep`, act on each path in `remove` (all `size` bytes).
#[derive(Deserialize)]
struct ReclaimJob {
    keep: String,
    remove: Vec<String>,
    size: u64,
}

/// Reclaim space across many duplicate groups. `action` is delete/trash/hardlink;
/// `dry_run` reports what would happen without changing anything.
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
    let (tx, rx) = std::sync::mpsc::channel();
    app.dialog()
        .file()
        .pick_folder(move |res| {
            let _ = tx.send(res);
        });
    rx.recv().ok().flatten().map(|p| p.to_string())
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            scan_dir,
            find_dupes,
            reclaim_dupes,
            pick_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}
