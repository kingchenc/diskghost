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

use crate::{build_globset, is_excluded, on_disk_size, FileEntry, Options, Progress};

/// What one directory contains.
pub(crate) struct Batch {
    pub(crate) dir: PathBuf,
    pub(crate) files: Vec<FileEntry>,
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
pub(crate) fn walk(root: &Path, opts: &Options, progress: &Progress, mut sink: impl FnMut(Batch)) {
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
}

fn visit<'s, 'c: 's>(s: &rayon::Scope<'s>, ctx: &'c Ctx<'c>, dir: PathBuf, depth: usize) {
    if ctx.progress.cancelled() {
        return;
    }
    let mut b = Batch {
        dir,
        files: Vec::new(),
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
    for (i, entry) in entries.enumerate() {
        if i % 1024 == 1023 && ctx.progress.cancelled() {
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
            if descend {
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
            let size = on_disk_size(&meta, &path);
            bytes += size;
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
