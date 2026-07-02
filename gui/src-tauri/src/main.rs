// Hide the console window on Windows release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;

use diskghost_core::{find_duplicates, scan, DupGroup, ScanReport};

/// Scan a directory. Runs on a blocking thread so the UI stays responsive.
#[tauri::command]
async fn scan_dir(path: String, top: usize) -> Result<ScanReport, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(scan(&p, top))
    })
    .await
    .map_err(|e| e.to_string())?
}

/// Find duplicate files under a directory (min size in MB).
#[tauri::command]
async fn find_dupes(path: String, mb: u64) -> Result<Vec<DupGroup>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !p.is_dir() {
            return Err(format!("not a directory: {path}"));
        }
        Ok(find_duplicates(&p, mb * 1024 * 1024))
    })
    .await
    .map_err(|e| e.to_string())?
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![scan_dir, find_dupes])
        .run(tauri::generate_context!())
        .expect("error while running Diskghost");
}
