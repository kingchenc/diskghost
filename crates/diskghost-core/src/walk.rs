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
    /// Device ids of remote filesystems ([`Options::skip_remote`]) other than
    /// the root's: listed when met, never descended into.
    remote: HashSet<u64>,
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
    let remote = if opts.skip_remote {
        remote_devices()
    } else {
        HashSet::new()
    };
    walk_with_remote(root, opts, progress, sink, remote)
}

/// [`walk_dyn`] with the set of remote filesystems (device ids) given — the
/// seam that lets tests mark a local mount as remote.
fn walk_with_remote(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    sink: &mut dyn FnMut(Batch),
    mut remote: HashSet<u64>,
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
    // Device of the root: the reference for both bounds. A root on a remote
    // filesystem is the user's explicit choice, so that one is walked.
    let root_dev = if opts.same_file_system || !remote.is_empty() {
        dev_of(None, root)
    } else {
        None
    };
    if let Some(dev) = root_dev {
        remote.remove(&dev);
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
        remote,
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
                Ok(pool) => pool.scope(|s| visit(s, &ctx, start, 0, root_dev)),
                Err(_) => rayon::scope(|s| visit(s, &ctx, start, 0, root_dev)),
            }
            // `ctx` (and with it the sender) drops here, ending the receiver loop.
        });
        for batch in rx {
            sink(batch);
        }
    });
    (Engine::Walk, fallback)
}

/// Device ids of the mounted network and FUSE filesystems, read from the
/// kernel's mount table — no call ever reaches the (possibly hung) share.
#[cfg(target_os = "linux")]
fn remote_devices() -> HashSet<u64> {
    std::fs::read_to_string("/proc/self/mountinfo")
        .map(|info| {
            info.lines()
                .filter_map(parse_mountinfo_line)
                .filter(|(_, t)| is_remote_fs(t))
                .map(|(dev, _)| dev)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(not(target_os = "linux"))]
fn remote_devices() -> HashSet<u64> {
    HashSet::new()
}

/// FUSE filesystems that only re-present local storage: walked like any
/// local disk. Every other FUSE filesystem (`gvfsd-fuse`, `sshfs`, `rclone`,
/// `s3fs`, the document portal, …) counts as remote — its daemon may answer
/// from the network, block, or loop.
const LOCAL_FUSE: &[&str] = &[
    "mergerfs",
    "bindfs",
    "gocryptfs",
    "encfs",
    "cryfs",
    "securefs",
    "unionfs",
    "unionfs-fuse",
    "fuse-overlayfs",
    "ntfs-3g",
    "exfat",
    "exfat-fuse",
    "squashfuse",
];

/// Whether a filesystem type (as in `/proc/self/mountinfo`) is reached over
/// the network or through a non-local FUSE daemon. `fuseblk` (ntfs-3g and
/// exFAT on a local block device) is local.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
fn is_remote_fs(fstype: &str) -> bool {
    match fstype {
        "nfs" | "nfs4" | "cifs" | "smb3" | "smbfs" | "ncpfs" | "afs" | "ceph" | "glusterfs"
        | "lustre" | "davfs" | "fuse" => true,
        t => t
            .strip_prefix("fuse.")
            .is_some_and(|sub| !LOCAL_FUSE.contains(&sub)),
    }
}

/// Device id and filesystem type of one `/proc/self/mountinfo` line
/// (`id parent major:minor root mountpoint options [tags] - fstype ...`).
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
fn parse_mountinfo_line(line: &str) -> Option<(u64, &str)> {
    let (head, tail) = line.split_once(" - ")?;
    let (major, minor) = head.split_whitespace().nth(2)?.split_once(':')?;
    let fstype = tail.split_whitespace().next()?;
    Some((makedev(major.parse().ok()?, minor.parse().ok()?), fstype))
}

/// glibc's `makedev`: the `st_dev` value of device `major:minor`.
#[cfg_attr(not(any(test, target_os = "linux")), allow(dead_code))]
fn makedev(major: u64, minor: u64) -> u64 {
    ((major & 0xffff_f000) << 32)
        | ((major & 0x0000_0fff) << 8)
        | ((minor & 0xffff_ff00) << 12)
        | (minor & 0x0000_00ff)
}

/// Device id of a directory (following a link), if the platform has one.
#[cfg(unix)]
fn dev_of(meta: Option<&std::fs::Metadata>, path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    match meta {
        Some(m) => Some(m.dev()),
        None => std::fs::metadata(path).ok().map(|m| m.dev()),
    }
}

#[cfg(not(unix))]
fn dev_of(_meta: Option<&std::fs::Metadata>, _path: &Path) -> Option<u64> {
    None
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

/// List `dir` (device `dev`, when known) and spawn a task per subdirectory.
fn visit<'s, 'c: 's>(
    s: &rayon::Scope<'s>,
    ctx: &'c Ctx<'c>,
    dir: PathBuf,
    depth: usize,
    dev: Option<u64>,
) {
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
            // Out of bounds — listed like a depth-limited folder (shown as
            // not measured) but never opened: another filesystem when the
            // walk stays on the root's (`same_file_system`), and a remote one
            // (`skip_remote`) however it is reached — mounted below, or via a
            // followed link. Remote and FUSE trees otherwise stall the scan
            // on every read, and gvfs loops through a remote `/sys` forever.
            let sub_dev = if ctx.same_file_system || !ctx.remote.is_empty() {
                dev_of(meta.as_ref(), &path)
            } else {
                None
            };
            let foreign = match (sub_dev, dev) {
                (Some(sub), Some(here)) => sub != here,
                // No device ids (not unix, or a failed stat): ask the path.
                _ => ctx.same_file_system && is_mount_point(&path),
            };
            let in_bounds = !(ctx.same_file_system && foreign)
                && !sub_dev.is_some_and(|d| ctx.remote.contains(&d));
            if descend && in_bounds {
                let sub = path.clone();
                s.spawn(move |s| visit(s, ctx, sub, child_depth, sub_dev));
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

    /// A readable mounted directory on another filesystem than `near`, and
    /// its device id — stands in for a remote share (no real network needed).
    /// Only local non-FUSE mounts are candidates, chosen from the mount table
    /// before any `stat`: the tests never touch a real share (gvfs, NFS, …).
    #[cfg(target_os = "linux")]
    fn foreign_mount(near: &Path) -> (PathBuf, u64) {
        use std::os::unix::fs::MetadataExt;
        let here = std::fs::metadata(near).unwrap().dev();
        let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
        mounts
            .lines()
            .filter_map(|l| {
                let mut f = l.split_whitespace().skip(1);
                let (path, fstype) = (f.next()?, f.next()?);
                (!fstype.starts_with("fuse") && !is_remote_fs(fstype)).then_some(path)
            })
            .map(PathBuf::from)
            .filter(|p| p != Path::new("/") && crate::is_mount_point(p))
            .filter(|p| std::fs::read_dir(p).is_ok())
            .find_map(|p| {
                let dev = std::fs::metadata(&p).ok()?.dev();
                (dev != here).then_some((p, dev))
            })
            .expect("no readable mount on another filesystem")
    }

    /// Directories `walk` listed under `root`, with `remote` marked remote.
    #[cfg(target_os = "linux")]
    fn listed(root: &Path, opts: &Options, remote: &[u64]) -> Vec<PathBuf> {
        let visited = Mutex::new(Vec::new());
        walk_with_remote(
            root,
            opts,
            &Progress::default(),
            &mut |b: Batch| visited.lock().unwrap().push(b.dir),
            remote.iter().copied().collect(),
        );
        visited.into_inner().unwrap()
    }

    /// Live incident (0.12.2, ~600 min, never finished): a GUI scan reached
    /// `/run/user/1000/gvfs/sftp:host=…` through a followed link and walked
    /// the remote host's `/sys` forever — gvfs serves remote symlinks as
    /// plain directories with a new inode per path, so no loop check can
    /// stop it. A followed link into a remote filesystem must not be opened.
    #[cfg(target_os = "linux")]
    #[test]
    fn followed_link_into_remote_fs_is_not_descended() {
        let d = tmpdir("link-remote");
        let (mount, dev) = foreign_mount(&d);
        let link = d.join("share");
        std::os::unix::fs::symlink(&mount, &link).unwrap();
        let opts = Options {
            follow_symlinks: true,
            skip_remote: true,
            max_depth: Some(2),
            ..Options::default()
        };

        // Not remote: the link is followed and its target listed (follow
        // keeps working for local filesystems).
        assert!(listed(&d, &opts, &[]).contains(&link));
        // Remote: listed as a folder of the root, never opened.
        let dirs = listed(&d, &opts, &[dev]);
        assert!(
            !dirs.iter().any(|p| p.starts_with(&link)),
            "walk descended into remote filesystem through {link:?}: {dirs:?}"
        );
        assert!(dirs.contains(&d), "the root itself is still listed");
    }

    /// A remote filesystem mounted below the root is not descended into even
    /// when `same_file_system` is off — the GUI scans ordinary folders
    /// without the bound, and that is how gvfs mounts were reached.
    #[cfg(target_os = "linux")]
    #[test]
    fn mounted_remote_fs_is_not_descended_without_same_file_system() {
        let (mount, dev) = foreign_mount(&std::env::temp_dir());
        let root = mount.parent().unwrap().to_path_buf();
        let opts = Options {
            skip_remote: true,
            max_depth: Some(mount.strip_prefix(&root).unwrap().components().count() + 1),
            ..Options::default()
        };
        // Local: walked (a second local disk inside the tree stays counted).
        assert!(listed(&root, &opts, &[]).contains(&mount));
        // Remote: not walked.
        assert!(
            !listed(&root, &opts, &[dev]).contains(&mount),
            "walk descended into remote mount {mount:?}"
        );
    }

    /// Scanning the remote share itself is the user's explicit choice: a root
    /// on a remote filesystem is walked.
    #[cfg(target_os = "linux")]
    #[test]
    fn remote_root_is_still_walked() {
        let (mount, dev) = foreign_mount(&std::env::temp_dir());
        let opts = Options {
            skip_remote: true,
            max_depth: Some(2),
            ..Options::default()
        };
        let dirs = listed(&mount, &opts, &[dev]);
        let has_subdir = std::fs::read_dir(&mount)
            .unwrap()
            .flatten()
            .any(|e| e.file_type().is_ok_and(|t| t.is_dir()));
        if has_subdir {
            assert!(dirs.len() > 1, "remote root was not walked: {dirs:?}");
        }
    }

    /// The live gvfs mount is classified remote from `/proc/self/mountinfo`
    /// (read-only table, no access to the share): `fuse.gvfsd-fuse`, NFS,
    /// SMB are remote; local and local-FUSE filesystems are not.
    #[test]
    fn remote_fs_types() {
        for t in [
            "fuse.gvfsd-fuse",
            "fuse.sshfs",
            "fuse.rclone",
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
        ] {
            assert!(is_remote_fs(t), "{t} should be remote");
        }
        for t in [
            "ext4",
            "btrfs",
            "xfs",
            "tmpfs",
            "fuseblk",
            "fuse.mergerfs",
            "fuse.gocryptfs",
            "ntfs3",
            "squashfs",
            "overlay",
        ] {
            assert!(!is_remote_fs(t), "{t} should be local");
        }
    }

    /// `major:minor` from mountinfo maps to the same id `stat` reports.
    #[cfg(target_os = "linux")]
    #[test]
    fn mountinfo_dev_matches_stat() {
        use std::os::unix::fs::MetadataExt;
        let line = "2188 190 0:78 / /run/user/1000/gvfs rw,nosuid,nodev,relatime \
                    shared:1525 - fuse.gvfsd-fuse gvfsd-fuse rw,user_id=1000";
        assert_eq!(
            parse_mountinfo_line(line),
            Some((makedev(0, 78), "fuse.gvfsd-fuse"))
        );
        // Against the kernel: `/proc` (btrfs is the known exception — its
        // mountinfo shows the superblock, `stat` the subvolume; never remote).
        let proc_dev = std::fs::metadata("/proc").unwrap().dev();
        let info = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
        let line = info
            .lines()
            .find(|l| l.split_whitespace().nth(4) == Some("/proc"))
            .expect("/proc not in mountinfo");
        assert_eq!(parse_mountinfo_line(line), Some((proc_dev, "proc")));
    }
}
