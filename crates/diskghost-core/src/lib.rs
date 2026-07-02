//! diskghost-core — fast parallel disk scanning and duplicate detection.
//!
//! The engine behind Diskghost. Two jobs:
//!   * [`scan`] — total size, biggest sub-folders and biggest files under a root.
//!   * [`find_duplicates`] — byte-identical files, found cheaply (group by size,
//!     then hash only the size-collisions with BLAKE3, in parallel).
//!
//! Everything is `serde`-serialisable so a CLI, a GUI or an agent can consume it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use serde::Serialize;

/// A single file with its size in bytes.
#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub path: PathBuf,
    pub size: u64,
}

/// Aggregated size of a directory.
#[derive(Debug, Clone, Serialize)]
pub struct DirSize {
    pub path: PathBuf,
    pub size: u64,
    pub file_count: u64,
}

/// Summary of a scan.
#[derive(Debug, Serialize)]
pub struct ScanReport {
    pub root: PathBuf,
    pub total_size: u64,
    pub total_files: u64,
    pub total_dirs: u64,
    /// Immediate children of `root`, largest first.
    pub children: Vec<DirSize>,
    /// Largest individual files, largest first.
    pub top_files: Vec<FileEntry>,
}

/// A group of byte-identical files.
#[derive(Debug, Serialize)]
pub struct DupGroup {
    pub hash: String,
    pub size: u64,
    pub files: Vec<PathBuf>,
    /// Bytes reclaimable if all but one copy is removed.
    pub wasted: u64,
}

/// Walk `root` recursively and return every file with its size. I/O errors
/// (permission denied, races) are skipped rather than aborting the whole scan.
pub fn walk_files(root: &Path) -> Vec<FileEntry> {
    jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let size = e.metadata().ok()?.len();
            Some(FileEntry {
                path: e.path(),
                size,
            })
        })
        .collect()
}

/// Scan `root`: total size, the `top_n` biggest immediate sub-folders and the
/// `top_n` biggest individual files.
pub fn scan(root: &Path, top_n: usize) -> ScanReport {
    let files = walk_files(root);

    let total_size: u64 = files.iter().map(|f| f.size).sum();
    let total_files = files.len() as u64;

    let mut by_child: HashMap<PathBuf, DirSize> = HashMap::new();
    for f in &files {
        if let Ok(rel) = f.path.strip_prefix(root) {
            if let Some(first) = rel.components().next() {
                let key = root.join(first.as_os_str());
                let entry = by_child.entry(key.clone()).or_insert_with(|| DirSize {
                    path: key,
                    size: 0,
                    file_count: 0,
                });
                entry.size += f.size;
                entry.file_count += 1;
            }
        }
    }
    let mut children: Vec<DirSize> = by_child.into_values().collect();
    children.sort_by_key(|e| std::cmp::Reverse(e.size));
    children.truncate(top_n);

    let mut top_files = files;
    top_files.sort_by_key(|e| std::cmp::Reverse(e.size));
    top_files.truncate(top_n);

    ScanReport {
        root: root.to_path_buf(),
        total_size,
        total_files,
        total_dirs: count_dirs(root),
        children,
        top_files,
    }
}

fn count_dirs(root: &Path) -> u64 {
    jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_dir())
        .count() as u64
}

/// Find groups of byte-identical files under `root`, ignoring files smaller
/// than `min_size` bytes. Cheap: files are grouped by size first, then only the
/// size-collisions are hashed (BLAKE3), in parallel across groups.
pub fn find_duplicates(root: &Path, min_size: u64) -> Vec<DupGroup> {
    let files: Vec<FileEntry> = walk_files(root)
        .into_iter()
        .filter(|f| f.size >= min_size && f.size > 0)
        .collect();

    let mut by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
    for f in files {
        by_size.entry(f.size).or_default().push(f.path);
    }

    let candidates: Vec<(u64, Vec<PathBuf>)> =
        by_size.into_iter().filter(|(_, v)| v.len() > 1).collect();

    let mut groups: Vec<DupGroup> = candidates
        .par_iter()
        .flat_map(|(size, paths)| {
            let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
            for p in paths {
                if let Ok(h) = hash_file(p) {
                    by_hash.entry(h).or_default().push(p.clone());
                }
            }
            by_hash
                .into_iter()
                .filter(|(_, v)| v.len() > 1)
                .map(|(hash, files)| {
                    let wasted = size * (files.len() as u64 - 1);
                    DupGroup {
                        hash,
                        size: *size,
                        files,
                        wasted,
                    }
                })
                .collect::<Vec<_>>()
        })
        .collect();

    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted));
    groups
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Format a byte count as a human-readable string, e.g. `1.5 GB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-test-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
    }

    #[test]
    fn scan_totals_and_children() {
        let d = tmpdir("scan");
        write(&d.join("a/1.bin"), &[0u8; 1000]);
        write(&d.join("a/2.bin"), &[0u8; 500]);
        write(&d.join("b/3.bin"), &[0u8; 200]);
        let r = scan(&d, 10);
        assert_eq!(r.total_size, 1700);
        assert_eq!(r.total_files, 3);
        assert_eq!(
            r.children[0].path.file_name().unwrap().to_str().unwrap(),
            "a"
        );
        assert_eq!(r.children[0].size, 1500);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn duplicates_detected() {
        let d = tmpdir("dupes");
        write(&d.join("x/dup1.bin"), b"hello world duplicate content");
        write(&d.join("y/dup2.bin"), b"hello world duplicate content");
        write(&d.join("z/unique.bin"), b"something else entirely here!");
        let groups = find_duplicates(&d, 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[0].wasted, groups[0].size);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn human_size_formats() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1536), "1.5 KB");
    }
}
