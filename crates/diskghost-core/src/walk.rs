//! The parallel directory walk behind every scan, duplicate search and delete.
//!
//! Each directory is listed by one task on a thread pool owned by this walk
//! (so concurrent walks never starve each other), and every subdirectory found
//! becomes a new task. A file's size comes from the directory listing itself:
//! on Windows the `FindNextFile` data already carries size and attributes, so
//! no file is ever opened — that per-file open used to be most of a scan's
//! cost. The listing of one directory is handed to the caller as a [`Batch`]
//! while the walk goes on, so building a tree overlaps with the I/O.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{mpsc, Mutex};

use crate::{
    build_globset, is_excluded, is_mount_point, on_disk_size, FileEntry, Options, Progress,
};

/// What one directory contains.
pub(crate) struct Batch {
    pub(crate) dir: PathBuf,
    pub(crate) files: Vec<FileEntry>,
    /// Last-modified time of each of `files` (nanoseconds since the Unix
    /// epoch, 0 if unknown) — from the same listing, so it costs nothing.
    pub(crate) modified: Vec<u64>,
    /// Subdirectories (also those not descended into because of `max_depth`).
    pub(crate) subdirs: Vec<PathBuf>,
    /// Entries neither counted nor followed: symlinks and junctions when links
    /// are not followed, sockets, devices. A delete still has to remove them.
    pub(crate) others: Vec<PathBuf>,
    /// Entries (or this directory itself) that could not be read.
    pub(crate) skipped: u64,
}

/// Threads for a walk with `Options::threads == None`: directory listing is
/// latency-bound, so twice the cores keeps fast SSDs busy.
pub(crate) fn default_threads() -> usize {
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    (cores * 2).clamp(4, 64)
}

struct Ctx<'a> {
    root: &'a Path,
    glob: globset::GlobSet,
    max_depth: Option<usize>,
    follow: bool,
    progress: &'a Progress,
    tx: mpsc::Sender<Batch>,
    /// Canonical directories already entered — only kept when following
    /// links, where a link back up the tree would otherwise loop forever.
    seen: Option<Mutex<HashSet<PathBuf>>>,
    /// Inodes already counted — hard links and followed symlinks reach the
    /// same file through several names; it is charged only once.
    files_seen: Mutex<HashSet<(u64, u64)>>,
    /// Stay on the filesystem of `root`: mounted children are listed but
    /// never descended into (`du -x` semantics).
    same_file_system: bool,
}

impl Ctx<'_> {
    fn first_visit(&self, dir: &Path) -> bool {
        let Some(seen) = &self.seen else { return true };
        let key = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        seen.lock().map(|mut s| s.insert(key)).unwrap_or(true)
    }
}

/// Walk `root`, calling `sink` on the caller's thread with each directory's
/// [`Batch`] as soon as it is listed (in no particular order). Honours
/// exclusions, max depth, link following and cancellation.
pub(crate) fn walk(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    mut sink: impl FnMut(Batch),
) -> (Engine, Option<String>) {
    walk_dyn(root, opts, progress, &mut sink)
}

/// [`walk`] with a type-erased sink, so the MFT engine can hand folders it
/// cannot read itself back to the directory walk.
pub(crate) fn walk_dyn(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    sink: &mut dyn FnMut(Batch),
) -> (Engine, Option<String>) {
    // Why the MFT was asked for but not used.
    let mut fallback = None;
    if opts.mft {
        #[cfg(windows)]
        if opts.follow_symlinks {
            fallback = Some("links are followed only by the directory walk".to_string());
        } else {
            match crate::mft::walk(root, opts, progress, sink) {
                Ok(()) => return (Engine::Mft, None),
                Err(e) => fallback = Some(e.to_string()),
            }
        }
        #[cfg(not(windows))]
        {
            fallback = Some("the MFT engine exists on Windows only".to_string());
        }
    }
    let threads = opts.threads.unwrap_or_else(default_threads).max(1);
    let (tx, rx) = mpsc::channel();
    let ctx = Ctx {
        root,
        glob: build_globset(&opts.exclude),
        max_depth: opts.max_depth,
        follow: opts.follow_symlinks,
        progress,
        tx,
        seen: opts.follow_symlinks.then(|| Mutex::new(HashSet::new())),
        files_seen: Mutex::new(HashSet::new()),
        same_file_system: opts.same_file_system,
    };
    if ctx.follow {
        ctx.first_visit(root);
    }
    std::thread::scope(|ts| {
        ts.spawn(move || {
            let start = root.to_path_buf();
            match rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .thread_name(|i| format!("diskghost-walk-{i}"))
                .build()
            {
                Ok(pool) => pool.scope(|s| visit(s, &ctx, start, 0)),
                Err(_) => rayon::scope(|s| visit(s, &ctx, start, 0)),
            }
            // `ctx` (and with it the sender) drops here, ending the receiver loop.
        });
        for batch in rx {
            sink(batch);
        }
    });
    (Engine::Walk, fallback)
}

/// How a tree was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// Directory by directory.
    Walk,
    /// From the NTFS Master File Table (Windows only).
    #[cfg_attr(not(windows), allow(dead_code))]
    Mft,
}

fn visit<'s, 'c: 's>(s: &rayon::Scope<'s>, ctx: &'c Ctx<'c>, dir: PathBuf, depth: usize) {
    if ctx.progress.cancelled() {
        return;
    }
    let mut b = Batch {
        dir,
        files: Vec::new(),
        modified: Vec::new(),
        subdirs: Vec::new(),
        others: Vec::new(),
        skipped: 0,
    };
    let entries = match std::fs::read_dir(&b.dir) {
        Ok(rd) => rd,
        Err(_) => {
            b.skipped = 1;
            let _ = ctx.tx.send(b);
            return;
        }
    };
    // Entries of this directory sit at `depth + 1`; descend only while the
    // next level is still within `max_depth` (the root is depth 0).
    let child_depth = depth + 1;
    let descend = ctx.max_depth.map_or(true, |m| child_depth < m);
    let mut bytes = 0u64;
    for entry in entries {
        if ctx.progress.cancelled() {
            break;
        }
        let Ok(entry) = entry else {
            b.skipped += 1;
            continue;
        };
        let path = entry.path();
        if !ctx.glob.is_empty() && is_excluded(&ctx.glob, ctx.root, &path) {
            continue;
        }
        let Ok(mut ft) = entry.file_type() else {
            b.skipped += 1;
            continue;
        };
        // Metadata straight from the listing (free on Windows), or of the
        // link target when following a link.
        let mut meta = None;
        if ft.is_symlink() {
            if !ctx.follow {
                b.others.push(path);
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(m) => {
                    ft = m.file_type();
                    meta = Some(m);
                }
                Err(_) => {
                    b.skipped += 1;
                    continue;
                }
            }
        }
        if ft.is_dir() {
            if ctx.follow && !ctx.first_visit(&path) {
                continue;
            }
            // Another filesystem mounted below the root stays out of bounds:
            // listed like a depth-limited folder (shown as not measured) but
            // never opened — network and FUSE mounts would otherwise walk
            // remote trees and stall the scan on every read.
            let in_bounds = !ctx.same_file_system || !is_mount_point(&path);
            if descend && in_bounds {
                let sub = path.clone();
                s.spawn(move |s| visit(s, ctx, sub, child_depth));
            }
            b.subdirs.push(path);
        } else if ft.is_file() {
            let meta = match meta.map_or_else(|| entry.metadata(), Ok) {
                Ok(m) => m,
                Err(_) => {
                    b.skipped += 1;
                    continue;
                }
            };
            if let Some(id) = file_key(&meta, ctx.follow) {
                let first = ctx
                    .files_seen
                    .lock()
                    .map(|mut s| s.insert(id))
                    .unwrap_or(true);
                if !first {
                    continue;
                }
            }
            let size = on_disk_size(&meta, &path);
            bytes += size;
            b.modified.push(modified_ns(&meta));
            b.files.push(FileEntry {
                path,
                size,
                logical_size: meta.len(),
            });
        } else {
            b.others.push(path);
        }
    }
    ctx.progress
        .files
        .fetch_add(b.files.len() as u64, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(bytes, Ordering::Relaxed);
    let _ = ctx.tx.send(b);
}

/// Device+inode identity of a file that may be reached again — through a
/// second name (hard link, `nlink > 1`) or, when links are followed, a second
/// path (symlink) — so it is charged once. `None` for files that can only be
/// met once: keeps the shared set (and its lock) off the common path.
#[cfg(unix)]
fn file_key(meta: &std::fs::Metadata, follow: bool) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    (follow || meta.nlink() > 1).then(|| (meta.dev(), meta.ino()))
}

/// `file_key` elsewhere: no stable file identity in `std` (Windows'
/// `file_index` is unstable, `windows_by_handle`) — no dedup.
#[cfg(not(unix))]
fn file_key(_meta: &std::fs::Metadata, _follow: bool) -> Option<(u64, u64)> {
    None
}

/// Modification time in nanoseconds since the Unix epoch (0 if unknown).
pub(crate) fn modified_ns(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

#[cfg(test)]
// Most cases need unix links/inodes; elsewhere the shared helpers sit unused.
#[cfg_attr(not(unix), allow(unused_imports, dead_code))]
mod tests {
    use super::*;
    use crate::{scan_with_progress, Options, Progress};
    use std::io::Write;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-walk-{}-{label}", std::process::id()));
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

    fn allocated(path: &Path) -> u64 {
        let m = std::fs::metadata(path).unwrap();
        on_disk_size(&m, path)
    }

    /// Two names for one inode must add their bytes once, not twice — a
    /// backup tree made of `cp -al` / `--link-dest` hard links otherwise
    /// reports a multiple of the space it actually occupies.
    #[cfg(unix)]
    #[test]
    fn hardlinked_entries_share_one_charge() {
        let d = tmpdir("hardlink");
        let a = d.join("a.bin");
        write(&a, &[7u8; 100_000]);
        std::fs::hard_link(&a, d.join("b.bin")).unwrap();
        write(&d.join("c.bin"), &[3u8; 10_000]);

        let r = scan_with_progress(&d, 10, &Options::default(), &Progress::default());
        assert_eq!(r.total_files, 2, "a hard link is not a second file");
        assert_eq!(r.total_size, allocated(&a) + allocated(&d.join("c.bin")));
    }

    /// With links followed, a symlink to a file inside the scan reaches the
    /// same inode as the real path — it must not be charged again.
    #[cfg(unix)]
    #[test]
    fn followed_symlink_to_file_is_charged_once() {
        let d = tmpdir("symlink-file");
        let a = d.join("sub").join("a.bin");
        write(&a, &[9u8; 100_000]);
        std::os::unix::fs::symlink(&a, d.join("link")).unwrap();

        let opts = Options {
            follow_symlinks: true,
            ..Options::default()
        };
        let r = scan_with_progress(&d, 10, &opts, &Progress::default());
        assert_eq!(r.total_files, 1, "link and target are one file");
        assert_eq!(r.total_size, allocated(&a));
    }

    /// A symlink to an already-walked directory is not descended into twice
    /// (guards the existing `seen` set — regression coverage).
    #[cfg(unix)]
    #[test]
    fn followed_symlink_to_dir_is_not_rewalked() {
        let d = tmpdir("symlink-dir");
        write(&d.join("real").join("x.bin"), &[5u8; 100_000]);
        std::os::unix::fs::symlink(d.join("real"), d.join("alias")).unwrap();

        let opts = Options {
            follow_symlinks: true,
            ..Options::default()
        };
        let r = scan_with_progress(&d, 10, &opts, &Progress::default());
        assert_eq!(r.total_files, 1);
        assert_eq!(r.total_size, allocated(&d.join("real/x.bin")));
    }

    /// A plain directory on the parent filesystem is not a "drive"; scanning
    /// it separately from `/` would recount the same content.
    #[test]
    fn plain_directory_is_not_a_mount_point() {
        let d = tmpdir("not-a-mount");
        assert!(!crate::is_mount_point(&d));
        assert!(crate::is_mount_point(Path::new("/")));
    }

    /// `same_file_system` keeps a walk on the filesystem of its root: a
    /// mounted child is listed but never opened. A `/` scan must not walk a
    /// FUSE share or a second drive mounted below — live bug: a root scan
    /// descended into `gvfsd` sftp/smb mounts, counted remote filesystems,
    /// and could not be cancelled out of the blocked `read_dir`s.
    #[cfg(target_os = "linux")]
    #[test]
    fn same_file_system_does_not_descend_into_other_filesystems() {
        // Prefer a mount whose parent is an ordinary directory (a small
        // walk); otherwise any direct child of `/` will do.
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        let mut mounted: Vec<PathBuf> = mounts
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .map(PathBuf::from)
            .filter(|p| p.is_dir() && crate::is_mount_point(p))
            .collect();
        mounted.sort();
        let (root, target) = mounted
            .iter()
            .filter_map(|m| m.parent().map(|p| (p.to_path_buf(), m.clone())))
            .find(|(p, _)| p != Path::new("/") && !crate::is_mount_point(p))
            .or_else(|| {
                mounted
                    .iter()
                    .find(|m| m.parent() == Some(Path::new("/")))
                    .map(|m| (PathBuf::from("/"), m.clone()))
            })
            .expect("no mounted directory in /proc/self/mounts");

        // Depth: the target would be descended into without the bound.
        let depth = target.strip_prefix(&root).unwrap().components().count();
        let collect = |opts: &Options| {
            let visited = Mutex::new(Vec::new());
            walk(&root, opts, &Progress::default(), |b| {
                visited.lock().unwrap().push(b.dir);
            });
            visited.into_inner().unwrap()
        };

        // Unbounded: the mounted child is walked (documents that the flag is
        // opt-in, not a change of the default traversal).
        let opts = Options {
            max_depth: Some(depth + 1),
            ..Options::default()
        };
        assert!(
            collect(&opts).contains(&target),
            "unbounded walk should descend into {target:?}"
        );

        let opts = Options {
            same_file_system: true,
            ..opts
        };
        assert!(
            !collect(&opts).contains(&target),
            "bounded walk descended into foreign filesystem {target:?}"
        );
    }

    /// Real mount points do compare different from their parent — take the
    /// first non-`/`, non-bind mount from the kernel table.
    #[cfg(target_os = "linux")]
    #[test]
    fn kernel_mounts_are_mount_points() {
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        let target = mounts
            .lines()
            .filter_map(|l| l.split_whitespace().nth(1))
            .map(PathBuf::from)
            .find(|p| p != Path::new("/") && p.is_dir())
            .expect("no mounted directory in /proc/self/mounts");
        let child = target.join(format!("diskghost-not-mounted-{}", std::process::id()));
        if std::fs::create_dir(&child).is_ok() {
            assert!(!crate::is_mount_point(&child));
            let _ = std::fs::remove_dir(&child);
        }
        assert!(crate::is_mount_point(&target));
    }
}
