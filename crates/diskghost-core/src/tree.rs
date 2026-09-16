//! The whole directory tree from a single walk, held in memory so a UI can
//! drill into any sub-folder — and back up — without touching the disk again.
//!
//! Nodes live in an arena (`Vec<Node>`) and are looked up by path through an
//! index. A node's totals (`size`, `file_count`, `dir_count`) always cover its
//! entire subtree; the files sitting directly in a directory are kept on that
//! node so any sub-folder can produce the same [`ScanReport`] a fresh scan of
//! it would.

use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::{disk_space, walk, DirSize, FileEntry, Options, Progress, ScanReport};

struct Node {
    path: PathBuf,
    parent: Option<usize>,
    /// Bytes in this directory and everything below it.
    size: u64,
    /// Files in this directory and everything below it.
    file_count: u64,
    /// Directories below this one (not counting itself).
    dir_count: u64,
    children: Vec<usize>,
    /// Files sitting directly in this directory.
    files: Vec<FileEntry>,
}

impl Node {
    fn new(path: PathBuf, parent: Option<usize>) -> Node {
        Node {
            path,
            parent,
            size: 0,
            file_count: 0,
            dir_count: 0,
            children: Vec::new(),
            files: Vec::new(),
        }
    }
}

/// An in-memory scan of a directory tree. Build it once with
/// [`ScanTree::build`], then ask for a [`ScanReport`] of the root or of any
/// sub-folder with [`ScanTree::report`] — no further I/O.
pub struct ScanTree {
    /// Arena; index 0 is always the root.
    nodes: Vec<Node>,
    index: HashMap<PathBuf, usize>,
    skipped: u64,
    cancelled: bool,
    disk_total: u64,
    disk_free: u64,
}

impl ScanTree {
    /// Walk `root` (honouring `opts`, reporting to `progress`) and build the tree.
    pub fn build(root: &Path, opts: &Options, progress: &Progress) -> ScanTree {
        let walked = walk(root, opts, progress);
        let mut tree = ScanTree {
            nodes: vec![Node::new(root.to_path_buf(), None)],
            index: HashMap::new(),
            skipped: walked.skipped,
            cancelled: progress.cancelled(),
            disk_total: 0,
            disk_free: 0,
        };
        tree.index.insert(root.to_path_buf(), 0);

        for dir in walked.dirs {
            tree.ensure_dir(&dir);
        }

        // Files of one directory arrive together, so remembering the last
        // parent saves a hash lookup per file on the hot path.
        let mut last: Option<(PathBuf, usize)> = None;
        for file in walked.files {
            let parent = file.path.parent().unwrap_or(root);
            let id = match &last {
                Some((p, id)) if p == parent => *id,
                _ => {
                    let id = tree.ensure_dir(parent);
                    last = Some((parent.to_path_buf(), id));
                    id
                }
            };
            let node = &mut tree.nodes[id];
            node.size += file.size;
            node.file_count += 1;
            node.files.push(file);
        }

        // Roll the totals up. A child is always created after its parent, so
        // walking the arena backwards visits every node before its parent.
        for id in (1..tree.nodes.len()).rev() {
            let node = &tree.nodes[id];
            let (size, files, dirs, parent) =
                (node.size, node.file_count, node.dir_count + 1, node.parent);
            if let Some(p) = parent {
                let pn = &mut tree.nodes[p];
                pn.size += size;
                pn.file_count += files;
                pn.dir_count += dirs;
            }
        }

        let (disk_total, disk_free) = disk_space(root);
        tree.disk_total = disk_total;
        tree.disk_free = disk_free;
        tree
    }

    /// Node id of `path`, creating it (and any missing ancestors up to the
    /// nearest known one) if needed. Every node is inserted after its parent.
    fn ensure_dir(&mut self, path: &Path) -> usize {
        if let Some(&id) = self.index.get(path) {
            return id;
        }
        let mut missing: Vec<PathBuf> = Vec::new();
        let mut cur = path.to_path_buf();
        let anchor = loop {
            if let Some(&id) = self.index.get(&cur) {
                break id;
            }
            missing.push(cur.clone());
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                // Ran past every known ancestor: hang the chain off the root.
                None => break 0,
            }
        };
        let mut parent = anchor;
        for p in missing.into_iter().rev() {
            let id = self.nodes.len();
            self.nodes.push(Node::new(p.clone(), Some(parent)));
            self.nodes[parent].children.push(id);
            self.index.insert(p, id);
            parent = id;
        }
        parent
    }

    /// The directory this tree was built from.
    pub fn root(&self) -> &Path {
        &self.nodes[0].path
    }

    /// Whether `path` is a directory in this tree (the root included).
    pub fn contains(&self, path: &Path) -> bool {
        self.index.contains_key(path)
    }

    /// True if the walk was cancelled; the tree then covers only what was seen.
    pub fn cancelled(&self) -> bool {
        self.cancelled
    }

    /// Re-query the filesystem's total/free bytes (e.g. after deleting files).
    pub fn refresh_disk_space(&mut self) {
        let (total, free) = disk_space(self.root());
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
        let &id = self.index.get(path)?;
        Some(self.report_of(id, top_n))
    }

    fn report_of(&self, id: usize, top_n: usize) -> ScanReport {
        let node = &self.nodes[id];

        let mut kids: Vec<&Node> = node.children.iter().map(|&c| &self.nodes[c]).collect();
        kids.sort_by_key(|k| Reverse(k.size));
        let children: Vec<DirSize> = kids
            .iter()
            .take(top_n)
            .map(|k| DirSize {
                path: k.path.clone(),
                size: k.size,
                file_count: k.file_count,
            })
            .collect();

        let root_files_size = node.files.iter().map(|f| f.size).sum();
        let root_files_count = node.files.len() as u64;

        // Biggest files anywhere below `id`: gather the subtree, then select
        // the top slice without sorting everything.
        let mut all: Vec<&FileEntry> = Vec::new();
        let mut stack = vec![id];
        while let Some(i) = stack.pop() {
            let n = &self.nodes[i];
            all.extend(n.files.iter());
            stack.extend(n.children.iter().copied());
        }
        if top_n == 0 {
            all.clear();
        } else if all.len() > top_n {
            all.select_nth_unstable_by_key(top_n - 1, |f| Reverse(f.size));
            all.truncate(top_n);
        }
        all.sort_by_key(|f| Reverse(f.size));
        let top_files = all.into_iter().cloned().collect();

        ScanReport {
            root: node.path.clone(),
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
        }
    }

    /// Every directory below the root with its subtree totals, sorted by path.
    /// The root itself is not included.
    pub fn dir_sizes(&self) -> Vec<DirSize> {
        let mut dirs: Vec<DirSize> = self
            .index
            .values()
            .filter(|&&id| id != 0)
            .map(|&id| {
                let n = &self.nodes[id];
                DirSize {
                    path: n.path.clone(),
                    size: n.size,
                    file_count: n.file_count,
                }
            })
            .collect();
        dirs.sort_by(|a, b| a.path.cmp(&b.path));
        dirs
    }

    /// Drop a directory (with everything below it) or a single file from the
    /// tree, adjusting every ancestor's totals — what a UI does after deleting
    /// the path on disk, instead of re-walking. The root cannot be removed.
    /// Returns `false` if the path is not in the tree.
    pub fn remove(&mut self, path: &Path) -> bool {
        if let Some(&id) = self.index.get(path) {
            if id == 0 {
                return false;
            }
            let node = &self.nodes[id];
            let (size, files, dirs, parent) =
                (node.size, node.file_count, node.dir_count + 1, node.parent);
            // Unindex the subtree; the arena slots simply become unreachable.
            let mut stack = vec![id];
            while let Some(i) = stack.pop() {
                let p = std::mem::take(&mut self.nodes[i].path);
                self.index.remove(&p);
                self.nodes[i].files.clear();
                stack.extend(std::mem::take(&mut self.nodes[i].children));
            }
            if let Some(p) = parent {
                self.nodes[p].children.retain(|&c| c != id);
                self.subtract(p, size, files, dirs);
            }
            return true;
        }
        let Some(parent) = path.parent() else {
            return false;
        };
        let Some(&pid) = self.index.get(parent) else {
            return false;
        };
        let Some(pos) = self.nodes[pid].files.iter().position(|f| f.path == path) else {
            return false;
        };
        let file = self.nodes[pid].files.swap_remove(pos);
        self.subtract(pid, file.size, 1, 0);
        true
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
        let d = tmpdir("drill");
        write(&d.join("a/1.bin"), &[0u8; 1000]);
        write(&d.join("a/x/2.bin"), &[0u8; 500]);
        write(&d.join("a/x/y/3.bin"), &[0u8; 250]);
        write(&d.join("b/4.bin"), &[0u8; 200]);
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
    fn cancelled_walk_is_flagged() {
        let d = tmpdir("cancel");
        write(&d.join("a/1.bin"), &[0u8; 10]);
        let p = Progress::default();
        p.cancel();
        let tree = ScanTree::build(&d, &Options::default(), &p);
        assert!(tree.cancelled());
        let r = tree.root_report(10);
        assert!(r.cancelled);
        assert_eq!(r.total_files, 0);
        let ok = ScanTree::build(&d, &Options::default(), &Progress::default());
        assert!(!ok.root_report(10).cancelled);
        assert_eq!(ok.root_report(10).total_files, 1);
        assert_eq!(logical_total(&d), 10);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn missing_ancestors_are_created_on_demand() {
        let d = tmpdir("ensure");
        let mut tree = ScanTree::build(&d, &Options::default(), &Progress::default());
        let deep = d.join("p/q/r");
        let id = tree.ensure_dir(&deep);
        assert_eq!(tree.nodes[id].path, deep);
        assert!(tree.contains(&d.join("p")));
        assert!(tree.contains(&d.join("p/q")));
        assert_eq!(tree.ensure_dir(&deep), id);
        std::fs::remove_dir_all(&d).ok();
    }
}
