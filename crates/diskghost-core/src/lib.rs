//! diskghost-core — fast parallel disk scanning and duplicate detection.
//!
//! The engine behind Diskghost:
//!   * [`scan`] / [`scan_with`] — total size, biggest sub-folders and files.
//!   * [`find_duplicates`] / [`find_duplicates_with`] — byte-identical files,
//!     found cheaply and correctly (group by size, collapse hard links, pre-hash
//!     the first block, then full-hash survivors with BLAKE3, all in parallel).
//!   * [`reclaim`] — act on duplicates: delete, send to trash, or replace with a
//!     hard link (dry-run by default at the call site).
//!
//! [`Options`] controls the walk: exclude globs, max depth, follow-symlinks.
//! Everything is `serde`-serialisable so a CLI, a GUI or an agent can consume it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use rayon::prelude::*;
use serde::Serialize;

/// Options controlling how the filesystem is walked (shared by scan + dupes).
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Maximum depth below the root (`None` = unlimited; `Some(1)` = root's children).
    pub max_depth: Option<usize>,
    /// Follow symbolic links / junctions (default `false` — avoids loops + surprises).
    pub follow_symlinks: bool,
    /// Glob patterns; a path is skipped if the glob matches the whole path, any
    /// path component, or the file name (so `node_modules` and `*.tmp` both work).
    pub exclude: Vec<String>,
}

/// Live progress + cancellation shared with a running scan / duplicate search.
/// Clone it (the inner counters are shared) to observe or cancel from another
/// thread while the operation runs.
#[derive(Clone, Default, Debug)]
pub struct Progress {
    files: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

impl Progress {
    /// Files seen so far.
    pub fn files(&self) -> u64 {
        self.files.load(Ordering::Relaxed)
    }
    /// Bytes seen so far.
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }
    /// Ask the running operation to stop as soon as possible.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
    /// Whether cancellation has been requested.
    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

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

/// How to reclaim the space taken by redundant duplicate copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimAction {
    /// Permanently delete the redundant copies.
    Delete,
    /// Move the redundant copies to the OS trash / recycle bin.
    Trash,
    /// Delete the copy and recreate it as a hard link to the kept file.
    Hardlink,
}

/// Outcome of a [`reclaim`] call.
#[derive(Debug, Serialize)]
pub struct ReclaimReport {
    pub removed: usize,
    pub reclaimed: u64,
    pub errors: Vec<String>,
    pub dry_run: bool,
}

/// First N bytes hashed as a cheap pre-filter before a full-file hash.
const PREFIX_LEN: usize = 4096;

struct Walk {
    files: Vec<FileEntry>,
    dirs: u64,
    skipped: u64,
}

fn build_globset(patterns: &[String]) -> globset::GlobSet {
    let mut builder = globset::GlobSetBuilder::new();
    for p in patterns {
        if let Ok(g) = globset::Glob::new(p) {
            builder.add(g);
        }
    }
    builder
        .build()
        .unwrap_or_else(|_| globset::GlobSet::empty())
}

/// Return the exclude patterns that aren't valid globs, so a caller can warn
/// instead of silently ignoring a typo like `[bad`.
pub fn validate_globs(patterns: &[String]) -> Vec<String> {
    patterns
        .iter()
        .filter(|p| globset::Glob::new(p).is_err())
        .cloned()
        .collect()
}

/// True if `path` should be excluded: the glob matches the whole path, any
/// component (e.g. a `node_modules` dir), or the file name (e.g. `*.tmp`).
fn is_excluded(glob: &globset::GlobSet, path: &Path) -> bool {
    if glob.is_empty() {
        return false;
    }
    if glob.is_match(path) {
        return true;
    }
    path.components().any(|c| glob.is_match(c.as_os_str()))
}

/// Walk `root` recursively in a single pass: collect files (with size), count
/// directories, and count unreadable entries, honouring [`Options`].
fn walk(root: &Path, opts: &Options, progress: &Progress) -> Walk {
    let glob = Arc::new(build_globset(&opts.exclude));

    let mut wd = jwalk::WalkDir::new(root)
        .skip_hidden(false)
        .follow_links(opts.follow_symlinks);
    if let Some(d) = opts.max_depth {
        wd = wd.max_depth(d);
    }
    if !glob.is_empty() {
        // Prune excluded entries *before* descending, so an excluded directory
        // (e.g. node_modules) costs no I/O for its entire subtree.
        let glob = glob.clone();
        wd = wd.process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|res| match res {
                Ok(e) => !is_excluded(&glob, &e.path()),
                Err(_) => true,
            });
        });
    }

    let mut files = Vec::new();
    let mut dirs = 0u64;
    let mut skipped = 0u64;

    for entry in wd {
        if progress.cancelled() {
            break;
        }
        match entry {
            Ok(e) => {
                let ft = e.file_type();
                if ft.is_dir() {
                    if e.depth() > 0 {
                        dirs += 1; // don't count the root itself
                    }
                } else if ft.is_file() {
                    match e.metadata() {
                        Ok(m) => {
                            let size = m.len();
                            progress.files.fetch_add(1, Ordering::Relaxed);
                            progress.bytes.fetch_add(size, Ordering::Relaxed);
                            files.push(FileEntry {
                                path: e.path(),
                                size,
                            });
                        }
                        Err(_) => skipped += 1,
                    }
                }
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

/// Walk `root` recursively and return every file with its size (default options).
pub fn walk_files(root: &Path) -> Vec<FileEntry> {
    walk(root, &Options::default(), &Progress::default()).files
}

/// Scan `root` with default options. See [`scan_with_progress`].
pub fn scan(root: &Path, top_n: usize) -> ScanReport {
    scan_with_progress(root, top_n, &Options::default(), &Progress::default())
}

/// Scan `root`: total size, the `top_n` biggest immediate sub-folders, the bytes of
/// files directly in the root, and the `top_n` biggest files. Reports live progress
/// and can be cancelled via [`Progress`].
pub fn scan_with_progress(
    root: &Path,
    top_n: usize,
    opts: &Options,
    progress: &Progress,
) -> ScanReport {
    let Walk {
        files,
        dirs,
        skipped,
    } = walk(root, opts, progress);

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

/// Find duplicate files under `root` with default options. See [`find_duplicates_with_progress`].
pub fn find_duplicates(root: &Path, min_size: u64) -> Vec<DupGroup> {
    find_duplicates_with_progress(root, min_size, &Options::default(), &Progress::default())
}

/// Find groups of byte-identical files under `root`, ignoring files smaller than
/// `min_size` bytes and zero-byte files. Reports live progress and can be cancelled.
///
/// Cheap and correct: group by size, collapse hard links, pre-hash the first
/// [`PREFIX_LEN`] bytes, then full-hash the survivors (BLAKE3). Parallel across
/// size groups.
pub fn find_duplicates_with_progress(
    root: &Path,
    min_size: u64,
    opts: &Options,
    progress: &Progress,
) -> Vec<DupGroup> {
    let files: Vec<FileEntry> = walk(root, opts, progress)
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
            if progress.cancelled() {
                return Vec::new(); // stop hashing new groups once cancelled
            }
            let reps = dedup_hard_links(paths);
            if reps.len() < 2 {
                return Vec::new();
            }

            let mut by_prefix: HashMap<[u8; 32], Vec<PathBuf>> = HashMap::new();
            for p in reps {
                if let Ok(h) = hash_prefix(&p, PREFIX_LEN) {
                    by_prefix.entry(h).or_default().push(p);
                }
            }

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
                    let mut group = group;
                    group.sort(); // stable order so "keep the first" is deterministic
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

/// Reclaim the space of redundant duplicate copies: keep `keep`, act on every
/// path in `remove` (each assumed to be `size` bytes). When `dry_run` is true no
/// filesystem changes are made — the report shows what *would* happen.
pub fn reclaim(
    keep: &Path,
    remove: &[PathBuf],
    size: u64,
    action: ReclaimAction,
    dry_run: bool,
) -> ReclaimReport {
    let mut removed = 0usize;
    let mut reclaimed = 0u64;
    let mut errors = Vec::new();

    for p in remove {
        if p == keep {
            continue;
        }
        if dry_run {
            removed += 1;
            reclaimed = reclaimed.saturating_add(size);
            continue;
        }
        let result: Result<(), String> = match action {
            ReclaimAction::Delete => std::fs::remove_file(p).map_err(|e| e.to_string()),
            ReclaimAction::Trash => trash::delete(p).map_err(|e| e.to_string()),
            ReclaimAction::Hardlink => {
                // Safe order: hard-link to a temp name, then atomically replace `p`.
                // If linking fails (e.g. different volume), `p` is left untouched —
                // no data-loss window like a remove-then-link would have.
                let mut tmp = p.clone().into_os_string();
                tmp.push(".dghtmp");
                let tmp = PathBuf::from(tmp);
                std::fs::hard_link(keep, &tmp)
                    .and_then(|_| std::fs::rename(&tmp, p))
                    .map_err(|e| {
                        let _ = std::fs::remove_file(&tmp);
                        e.to_string()
                    })
            }
        };
        match result {
            Ok(()) => {
                removed += 1;
                reclaimed = reclaimed.saturating_add(size);
            }
            Err(e) => errors.push(format!("{}: {e}", p.display())),
        }
    }

    ReclaimReport {
        removed,
        reclaimed,
        errors,
        dry_run,
    }
}

/// Keep one path per distinct physical file. Hard links share a file identity,
/// so they must not be counted as duplicates. [`same_file::Handle`] provides that
/// identity cross-platform. Paths whose identity can't be read are kept.
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
        assert_eq!(r.children.len(), 1);
        assert_eq!(r.total_size, 1077);
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
            return;
        }
        let groups = find_duplicates(&d, 1);
        assert!(
            groups.is_empty(),
            "hard links must not be reported as duplicates, got {groups:?}"
        );
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn exclude_and_depth_options() {
        let d = tmpdir("opts");
        write(&d.join("keep.bin"), &[0u8; 100]);
        write(&d.join("junk.tmp"), &[0u8; 1000]);
        write(&d.join("nested/deep/deeper.bin"), &[0u8; 50]);

        // Exclude *.tmp by name.
        let opts = Options {
            exclude: vec!["*.tmp".into()],
            ..Default::default()
        };
        let r = scan_with_progress(&d, 20, &opts, &Progress::default());
        assert_eq!(r.total_size, 150, "junk.tmp should be excluded");

        // Depth 1: only the root's direct children are visited.
        let shallow = Options {
            max_depth: Some(1),
            ..Default::default()
        };
        let r2 = scan_with_progress(&d, 20, &shallow, &Progress::default());
        // deeper.bin (depth 3) must not be counted.
        assert!(!r2.top_files.iter().any(|f| f.path.ends_with("deeper.bin")));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn reclaim_dry_run_then_delete() {
        let d = tmpdir("reclaim");
        let keep = d.join("keep.bin");
        let dup1 = d.join("dup1.bin");
        let dup2 = d.join("dup2.bin");
        let body = b"reclaimable duplicate content, definitely long enough here";
        write(&keep, body);
        write(&dup1, body);
        write(&dup2, body);
        let size = body.len() as u64;
        let remove = vec![dup1.clone(), dup2.clone()];

        // Dry run: nothing deleted, but reported.
        let dry = reclaim(&keep, &remove, size, ReclaimAction::Delete, true);
        assert_eq!(dry.removed, 2);
        assert_eq!(dry.reclaimed, 2 * size);
        assert!(dry.errors.is_empty());
        assert!(dup1.exists() && dup2.exists() && keep.exists());

        // Real delete: dups gone, keep stays.
        let done = reclaim(&keep, &remove, size, ReclaimAction::Delete, false);
        assert_eq!(done.removed, 2);
        assert_eq!(done.reclaimed, 2 * size);
        assert!(done.errors.is_empty());
        assert!(!dup1.exists() && !dup2.exists() && keep.exists());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn progress_counts_and_cancel_stops() {
        let d = tmpdir("progress");
        write(&d.join("a/1.bin"), &[0u8; 10]);
        write(&d.join("a/2.bin"), &[0u8; 20]);

        let p = Progress::default();
        let r = scan_with_progress(&d, 10, &Options::default(), &p);
        assert_eq!(r.total_files, 2);
        assert_eq!(p.files(), 2);
        assert_eq!(p.bytes(), 30);

        // Pre-cancelled: the walk stops immediately and sees nothing.
        let c = Progress::default();
        c.cancel();
        let r2 = scan_with_progress(&d, 10, &Options::default(), &c);
        assert_eq!(r2.total_files, 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn reclaim_hardlink_dedups_without_loss() {
        let d = tmpdir("reclaimhl");
        let keep = d.join("keep.bin");
        let dup = d.join("dup.bin");
        let body = b"hardlink reclaim content, long enough to be a real file here";
        write(&keep, body);
        write(&dup, body);
        let rep = reclaim(
            &keep,
            std::slice::from_ref(&dup),
            body.len() as u64,
            ReclaimAction::Hardlink,
            false,
        );
        // On filesystems without hard links this errors — but never loses data.
        if rep.errors.is_empty() {
            assert_eq!(rep.removed, 1);
            assert!(keep.exists() && dup.exists());
            assert!(
                find_duplicates(&d, 1).is_empty(),
                "should be hard-linked now"
            );
        } else {
            assert!(dup.exists(), "dup must survive a failed hardlink reclaim");
        }
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn reclaim_skips_keep_listed_in_remove() {
        let d = tmpdir("reclaimkeep");
        let keep = d.join("k.bin");
        let dup = d.join("d.bin");
        write(&keep, b"content long enough to matter for this test case");
        write(&dup, b"content long enough to matter for this test case");
        let rep = reclaim(
            &keep,
            &[keep.clone(), dup.clone()],
            40,
            ReclaimAction::Delete,
            false,
        );
        assert_eq!(rep.removed, 1);
        assert!(keep.exists());
        assert!(!dup.exists());
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
