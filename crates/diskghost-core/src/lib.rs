//! diskghost-core — fast parallel disk scanning and duplicate detection.
//!
//! The engine behind Diskghost. Two jobs:
//!   * [`scan`] — total size, biggest sub-folders and biggest files under a root.
//!   * [`find_duplicates`] — byte-identical files, found cheaply and *correctly*:
//!     group by size, collapse hard links (same physical file), pre-hash the
//!     first block, then full-hash only the survivors (BLAKE3), all in parallel.
//!
//! Everything is `serde`-serialisable so a CLI, a GUI or an agent can consume it.

use std::collections::{HashMap, HashSet};
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
    /// Entries that could not be read (permissions, races) and were skipped.
    pub skipped: u64,
    /// Real sub-folders only, largest first.
    pub children: Vec<DirSize>,
    /// Bytes of files sitting directly in the scanned root (not in any sub-folder).
    pub root_files_size: u64,
    pub root_files_count: u64,
    /// Largest individual files, largest first.
    pub top_files: Vec<FileEntry>,
}

/// A group of byte-identical files (hard links to the same physical file are
/// collapsed, so `wasted` reflects space that is actually reclaimable).
#[derive(Debug, Serialize)]
pub struct DupGroup {
    pub hash: String,
    pub size: u64,
    pub files: Vec<PathBuf>,
    /// Bytes reclaimable if all but one copy is removed.
    pub wasted: u64,
}

/// First N bytes hashed as a cheap pre-filter before a full-file hash.
const PREFIX_LEN: usize = 4096;

struct Walk {
    files: Vec<FileEntry>,
    dirs: u64,
    skipped: u64,
}

/// Walk `root` recursively in a single pass: collect files (with size), count
/// directories, and count unreadable entries. Symlinks are not followed, so
/// symlink loops cannot cause infinite recursion.
fn walk(root: &Path) -> Walk {
    let mut files = Vec::new();
    let mut dirs = 0u64;
    let mut skipped = 0u64;

    for entry in jwalk::WalkDir::new(root).skip_hidden(false) {
        match entry {
            Ok(e) => {
                let ft = e.file_type();
                if ft.is_dir() {
                    if e.depth() > 0 {
                        dirs += 1; // don't count the root itself
                    }
                } else if ft.is_file() {
                    match e.metadata() {
                        Ok(m) => files.push(FileEntry {
                            path: e.path(),
                            size: m.len(),
                        }),
                        Err(_) => skipped += 1,
                    }
                }
                // Unfollowed symlinks are ignored (see the follow-symlinks option, roadmap).
            }
            Err(_) => skipped += 1,
        }
    }

    Walk {
        files,
        dirs,
        skipped,
    }
}

/// Walk `root` recursively and return every file with its size. I/O errors
/// (permission denied, races) are skipped rather than aborting the whole scan.
pub fn walk_files(root: &Path) -> Vec<FileEntry> {
    walk(root).files
}

/// Scan `root`: total size, the `top_n` biggest immediate sub-folders, the bytes
/// of files sitting directly in the root, and the `top_n` biggest individual files.
pub fn scan(root: &Path, top_n: usize) -> ScanReport {
    let Walk {
        files,
        dirs,
        skipped,
    } = walk(root);

    let total_size: u64 = files.iter().map(|f| f.size).sum();
    let total_files = files.len() as u64;

    let mut by_child: HashMap<PathBuf, DirSize> = HashMap::new();
    let mut root_files_size = 0u64;
    let mut root_files_count = 0u64;

    for f in &files {
        if let Ok(rel) = f.path.strip_prefix(root) {
            let mut comps = rel.components();
            match (comps.next(), comps.next()) {
                // Two or more components: `first` is a real sub-directory.
                (Some(first), Some(_)) => {
                    let key = root.join(first.as_os_str());
                    let entry = by_child.entry(key.clone()).or_insert_with(|| DirSize {
                        path: key,
                        size: 0,
                        file_count: 0,
                    });
                    entry.size += f.size;
                    entry.file_count += 1;
                }
                // Exactly one component: the file sits directly in the root.
                (Some(_), None) => {
                    root_files_size += f.size;
                    root_files_count += 1;
                }
                _ => {}
            }
        }
    }

    let mut children: Vec<DirSize> = by_child.into_values().collect();
    children.sort_by_key(|d| std::cmp::Reverse(d.size));
    children.truncate(top_n);

    let mut top_files = files;
    top_files.sort_by_key(|f| std::cmp::Reverse(f.size));
    top_files.truncate(top_n);

    ScanReport {
        root: root.to_path_buf(),
        total_size,
        total_files,
        total_dirs: dirs,
        skipped,
        children,
        root_files_size,
        root_files_count,
        top_files,
    }
}

/// Find groups of byte-identical files under `root`, ignoring files smaller than
/// `min_size` bytes and zero-byte files. Cheap and correct:
///   1. group by size (a full hash is pointless unless sizes match);
///   2. collapse hard links — entries pointing at the same physical file are not
///      wasted space, so they are counted once;
///   3. pre-hash the first [`PREFIX_LEN`] bytes to drop files that differ early;
///   4. full-hash the survivors with BLAKE3.
///
/// Steps 2-4 run in parallel across size groups.
pub fn find_duplicates(root: &Path, min_size: u64) -> Vec<DupGroup> {
    let files: Vec<FileEntry> = walk(root)
        .files
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
            // (2) collapse hard links to distinct physical files
            let reps = dedup_hard_links(paths);
            if reps.len() < 2 {
                return Vec::new();
            }

            // (3) pre-hash first block
            let mut by_prefix: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
            for p in reps {
                if let Ok(h) = hash_prefix(&p, PREFIX_LEN) {
                    by_prefix.entry(h).or_default().push(p);
                }
            }

            // (4) full-hash the survivors
            let mut out = Vec::new();
            for prefix_group in by_prefix.into_values() {
                if prefix_group.len() < 2 {
                    continue;
                }
                let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
                for p in prefix_group {
                    if let Ok(h) = hash_file(&p) {
                        by_hash.entry(h).or_default().push(p);
                    }
                }
                for (hash, group) in by_hash {
                    if group.len() < 2 {
                        continue;
                    }
                    let wasted = size.saturating_mul(group.len() as u64 - 1);
                    out.push(DupGroup {
                        hash,
                        size: *size,
                        files: group,
                        wasted,
                    });
                }
            }
            out
        })
        .collect();

    groups.sort_by_key(|g| std::cmp::Reverse(g.wasted));
    groups
}

/// Keep one path per distinct physical file. Hard links share a file identity,
/// so they must not be counted as duplicates. [`same_file::Handle`] provides that
/// identity cross-platform (device + inode on Unix, volume-serial + file-index on
/// Windows). Paths whose identity can't be read are kept (we can't prove they are
/// hard links).
fn dedup_hard_links(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen: HashSet<same_file::Handle> = HashSet::new();
    let mut reps = Vec::new();
    for p in paths {
        match same_file::Handle::from_path(p) {
            Ok(h) => {
                if seen.insert(h) {
                    reps.push(p.clone());
                }
            }
            Err(_) => reps.push(p.clone()),
        }
    }
    reps
}

fn hash_prefix(path: &Path, len: usize) -> std::io::Result<[u8; 32]> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; len];
    let mut read = 0usize;
    while read < buf.len() {
        let n = file.read(&mut buf[read..])?;
        if n == 0 {
            break;
        }
        read += n;
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&buf[..read]);
    Ok(*hasher.finalize().as_bytes())
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
    fn loose_root_files_are_not_folders() {
        let d = tmpdir("loose");
        write(&d.join("loose.bin"), &[0u8; 777]);
        write(&d.join("sub/inner.bin"), &[0u8; 300]);
        let r = scan(&d, 10);
        assert!(r
            .children
            .iter()
            .all(|c| c.path.file_name().unwrap() != "loose.bin"));
        assert_eq!(r.root_files_count, 1);
        assert_eq!(r.root_files_size, 777);
        assert_eq!(r.children.len(), 1); // only `sub`
        assert_eq!(r.total_size, 1077); // both still counted
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn duplicates_detected() {
        let d = tmpdir("dupes");
        write(
            &d.join("x/dup1.bin"),
            b"hello world duplicate content that is long enough",
        );
        write(
            &d.join("y/dup2.bin"),
            b"hello world duplicate content that is long enough",
        );
        write(
            &d.join("z/unique.bin"),
            b"something else entirely here, also long enough!!",
        );
        let groups = find_duplicates(&d, 1);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[0].wasted, groups[0].size);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hard_links_are_not_duplicates() {
        let d = tmpdir("hardlink");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write(
            &a,
            b"identical bytes shared through a hard link, long enough",
        );
        if std::fs::hard_link(&a, &b).is_err() {
            std::fs::remove_dir_all(&d).ok();
            return; // filesystem doesn't support hard links; skip
        }
        let groups = find_duplicates(&d, 1);
        assert!(
            groups.is_empty(),
            "hard links must not be reported as duplicates, got {groups:?}"
        );
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
