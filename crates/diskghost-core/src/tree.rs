//! The whole directory tree from a single walk, held in memory so a UI can
//! drill into any sub-folder — and back up — without touching the disk again.
//!
//! Nodes live in an arena (`Vec<Node>`). Each keeps only its own name: full
//! paths are rebuilt from the parent chain when a report needs one, and a path
//! is found by descending from the root through children kept sorted by name.
//! That keeps a multi-million-file tree to a fraction of the memory full paths
//! would take. A node's totals (`size`, `file_count`, `dir_count`) always
//! cover its entire subtree; the files sitting directly in a directory are kept
//! on that node so any sub-folder can produce the same [`ScanReport`] a fresh
//! scan of it would.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::snapshot::{ENTRY_DIR, ENTRY_FILE};
use crate::walk::{walk, Batch, Engine};
use crate::{
    disk_space, DirSize, ExtStat, FileEntry, FileStats, Options, Progress, ScanReport, SizeBucket,
    SIZE_BUCKET_BOUNDS,
};

/// A file sitting directly in a directory.
pub(crate) struct FileRec {
    pub(crate) name: Box<OsStr>,
    pub(crate) size: u64,
    logical_size: u64,
    /// Index into [`ScanTree::exts`].
    ext: u32,
}

pub(crate) struct Node {
    /// Own name; the root's full path is [`ScanTree::root`].
    pub(crate) name: Box<OsStr>,
    parent: Option<usize>,
    /// Bytes in this directory and everything below it.
    pub(crate) size: u64,
    /// Files in this directory and everything below it.
    pub(crate) file_count: u64,
    /// Directories below this one (not counting itself).
    dir_count: u64,
    /// Sub-directories, sorted by name once the walk is done.
    pub(crate) children: Vec<usize>,
    /// Files directly in this directory, sorted by name once the walk is done.
    pub(crate) files: Vec<FileRec>,
    /// Whether this directory's own listing was read (it was not cut off by
    /// the depth limit or a cancelled walk).
    pub(crate) listed: bool,
    /// Whether this directory and everything below it was read, so its totals
    /// are exact — set once the walk is done.
    complete: bool,
}

/// Byte order of two names — the order children and files are kept in.
pub(crate) fn cmp_name(a: &OsStr, b: &OsStr) -> Ordering {
    a.as_encoded_bytes().cmp(b.as_encoded_bytes())
}

/// An in-memory scan of a directory tree. Build it once with
/// [`ScanTree::build`], then ask for a [`ScanReport`] of the root or of any
/// sub-folder with [`ScanTree::report`] — no further I/O.
pub struct ScanTree {
    root: PathBuf,
    /// Arena; index 0 is always the root.
    pub(crate) nodes: Vec<Node>,
    /// Lower-cased file extensions, indexed by [`FileRec::ext`].
    exts: Vec<Box<str>>,
    skipped: u64,
    cancelled: bool,
    engine: Engine,
    /// Why the MFT was asked for but the directory walk ran.
    mft_fallback: Option<String>,
    disk_total: u64,
    disk_free: u64,
}

/// Puts batches from the walk into the tree as they arrive (in any order).
struct Builder {
    tree: ScanTree,
    /// Directories created but not listed yet, by path. A directory's own
    /// batch can arrive before its parent's, so any missing ancestor is
    /// created on the spot and found here later.
    pending: HashMap<PathBuf, usize>,
    ext_ids: HashMap<Box<str>, u32>,
}

impl Builder {
    fn push(&mut self, parent: usize, name: &OsStr) -> usize {
        let id = self.tree.nodes.len();
        self.tree.nodes.push(Node::new(name, Some(parent)));
        self.tree.nodes[parent].children.push(id);
        id
    }

    fn dir_id(&mut self, path: &Path) -> usize {
        if path == self.tree.root {
            return 0;
        }
        if let Some(&id) = self.pending.get(path) {
            return id;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return 0;
        };
        let parent = self.dir_id(parent);
        let id = self.push(parent, name);
        self.pending.insert(path.to_path_buf(), id);
        id
    }

    fn ext_id(&mut self, name: &OsStr) -> u32 {
        let ext = Path::new(name).extension().unwrap_or_default();
        let ext = ext.to_string_lossy();
        let lower;
        let key: &str = if ext
            .bytes()
            .any(|b| !b.is_ascii_lowercase() && !b.is_ascii_digit())
        {
            lower = ext.to_lowercase();
            &lower
        } else {
            &ext
        };
        if let Some(&id) = self.ext_ids.get(key) {
            return id;
        }
        let id = self.tree.exts.len() as u32;
        self.tree.exts.push(key.into());
        self.ext_ids.insert(key.into(), id);
        id
    }

    fn add(&mut self, batch: Batch) {
        let id = self.dir_id(&batch.dir);
        self.pending.remove(&batch.dir);
        self.tree.skipped += batch.skipped;

        // Children already here were created early, by a batch from deeper
        // down that arrived first; don't create them twice.
        let early: HashMap<Box<OsStr>, usize> = self.tree.nodes[id]
            .children
            .iter()
            .map(|&c| (self.tree.nodes[c].name.clone(), c))
            .collect();
        for sub in batch.subdirs {
            let Some(name) = sub.file_name() else {
                continue;
            };
            if early.contains_key(name) {
                continue;
            }
            let cid = self.push(id, name);
            self.pending.insert(sub, cid);
        }

        let mut files = Vec::with_capacity(batch.files.len());
        let mut size = 0u64;
        for f in batch.files {
            let Some(name) = f.path.file_name() else {
                continue;
            };
            let ext = self.ext_id(name);
            size += f.size;
            files.push(FileRec {
                name: name.into(),
                size: f.size,
                logical_size: f.logical_size,
                ext,
            });
        }
        let node = &mut self.tree.nodes[id];
        node.listed = true;
        node.size += size;
        node.file_count += files.len() as u64;
        node.files.extend(files);
    }

    fn finish(mut self, cancelled: bool) -> ScanTree {
        let tree = &mut self.tree;
        tree.cancelled = cancelled;
        // A cancelled walk may have stopped inside any listing: nothing in
        // it is exact.
        for node in &mut tree.nodes {
            node.complete = node.listed && !cancelled;
        }
        // Roll the totals up. A child is always created after its parent, so
        // walking the arena backwards visits every node before its parent.
        for id in (1..tree.nodes.len()).rev() {
            let node = &tree.nodes[id];
            let (size, files, dirs, complete, parent) = (
                node.size,
                node.file_count,
                node.dir_count + 1,
                node.complete,
                node.parent,
            );
            if let Some(p) = parent {
                let pn = &mut tree.nodes[p];
                pn.size += size;
                pn.file_count += files;
                pn.dir_count += dirs;
                pn.complete &= complete;
            }
        }
        // Sort children and files by name: lookups binary-search them, and
        // snapshots list them in this order.
        for id in 0..tree.nodes.len() {
            let mut kids = std::mem::take(&mut tree.nodes[id].children);
            kids.sort_by(|&a, &b| cmp_name(&tree.nodes[a].name, &tree.nodes[b].name));
            tree.nodes[id].children = kids;
            tree.nodes[id]
                .files
                .sort_by(|a, b| cmp_name(&a.name, &b.name));
        }
        let (disk_total, disk_free) = disk_space(&tree.root);
        tree.disk_total = disk_total;
        tree.disk_free = disk_free;
        self.tree
    }
}

#[cfg(windows)]
impl Builder {
    /// Build the tree straight from the NTFS Master File Table — no path is
    /// made except for cloud placeholders, which are sized (files) or listed
    /// (folders) the ordinary way, through the cloud filter.
    fn add_mft(
        &mut self,
        table: &crate::mft::Table,
        opts: &Options,
        progress: &Progress,
    ) -> Result<(), crate::mft::MftError> {
        use std::sync::atomic::Ordering;
        let start = table.find(&self.tree.root)?;
        let mut queue = vec![(start, 0usize, 0usize)]; // (record, node, depth)
        let mut cloud = Vec::new();
        if table.is_cloud(start) {
            // The folder itself is synced: all of it through Windows.
            queue.clear();
            cloud.push((0usize, 0usize));
        }
        while let Some((dir, id, depth)) = queue.pop() {
            if progress.cancelled() {
                break;
            }
            let child_depth = depth + 1;
            if opts.max_depth.is_some_and(|m| child_depth > m) {
                continue;
            }
            let descend = opts.max_depth.map_or(true, |m| child_depth < m);
            self.tree.nodes[id].listed = true;
            let (mut size, mut count) = (0u64, 0u64);
            for e in table.children(dir) {
                if e.is_dir {
                    let child = self.push(id, e.name);
                    if descend {
                        if e.cloud {
                            cloud.push((child, child_depth));
                        } else {
                            queue.push((e.record, child, child_depth));
                        }
                    }
                } else {
                    let mut bytes = e.size;
                    if e.cloud {
                        let path = self.tree.path_of(id).join(e.name);
                        bytes = crate::compressed_size(&path).unwrap_or(bytes);
                    }
                    let ext = self.ext_id(e.name);
                    self.tree.nodes[id].files.push(FileRec {
                        name: e.name.as_os_str().into(),
                        size: bytes,
                        logical_size: e.logical,
                        ext,
                    });
                    size += bytes;
                    count += 1;
                }
            }
            let node = &mut self.tree.nodes[id];
            node.size += size;
            node.file_count += count;
            progress.files.fetch_add(count, Ordering::Relaxed);
            progress.bytes.fetch_add(size, Ordering::Relaxed);
        }
        // Cloud folders, listed the ordinary way into this same tree.
        for (id, depth) in cloud {
            if progress.cancelled() {
                break;
            }
            let local = Options {
                mft: false,
                max_depth: opts.max_depth.map(|m| m.saturating_sub(depth)),
                ..opts.clone()
            };
            if local.max_depth == Some(0) {
                continue;
            }
            let path = self.tree.path_of(id);
            self.pending.insert(path.clone(), id);
            let _ = walk(&path, &local, progress, |b| self.add(b));
        }
        Ok(())
    }
}

/// Fill `b` from the MFT if asked and possible (no exclusions: those need
/// paths), else from the directory walk. Returns the engine used and, if the
/// MFT was asked for but not used, why.
#[cfg(windows)]
fn fill(
    b: &mut Builder,
    root: &Path,
    opts: &Options,
    progress: &Progress,
) -> (Engine, Option<String>) {
    if opts.mft && opts.exclude.is_empty() && !opts.follow_symlinks {
        let direct = crate::mft::Table::read(root, progress).and_then(|mut table| {
            b.add_mft(&table, opts, progress)?;
            table.mark("build tree");
            Ok(())
        });
        return match direct {
            Ok(()) => (Engine::Mft, None),
            Err(e) => {
                let walk_opts = Options {
                    mft: false,
                    ..opts.clone()
                };
                let (engine, _) = walk(root, &walk_opts, progress, |batch| b.add(batch));
                (engine, Some(e.to_string()))
            }
        };
    }
    walk(root, opts, progress, |batch| b.add(batch))
}

#[cfg(not(windows))]
fn fill(
    b: &mut Builder,
    root: &Path,
    opts: &Options,
    progress: &Progress,
) -> (Engine, Option<String>) {
    walk(root, opts, progress, |batch| b.add(batch))
}

impl Node {
    fn new(name: &OsStr, parent: Option<usize>) -> Node {
        Node {
            name: name.into(),
            parent,
            size: 0,
            file_count: 0,
            dir_count: 0,
            children: Vec::new(),
            files: Vec::new(),
            listed: false,
            complete: false,
        }
    }
}

impl ScanTree {
    /// Walk `root` (honouring `opts`, reporting to `progress`) and build the
    /// tree while the walk is still listing directories.
    pub fn build(root: &Path, opts: &Options, progress: &Progress) -> ScanTree {
        let mut b = Builder {
            tree: ScanTree {
                root: root.to_path_buf(),
                nodes: vec![Node::new(OsStr::new(""), None)],
                exts: Vec::new(),
                skipped: 0,
                cancelled: false,
                engine: Engine::Walk,
                mft_fallback: None,
                disk_total: 0,
                disk_free: 0,
            },
            pending: HashMap::new(),
            ext_ids: HashMap::new(),
        };
        let (engine, fallback) = fill(&mut b, root, opts, progress);
        b.tree.engine = engine;
        b.tree.mft_fallback = fallback;
        let started = std::time::Instant::now();
        let tree = b.finish(progress.cancelled());
        if std::env::var_os("DISKGHOST_TRACE").is_some() {
            eprintln!(
                "tree: finish {:.0} ms",
                started.elapsed().as_secs_f64() * 1000.0
            );
        }
        tree
    }

    /// A tree from snapshot entries: (key, kind, size) in snapshot order,
    /// where a folder comes before everything inside it. Keys are paths
    /// relative to `root` with `/` between components; kinds are the
    /// snapshot's `ENTRY_*` values.
    pub(crate) fn from_entries(root: &Path, entries: Vec<(Vec<u8>, u8, u64)>) -> ScanTree {
        let mut b = Builder {
            tree: ScanTree {
                root: root.to_path_buf(),
                nodes: vec![Node::new(OsStr::new(""), None)],
                exts: Vec::new(),
                skipped: 0,
                cancelled: false,
                engine: Engine::Walk,
                mft_fallback: None,
                disk_total: 0,
                disk_free: 0,
            },
            pending: HashMap::new(),
            ext_ids: HashMap::new(),
        };
        b.tree.nodes[0].listed = true;
        // Node ids of the folders on the path to the current entry.
        let mut stack: Vec<usize> = vec![0];
        for (key, kind, size) in entries {
            let depth = key.iter().filter(|&&c| c == b'/').count() + 1;
            let name_bytes = key.rsplit(|&c| c == b'/').next().unwrap_or(&[]);
            let name = std::ffi::OsString::from(String::from_utf8_lossy(name_bytes).into_owned());
            stack.truncate(depth);
            let Some(&parent) = stack.last() else {
                continue;
            };
            if kind != ENTRY_FILE {
                let id = b.push(parent, &name);
                b.tree.nodes[id].listed = kind == ENTRY_DIR;
                stack.push(id);
            } else {
                let ext = b.ext_id(&name);
                let node = &mut b.tree.nodes[parent];
                node.size += size;
                node.file_count += 1;
                node.files.push(FileRec {
                    name: name.into_boxed_os_str(),
                    size,
                    logical_size: size,
                    ext,
                });
            }
        }
        b.finish(false)
    }

    /// The directory this tree was built from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Node id of the directory `path`, if it is in the tree.
    fn find(&self, path: &Path) -> Option<usize> {
        let rel = path.strip_prefix(&self.root).ok()?;
        let mut id = 0;
        for c in rel.components() {
            let Component::Normal(name) = c else {
                return None;
            };
            let kids = &self.nodes[id].children;
            let pos = kids
                .binary_search_by(|&k| cmp_name(&self.nodes[k].name, name))
                .ok()?;
            id = kids[pos];
        }
        Some(id)
    }

    /// Full path of node `id`, rebuilt from its parent chain.
    pub(crate) fn path_of(&self, id: usize) -> PathBuf {
        let mut names = Vec::new();
        let mut cur = id;
        while let Some(p) = self.nodes[cur].parent {
            names.push(&*self.nodes[cur].name);
            cur = p;
        }
        let mut path = self.root.clone();
        for n in names.into_iter().rev() {
            path.push(n);
        }
        path
    }

    /// Whether `path` is a directory in this tree (the root included).
    pub fn contains(&self, path: &Path) -> bool {
        self.find(path).is_some()
    }

    /// True if the walk was cancelled; the tree then covers only what was seen.
    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// How the tree was read: `"mft"` (NTFS Master File Table) or `"walk"`.
    pub fn engine(&self) -> &'static str {
        match self.engine {
            Engine::Mft => "mft",
            Engine::Walk => "walk",
        }
    }

    /// Record how a tree rebuilt elsewhere was read — for one that came from
    /// another process (`mft`: read from the Master File Table; `fallback`:
    /// why not, if it was asked for).
    pub fn set_engine(&mut self, mft: bool, fallback: Option<String>) {
        self.engine = if mft { Engine::Mft } else { Engine::Walk };
        self.mft_fallback = fallback;
    }

    /// Flag a tree rebuilt elsewhere as the result of a cancelled walk.
    pub fn mark_cancelled(&mut self) {
        self.cancelled = true;
        for node in &mut self.nodes {
            node.complete = false;
        }
    }

    /// True if every directory was read: no depth limit cut the walk short
    /// and it was not cancelled. Only then are the totals exact — and only
    /// then is the tree worth a snapshot to compare against.
    pub fn complete(&self) -> bool {
        self.nodes[0].complete
    }

    /// Why the MFT engine was requested but not used (e.g. no administrator
    /// rights), if so.
    pub fn mft_fallback(&self) -> Option<&str> {
        self.mft_fallback.as_deref()
    }

    /// Re-query the filesystem's total/free bytes (e.g. after deleting files).
    pub fn refresh_disk_space(&mut self) {
        let (total, free) = disk_space(&self.root);
        self.disk_total = total;
        self.disk_free = free;
    }

    /// Report for the root: the same shape a fresh [`crate::scan`] returns.
    pub fn root_report(&self, top_n: usize) -> ScanReport {
        self.report_of(0, top_n)
    }

    /// Report for `path` — the root or any directory below it — as if that
    /// directory had been scanned on its own. `None` if it is not in the tree.
    pub fn report(&self, path: &Path, top_n: usize) -> Option<ScanReport> {
        Some(self.report_of(self.find(path)?, top_n))
    }

    /// The `top_n` biggest immediate sub-folders of `path`, largest first —
    /// the `children` of its report without the rest. `None` if `path` is not
    /// in the tree.
    pub fn children(&self, path: &Path, top_n: usize) -> Option<Vec<DirSize>> {
        let id = self.find(path)?;
        Some(self.children_of(id, &self.path_of(id), top_n))
    }

    fn children_of(&self, id: usize, base: &Path, top_n: usize) -> Vec<DirSize> {
        let mut kids: Vec<usize> = self.nodes[id].children.clone();
        kids.sort_by_key(|&k| Reverse(self.nodes[k].size));
        kids.iter()
            .take(top_n)
            .map(|&k| {
                let n = &self.nodes[k];
                DirSize {
                    path: base.join(&*n.name),
                    size: n.size,
                    file_count: n.file_count,
                    complete: n.complete,
                }
            })
            .collect()
    }

    /// Every node id in the subtree of `id`, `id` included.
    fn subtree(&self, id: usize) -> impl Iterator<Item = usize> + '_ {
        let mut stack = vec![id];
        std::iter::from_fn(move || {
            let i = stack.pop()?;
            stack.extend(self.nodes[i].children.iter().copied());
            Some(i)
        })
    }

    /// Bytes per file extension and per size range for every file at or
    /// below `path`. `None` if `path` is not in the tree.
    pub fn file_stats(&self, path: &Path) -> Option<FileStats> {
        let id = self.find(path)?;
        let mut by_ext = vec![(0u64, 0u64); self.exts.len()];
        let mut buckets: Vec<SizeBucket> = SIZE_BUCKET_BOUNDS
            .iter()
            .map(|&b| Some(b))
            .chain(std::iter::once(None))
            .map(|upper| SizeBucket {
                upper,
                size: 0,
                count: 0,
            })
            .collect();
        for i in self.subtree(id) {
            for f in &self.nodes[i].files {
                let e = &mut by_ext[f.ext as usize];
                e.0 += f.size;
                e.1 += 1;
                let b = SIZE_BUCKET_BOUNDS.partition_point(|&bound| bound <= f.size);
                buckets[b].size += f.size;
                buckets[b].count += 1;
            }
        }
        let mut extensions: Vec<ExtStat> = by_ext
            .into_iter()
            .enumerate()
            .filter(|(_, (_, count))| *count > 0)
            .map(|(i, (size, count))| ExtStat {
                ext: self.exts[i].to_string(),
                size,
                count,
            })
            .collect();
        extensions.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.ext.cmp(&b.ext)));
        Some(FileStats {
            extensions,
            size_buckets: buckets,
        })
    }

    fn report_of(&self, id: usize, top_n: usize) -> ScanReport {
        let node = &self.nodes[id];
        let base = self.path_of(id);
        let children = self.children_of(id, &base, top_n);

        let root_files_size = node.files.iter().map(|f| f.size).sum();
        let root_files_count = node.files.len() as u64;

        // Biggest files anywhere below `id`: a min-heap holding the best
        // `top_n` seen so far, so the subtree is scanned once without
        // collecting it.
        let mut heap: BinaryHeap<Reverse<(u64, usize, usize)>> = BinaryHeap::new();
        if top_n > 0 {
            for i in self.subtree(id) {
                for (j, f) in self.nodes[i].files.iter().enumerate() {
                    if heap.len() < top_n {
                        heap.push(Reverse((f.size, i, j)));
                    } else if heap.peek().is_some_and(|Reverse(min)| f.size > min.0) {
                        heap.pop();
                        heap.push(Reverse((f.size, i, j)));
                    }
                }
            }
        }
        let top: Vec<(u64, usize, usize)> = heap.into_iter().map(|Reverse(x)| x).collect();
        let mut top_files: Vec<FileEntry> = top
            .into_iter()
            .map(|(_, i, j)| {
                let f = &self.nodes[i].files[j];
                FileEntry {
                    path: self.path_of(i).join(&*f.name),
                    size: f.size,
                    logical_size: f.logical_size,
                }
            })
            .collect();
        // Biggest first; equal sizes by path, so the list never reshuffles.
        top_files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));

        ScanReport {
            root: base,
            total_size: node.size,
            total_files: node.file_count,
            total_dirs: node.dir_count,
            skipped: self.skipped,
            children,
            root_files_size,
            root_files_count,
            top_files,
            disk_total: self.disk_total,
            disk_free: self.disk_free,
            cancelled: self.cancelled,
            complete: node.complete,
        }
    }

    /// Every directory below the root with its subtree totals, sorted by path.
    /// The root itself is not included.
    pub fn dir_sizes(&self) -> Vec<DirSize> {
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut stack: Vec<(usize, PathBuf)> = self.nodes[0]
            .children
            .iter()
            .rev()
            .map(|&c| (c, self.root.join(&*self.nodes[c].name)))
            .collect();
        while let Some((id, path)) = stack.pop() {
            let n = &self.nodes[id];
            for &c in n.children.iter().rev() {
                stack.push((c, path.join(&*self.nodes[c].name)));
            }
            out.push(DirSize {
                path,
                size: n.size,
                file_count: n.file_count,
                complete: n.complete,
            });
        }
        out
    }

    /// Drop a directory (with everything below it) or a single file from the
    /// tree, adjusting every ancestor's totals — what a UI does after deleting
    /// the path on disk, instead of re-walking. The root cannot be removed.
    /// Returns `false` if the path is not in the tree.
    pub fn remove(&mut self, path: &Path) -> bool {
        if let Some(id) = self.find(path) {
            let Some(parent) = self.nodes[id].parent else {
                return false; // the root
            };
            let n = &self.nodes[id];
            let (size, files, dirs) = (n.size, n.file_count, n.dir_count + 1);
            self.nodes[parent].children.retain(|&c| c != id);
            self.subtract(parent, size, files, dirs);
            self.recheck_complete(parent);
            // The subtree is unreachable now; give its memory back.
            let mut stack = vec![id];
            while let Some(i) = stack.pop() {
                self.nodes[i].files = Vec::new();
                stack.extend(std::mem::take(&mut self.nodes[i].children));
            }
            return true;
        }
        let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
            return false;
        };
        let Some(pid) = self.find(parent) else {
            return false;
        };
        let files = &mut self.nodes[pid].files;
        let Ok(pos) = files.binary_search_by(|f| cmp_name(&f.name, name)) else {
            return false;
        };
        let file = files.remove(pos);
        self.subtract(pid, file.size, 1, 0);
        true
    }

    /// Recompute `complete` for `id` and its ancestors after a child went
    /// away: removing the only unread folder makes the rest exact again.
    fn recheck_complete(&mut self, mut id: usize) {
        loop {
            let n = &self.nodes[id];
            let complete =
                n.listed && !self.cancelled && n.children.iter().all(|&c| self.nodes[c].complete);
            self.nodes[id].complete = complete;
            match self.nodes[id].parent {
                Some(p) => id = p,
                None => break,
            }
        }
    }

    /// Subtract totals from `id` and every ancestor up to the root.
    fn subtract(&mut self, mut id: usize, size: u64, files: u64, dirs: u64) {
        loop {
            let n = &mut self.nodes[id];
            n.size = n.size.saturating_sub(size);
            n.file_count = n.file_count.saturating_sub(files);
            n.dir_count = n.dir_count.saturating_sub(dirs);
            match n.parent {
                Some(p) => id = p,
                None => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-tree-{}-{label}", std::process::id()));
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

    fn logical_total(dir: &Path) -> u64 {
        crate::walk_files(dir).iter().map(|f| f.logical_size).sum()
    }

    #[test]
    fn drill_in_matches_a_fresh_scan() {
        // Sizes are spread well beyond one filesystem block: on-disk sizes
        // round up to the block (4 KB on ext4), so small files would tie.
        let d = tmpdir("drill");
        write(&d.join("a/1.bin"), &[0u8; 100_000]);
        write(&d.join("a/x/2.bin"), &[0u8; 40_000]);
        write(&d.join("a/x/y/3.bin"), &[0u8; 20_000]);
        write(&d.join("b/4.bin"), &[0u8; 10_000]);
        write(&d.join("top.bin"), &[0u8; 100]);
        std::fs::create_dir_all(d.join("empty")).unwrap();

        let tree = ScanTree::build(&d, &Options::default(), &Progress::default());
        assert_eq!(tree.root(), d.as_path());
        assert!(tree.contains(&d.join("a/x/y")));
        assert!(tree.contains(&d.join("empty")));
        assert!(!tree.contains(&d.join("nope")));
        assert!(!tree.cancelled());

        let root = tree.root_report(10);
        let fresh = crate::scan(&d, 10);
        assert_eq!(root.total_size, fresh.total_size);
        assert_eq!(root.total_files, 5);
        assert_eq!(root.total_dirs, 5); // a, a/x, a/x/y, b, empty
        assert_eq!(root.root_files_count, 1);
        assert_eq!(root.children[0].path, d.join("a"));
        assert_eq!(root.top_files[0].path, d.join("a/1.bin"));
        assert_eq!(root.children.len(), 3);
        assert_eq!(root.children[2].path, d.join("empty"));
        assert_eq!(root.children[2].size, 0);

        // A sub-folder from the tree equals a fresh scan of just that folder.
        let sub = tree.report(&d.join("a/x"), 10).unwrap();
        let fresh_sub = crate::scan(&d.join("a/x"), 10);
        assert_eq!(sub.root, fresh_sub.root);
        assert_eq!(sub.total_size, fresh_sub.total_size);
        assert_eq!(sub.total_files, 2);
        assert_eq!(sub.total_dirs, 1);
        assert_eq!(sub.root_files_count, 1);
        assert_eq!(sub.children.len(), 1);
        assert_eq!(sub.children[0].path, d.join("a/x/y"));
        assert_eq!(sub.top_files.len(), 2);
        assert_eq!(sub.top_files[0].path, d.join("a/x/2.bin"));

        assert!(tree.report(&d.join("nope"), 10).is_none());

        // top_n bounds both lists; 0 yields none.
        let one = tree.root_report(1);
        assert_eq!(one.children.len(), 1);
        assert_eq!(one.top_files.len(), 1);
        let none = tree.root_report(0);
        assert!(none.children.is_empty() && none.top_files.is_empty());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn dir_sizes_lists_every_directory() {
        let d = tmpdir("dirsizes");
        write(&d.join("a/1.bin"), &[0u8; 10]);
        write(&d.join("a/b/2.bin"), &[0u8; 20]);
        let tree = ScanTree::build(&d, &Options::default(), &Progress::default());
        let dirs = tree.dir_sizes();
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].path, d.join("a"));
        assert_eq!(dirs[1].path, d.join("a/b"));
        assert_eq!(dirs[0].file_count, 2);
        assert_eq!(dirs[1].file_count, 1);
        assert!(dirs[0].size >= dirs[1].size);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_updates_every_ancestor() {
        let d = tmpdir("remove");
        write(&d.join("a/1.bin"), &[0u8; 1000]);
        write(&d.join("a/x/2.bin"), &[0u8; 500]);
        write(&d.join("b/3.bin"), &[0u8; 200]);
        let mut tree = ScanTree::build(&d, &Options::default(), &Progress::default());
        let before = tree.root_report(10);
        let a_x = tree.report(&d.join("a/x"), 10).unwrap();

        // A directory: its subtree leaves the tree and the totals shrink.
        assert!(tree.remove(&d.join("a/x")));
        assert!(!tree.contains(&d.join("a/x")));
        let after = tree.root_report(10);
        assert_eq!(after.total_size, before.total_size - a_x.total_size);
        assert_eq!(after.total_files, before.total_files - 1);
        assert_eq!(after.total_dirs, before.total_dirs - 1);
        let a = tree.report(&d.join("a"), 10).unwrap();
        assert_eq!(a.total_files, 1);
        assert!(a.children.is_empty());

        // A single file: only its size and count go.
        let one = tree.report(&d.join("b"), 10).unwrap().top_files[0].clone();
        assert!(tree.remove(&one.path));
        let b = tree.report(&d.join("b"), 10).unwrap();
        assert_eq!(b.total_files, 0);
        assert_eq!(b.total_size, 0);
        assert_eq!(tree.root_report(10).total_size, after.total_size - one.size);

        // Unknown paths and the root are refused.
        assert!(!tree.remove(&d.join("a/x")));
        assert!(!tree.remove(&d.join("b/missing.bin")));
        assert!(!tree.remove(&d));
        assert!(!tree.remove(Path::new("/")));

        // Disk space can be refreshed in place.
        tree.refresh_disk_space();
        assert_eq!(tree.root_report(1).disk_total, before.disk_total);

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn children_and_file_stats_cover_the_subtree() {
        let d = tmpdir("stats");
        write(&d.join("a/movie.MKV"), &[0u8; 2_000_000]);
        write(&d.join("a/x/clip.mkv"), &[0u8; 100_000]);
        write(&d.join("b/notes.txt"), &[0u8; 100_000]);
        write(&d.join("b/README"), &[0u8; 100_000]);
        let tree = ScanTree::build(&d, &Options::default(), &Progress::default());

        let kids = tree.children(&d, 10).unwrap();
        assert_eq!(kids, tree.root_report(10).children);
        assert_eq!(tree.children(&d, 1).unwrap().len(), 1);
        assert!(tree.children(&d.join("nope"), 10).is_none());

        let s = tree.file_stats(&d).unwrap();
        let total = tree.root_report(1).total_size;
        // Extensions are lower-cased; files without one share "".
        let exts: Vec<(&str, u64)> = s
            .extensions
            .iter()
            .map(|e| (e.ext.as_str(), e.count))
            .collect();
        assert_eq!(exts[0], ("mkv", 2));
        assert!(exts.contains(&("txt", 1)));
        assert!(exts.contains(&("", 1)));
        assert_eq!(s.extensions.iter().map(|e| e.size).sum::<u64>(), total);
        // 100 KB files land in 64 KB..1 MB, the 2 MB one in 1..16 MB.
        assert_eq!(s.size_buckets.len(), SIZE_BUCKET_BOUNDS.len() + 1);
        assert_eq!(s.size_buckets[2].count, 3);
        assert_eq!(s.size_buckets[3].count, 1);
        assert_eq!(s.size_buckets.last().unwrap().upper, None);
        assert_eq!(s.size_buckets.iter().map(|b| b.size).sum::<u64>(), total);

        // A sub-folder only counts its own subtree.
        let a = tree.file_stats(&d.join("a")).unwrap();
        assert_eq!(a.extensions.len(), 1);
        assert_eq!(a.extensions[0].count, 2);
        assert!(tree.file_stats(&d.join("nope")).is_none());

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn depth_limit_marks_what_was_not_read() {
        // root (depth 0) > a, b, c (1) > a/x (2) > a/x/y (3)
        let d = tmpdir("depth");
        write(&d.join("top.bin"), &[0u8; 100_000]);
        write(&d.join("a/1.bin"), &[0u8; 100_000]);
        write(&d.join("a/x/2.bin"), &[0u8; 40_000]);
        write(&d.join("a/x/y/3.bin"), &[0u8; 20_000]);
        write(&d.join("b/4.bin"), &[0u8; 10_000]);
        std::fs::create_dir_all(d.join("c")).unwrap();
        let build = |depth| {
            let opts = Options {
                max_depth: depth,
                ..Default::default()
            };
            ScanTree::build(&d, &opts, &Progress::default())
        };
        let by_name = |r: &ScanReport, name: &str| {
            r.children
                .iter()
                .find(|c| c.path.file_name() == Some(OsStr::new(name)))
                .cloned()
                .unwrap()
        };

        let full = build(None);
        assert!(full.complete());
        let r = full.root_report(10);
        assert!(r.complete && r.children.iter().all(|c| c.complete));

        // Depth 1: the root is listed, its folders are not.
        let one = build(Some(1));
        assert!(!one.complete());
        let r = one.root_report(10);
        assert!(!r.complete);
        assert_eq!(r.total_files, 1);
        assert_eq!(r.children.len(), 3);
        for c in &r.children {
            assert!(!c.complete, "{} was not read", c.path.display());
            assert_eq!((c.size, c.file_count), (0, 0));
        }

        // Depth 2: a, b and c are listed; only a has a folder below.
        let two = build(Some(2));
        let r = two.root_report(10);
        assert!(!r.complete);
        let a = by_name(&r, "a");
        assert!(!a.complete);
        assert_eq!(a.file_count, 1, "a's own file is counted");
        assert!(a.size > 0);
        assert!(by_name(&r, "b").complete);
        assert!(by_name(&r, "c").complete);
        let x = &two.report(&d.join("a"), 10).unwrap().children[0];
        assert!(!x.complete);
        assert_eq!((x.size, x.file_count), (0, 0));
        assert!(two.report(&d.join("b"), 10).unwrap().complete);

        // Depth 3 still stops above y; depth 4 reads everything.
        assert!(!build(Some(3)).complete());
        let four = build(Some(4));
        assert!(four.complete());
        assert_eq!(
            four.root_report(10).total_size,
            full.root_report(10).total_size
        );

        // What a snapshot of a partial scan keeps, and refuses.
        let snap = crate::Snapshot::capture(&two);
        assert!(!snap.complete);
        let file = d.join("partial.dgs");
        snap.save(&file).unwrap();
        let loaded = crate::Snapshot::load(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert!(!loaded.complete && loaded.has_files);
        let back = loaded.to_tree().unwrap();
        assert!(!back.complete());
        assert_eq!(back.root_report(10).children, r.children);
        let full_snap = crate::Snapshot::capture(&full);
        assert!(full_snap.complete);
        for (old, new) in [(&full_snap, &snap), (&snap, &full_snap)] {
            assert!(matches!(
                crate::diff(old, new, &d),
                Err(crate::SnapshotError::Partial)
            ));
        }
        assert!(full_snap.to_tree().unwrap().complete());

        // Removing the only unread folders makes the rest exact again.
        let mut one = one;
        for name in ["a", "b"] {
            assert!(one.remove(&d.join(name)));
            assert!(!one.complete());
        }
        assert!(one.remove(&d.join("c")));
        assert!(one.complete());
        assert!(one.root_report(10).complete);

        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cancelled_walk_is_flagged() {
        let d = tmpdir("cancel");
        write(&d.join("a/1.bin"), &[0u8; 10]);
        let p = Progress::default();
        p.cancel();
        let tree = ScanTree::build(&d, &Options::default(), &p);
        assert!(tree.cancelled());
        let r = tree.root_report(10);
        assert!(r.cancelled);
        assert!(!r.complete && !tree.complete());
        assert_eq!(r.total_files, 0);
        let ok = ScanTree::build(&d, &Options::default(), &Progress::default());
        assert!(!ok.root_report(10).cancelled);
        assert!(ok.complete() && ok.root_report(10).complete);
        assert_eq!(ok.root_report(10).total_files, 1);
        assert_eq!(logical_total(&d), 10);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn batches_in_any_order_build_one_tree() {
        // The walk hands over directories in whatever order its threads finish:
        // a deep batch first creates its missing ancestors, and the parents'
        // own batches later must not create them a second time.
        let root = PathBuf::from("/r");
        let batch = |dir: &str, subdirs: &[&str], files: &[(&str, u64)]| Batch {
            dir: PathBuf::from(dir),
            files: files
                .iter()
                .map(|&(p, size)| FileEntry {
                    path: PathBuf::from(p),
                    size,
                    logical_size: size,
                })
                .collect(),
            modified: vec![0; files.len()],
            subdirs: subdirs.iter().map(PathBuf::from).collect(),
            others: Vec::new(),
            skipped: 0,
        };
        let mut b = Builder {
            tree: ScanTree {
                root: root.clone(),
                nodes: vec![Node::new(OsStr::new(""), None)],
                exts: Vec::new(),
                skipped: 0,
                cancelled: false,
                engine: Engine::Walk,
                mft_fallback: None,
                disk_total: 0,
                disk_free: 0,
            },
            pending: HashMap::new(),
            ext_ids: HashMap::new(),
        };
        b.add(batch("/r/p/q", &[], &[("/r/p/q/a.TXT", 5)]));
        b.add(batch("/r", &["/r/p", "/r/z"], &[("/r/top.bin", 1)]));
        b.add(batch("/r/p", &["/r/p/q"], &[("/r/p/b.txt", 2)]));
        b.add(batch("/r/z", &[], &[]));
        let tree = b.finish(false);

        assert_eq!(tree.nodes.len(), 4, "no node created twice");
        assert!(tree.complete(), "every folder got its batch");
        let r = tree.root_report(10);
        assert_eq!(r.total_size, 8);
        assert_eq!(r.total_files, 3);
        assert_eq!(r.total_dirs, 3);
        assert_eq!(r.children[0].path, root.join("p"));
        assert_eq!(r.children[0].size, 7);
        let q = tree.report(&root.join("p").join("q"), 10).unwrap();
        assert_eq!(q.top_files[0].path, root.join("p").join("q").join("a.TXT"));
        // Extensions are folded to lower case.
        let s = tree.file_stats(&root).unwrap();
        assert_eq!(s.extensions[0].ext, "txt");
        assert_eq!(s.extensions[0].count, 2);
    }
}
