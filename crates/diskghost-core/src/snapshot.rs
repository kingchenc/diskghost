//! Snapshots persist a scan so a later one can be compared against it:
//! "what grew since last time" — down to the single file.
//!
//! A [`Snapshot`] records *every* directory (with its subtree totals) and
//! *every* file (with its size) of a [`ScanTree`]. Entries are stored sorted
//! by path, each path front-coded against the previous one (only the differing
//! tail is written) with variable-length integers, and the whole stream is
//! zstd-compressed: about 8 bytes per file, written and read at hundreds of
//! megabytes per second. Because both sides are sorted the same way, [`diff`] compares two
//! snapshots in one streaming pass without ever expanding them in memory.
//!
//! Format 1 snapshots (plain JSON, directories only) are still read; compared
//! against them, only folder changes can be reported.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::tree::cmp_name;
use crate::{DirSize, ScanTree};

/// On-disk format version written by [`Snapshot::save`].
pub const SNAPSHOT_FORMAT: u32 = 2;

/// How many changed files of each kind a [`DiffReport`] lists at most (the
/// biggest changes; the counts and byte totals always cover all of them).
pub const DIFF_FILE_LIMIT: usize = 1000;

const MAGIC: &[u8; 8] = b"DGSNAP02";

/// What an entry of the stream is (its first byte).
pub(crate) const ENTRY_FILE: u8 = 0;
pub(crate) const ENTRY_DIR: u8 = 1;
/// A directory whose contents were never read (the depth limit, or a
/// cancelled walk). Readers before 0.12 take it for an ordinary directory.
pub(crate) const ENTRY_UNREAD_DIR: u8 = 2;

/// Header flags.
const FLAG_HAS_FILES: u64 = 1;
/// Some directory was not read, or the walk was cancelled.
const FLAG_PARTIAL: u64 = 2;

/// Why a snapshot could not be read, written or compared.
#[derive(Debug)]
pub enum SnapshotError {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The file is a snapshot of another format version.
    Format(u32),
    /// The file is not a snapshot, or it is damaged.
    Corrupt(String),
    /// The two snapshots were taken of different directories.
    RootMismatch {
        old: PathBuf,
        new: PathBuf,
    },
    /// The directory to compare at is not part of the newer snapshot.
    NotInSnapshot(PathBuf),
    /// A snapshot of a scan that did not read every folder: compared, the
    /// folders it skipped would look emptied.
    Partial,
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "{e}"),
            SnapshotError::Json(e) => write!(f, "invalid snapshot: {e}"),
            SnapshotError::Format(v) => write!(
                f,
                "unsupported snapshot format {v} (this build reads formats 1 and {SNAPSHOT_FORMAT})"
            ),
            SnapshotError::Corrupt(why) => write!(f, "invalid snapshot: {why}"),
            SnapshotError::RootMismatch { old, new } => write!(
                f,
                "snapshots are of different directories: {} vs {}",
                old.display(),
                new.display()
            ),
            SnapshotError::NotInSnapshot(p) => {
                write!(f, "not a directory in the snapshot: {}", p.display())
            }
            SnapshotError::Partial => write!(
                f,
                "a scan that did not read every folder (depth limit, or cancelled) cannot be compared"
            ),
        }
    }
}

impl std::error::Error for SnapshotError {}

impl From<std::io::Error> for SnapshotError {
    fn from(e: std::io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

impl From<serde_json::Error> for SnapshotError {
    fn from(e: serde_json::Error) -> Self {
        SnapshotError::Json(e)
    }
}

/// A persisted scan: totals of the root and every directory and file below it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Format the snapshot was captured in or read from.
    pub format: u32,
    /// When the scan was taken, in seconds since the Unix epoch.
    pub taken_at: u64,
    pub root: PathBuf,
    pub total_size: u64,
    pub total_files: u64,
    pub total_dirs: u64,
    /// Whether every file is recorded. Format 1 snapshots only know folders.
    pub has_files: bool,
    /// Whether the scan read every folder (see [`ScanTree::complete`]). Only
    /// complete snapshots can be compared.
    pub complete: bool,
    /// Number of recorded entries (directories plus files).
    pub entries: u64,
    /// The zstd-compressed entry stream.
    data: Vec<u8>,
}

/// A format 1 (JSON) snapshot; only what is needed to convert it.
#[derive(Deserialize)]
struct LegacySnapshot {
    format: u32,
    #[serde(default)]
    taken_at: u64,
    #[serde(default)]
    root: PathBuf,
    #[serde(default)]
    total_size: u64,
    #[serde(default)]
    total_files: u64,
    #[serde(default)]
    total_dirs: u64,
    #[serde(default)]
    dirs: Vec<DirSize>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Sort order of entry keys: byte order with the separator below every other
/// byte, so a folder's contents directly follow it — the order a depth-first
/// walk over name-sorted children produces.
fn cmp_key(a: &[u8], b: &[u8]) -> Ordering {
    let map = |c: &u8| if *c == b'/' { 0 } else { *c };
    a.iter().map(map).cmp(b.iter().map(map))
}

/// The key of `path` relative to `root` (components joined by `/`), or
/// `None` if `path` is not `root` or below it.
fn key_of(root: &Path, path: &Path) -> Option<Vec<u8>> {
    let rel = path.strip_prefix(root).ok()?;
    let mut key = Vec::new();
    for c in rel.components() {
        let Component::Normal(name) = c else {
            return None;
        };
        if !key.is_empty() {
            key.push(b'/');
        }
        key.extend_from_slice(name.as_encoded_bytes());
    }
    Some(key)
}

fn path_of_key(root: &Path, key: &[u8]) -> PathBuf {
    let mut p = root.to_path_buf();
    if !key.is_empty() {
        for part in key.split(|&b| b == b'/') {
            p.push(&*String::from_utf8_lossy(part));
        }
    }
    p
}

pub(crate) fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_u8(r: &mut impl BufRead) -> Result<u8, SnapshotError> {
    let buf = r.fill_buf()?;
    let Some(&b) = buf.first() else {
        return Err(SnapshotError::Corrupt("truncated entry stream".into()));
    };
    r.consume(1);
    Ok(b)
}

pub(crate) fn get_varint(r: &mut impl BufRead) -> Result<u64, SnapshotError> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let b = get_u8(r)?;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    Err(SnapshotError::Corrupt("bad number".into()))
}

/// zstd level for snapshots: within a few percent of the best ratio seen on
/// real trees at a fraction of the time.
const ZSTD_LEVEL: i32 = 3;

/// Writes entries (in key order) front-coded into a zstd stream.
struct Encoder {
    out: zstd::stream::write::Encoder<'static, Vec<u8>>,
    prev: Vec<u8>,
    buf: Vec<u8>,
    count: u64,
}

impl Encoder {
    fn new() -> Encoder {
        Encoder {
            // Only fails on an invalid level.
            out: zstd::stream::write::Encoder::new(Vec::new(), ZSTD_LEVEL)
                .expect("valid zstd level"),
            prev: Vec::new(),
            buf: Vec::with_capacity(1 << 16),
            count: 0,
        }
    }

    fn entry(&mut self, key: &[u8], kind: u8, size: u64, files: u64) {
        let shared = self
            .prev
            .iter()
            .zip(key)
            .take_while(|(a, b)| a == b)
            .count();
        self.buf.push(kind);
        put_varint(&mut self.buf, shared as u64);
        put_varint(&mut self.buf, (key.len() - shared) as u64);
        self.buf.extend_from_slice(&key[shared..]);
        put_varint(&mut self.buf, size);
        if kind != ENTRY_FILE {
            put_varint(&mut self.buf, files);
        }
        self.prev.truncate(shared);
        self.prev.extend_from_slice(&key[shared..]);
        self.count += 1;
        if self.buf.len() >= 1 << 16 {
            self.flush();
        }
    }

    fn flush(&mut self) {
        // Writing into a Vec cannot fail.
        let _ = self.out.write_all(&self.buf);
        self.buf.clear();
    }

    fn finish(mut self) -> (Vec<u8>, u64) {
        self.flush();
        let data = self.out.finish().unwrap_or_default();
        (data, self.count)
    }
}

/// One decoded entry; `key` is reused between entries.
struct Cursor<'a> {
    r: BufReader<zstd::stream::read::Decoder<'a, &'a [u8]>>,
    left: u64,
    key: Vec<u8>,
    kind: u8,
    dir: bool,
    size: u64,
    files: u64,
    done: bool,
}

impl<'a> Cursor<'a> {
    fn new(s: &'a Snapshot) -> Result<Cursor<'a>, SnapshotError> {
        Ok(Cursor {
            r: BufReader::with_capacity(
                1 << 16,
                zstd::stream::read::Decoder::with_buffer(&s.data[..])?,
            ),
            left: s.entries,
            key: Vec::new(),
            kind: ENTRY_FILE,
            dir: false,
            size: 0,
            files: 0,
            done: s.entries == 0,
        })
    }

    /// Move to the next entry; `done` is set after the last one.
    fn advance(&mut self) -> Result<(), SnapshotError> {
        if self.left == 0 {
            self.done = true;
            return Ok(());
        }
        self.left -= 1;
        self.kind = get_u8(&mut self.r)?;
        self.dir = self.kind != ENTRY_FILE;
        let shared = get_varint(&mut self.r)? as usize;
        let len = get_varint(&mut self.r)? as usize;
        if shared > self.key.len() || len > 1 << 20 {
            return Err(SnapshotError::Corrupt("bad path".into()));
        }
        self.key.truncate(shared);
        let start = self.key.len();
        self.key.resize(start + len, 0);
        self.r.read_exact(&mut self.key[start..])?;
        self.size = get_varint(&mut self.r)?;
        self.files = if self.dir {
            get_varint(&mut self.r)?
        } else {
            0
        };
        self.done = false;
        Ok(())
    }
}

impl Snapshot {
    /// Capture `tree` as it is now: every directory and file.
    pub fn capture(tree: &ScanTree) -> Snapshot {
        let mut enc = Encoder::new();
        let mut key = Vec::new();
        capture_node(tree, 0, &mut key, &mut enc);
        let (data, entries) = enc.finish();
        let root = &tree.nodes[0];
        let r = tree.root_report(0);
        Snapshot {
            format: SNAPSHOT_FORMAT,
            taken_at: now_unix(),
            root: tree.root().to_path_buf(),
            total_size: root.size,
            total_files: root.file_count,
            total_dirs: r.total_dirs,
            has_files: true,
            complete: tree.complete(),
            entries,
            data,
        }
    }

    /// Bytes the snapshot takes when saved: its compressed entries plus a
    /// small header.
    pub fn stored_size(&self) -> u64 {
        (MAGIC.len() + 64 + self.root.to_string_lossy().len() + self.data.len()) as u64
    }

    /// Read a snapshot written by [`Snapshot::save`] (or a format 1 JSON one).
    pub fn load(path: &Path) -> Result<Snapshot, SnapshotError> {
        let bytes = std::fs::read(path)?;
        if bytes.starts_with(MAGIC) {
            return Snapshot::decode(&bytes[MAGIC.len()..]);
        }
        if bytes.first() == Some(&b'{') {
            return Snapshot::from_legacy(serde_json::from_slice(&bytes)?);
        }
        if bytes.starts_with(b"DGSNAP") {
            let v = bytes
                .get(6..8)
                .and_then(|v| std::str::from_utf8(v).ok())
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return Err(SnapshotError::Format(v));
        }
        Err(SnapshotError::Corrupt("not a Diskghost snapshot".into()))
    }

    fn decode(mut b: &[u8]) -> Result<Snapshot, SnapshotError> {
        let mut u64_ = || -> Result<u64, SnapshotError> {
            let (head, rest) = b
                .split_first_chunk::<8>()
                .ok_or_else(|| SnapshotError::Corrupt("truncated header".into()))?;
            b = rest;
            Ok(u64::from_le_bytes(*head))
        };
        let taken_at = u64_()?;
        let total_size = u64_()?;
        let total_files = u64_()?;
        let total_dirs = u64_()?;
        let entries = u64_()?;
        let flags = u64_()?;
        let root_len = u64_()? as usize;
        let data_len = u64_()? as usize;
        if b.len() != root_len.saturating_add(data_len) {
            return Err(SnapshotError::Corrupt("truncated file".into()));
        }
        let root = PathBuf::from(
            String::from_utf8(b[..root_len].to_vec())
                .map_err(|_| SnapshotError::Corrupt("bad root".into()))?,
        );
        Ok(Snapshot {
            format: SNAPSHOT_FORMAT,
            taken_at,
            root,
            total_size,
            total_files,
            total_dirs,
            has_files: flags & FLAG_HAS_FILES != 0,
            complete: flags & FLAG_PARTIAL == 0,
            entries,
            data: b[root_len..].to_vec(),
        })
    }

    fn from_legacy(old: LegacySnapshot) -> Result<Snapshot, SnapshotError> {
        if old.format != 1 {
            return Err(SnapshotError::Format(old.format));
        }
        let mut dirs: Vec<(Vec<u8>, &DirSize)> = old
            .dirs
            .iter()
            .filter_map(|d| Some((key_of(&old.root, &d.path)?, d)))
            .filter(|(k, _)| !k.is_empty())
            .collect();
        dirs.sort_by(|a, b| cmp_key(&a.0, &b.0));
        let mut enc = Encoder::new();
        for (k, d) in &dirs {
            enc.entry(k, ENTRY_DIR, d.size, d.file_count);
        }
        let (data, entries) = enc.finish();
        Ok(Snapshot {
            format: 1,
            taken_at: old.taken_at,
            root: old.root,
            total_size: old.total_size,
            total_files: old.total_files,
            total_dirs: old.total_dirs,
            has_files: false,
            complete: true,
            entries,
            data,
        })
    }

    /// Write the snapshot (format 2), creating parent directories as needed.
    /// The file is written next to its target and renamed into place, so a
    /// crash never leaves a half-written snapshot behind.
    pub fn save(&self, path: &Path) -> Result<(), SnapshotError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let root = self.root.to_string_lossy();
        let mut flags = 0;
        if self.has_files {
            flags |= FLAG_HAS_FILES;
        }
        if !self.complete {
            flags |= FLAG_PARTIAL;
        }
        let mut out = Vec::with_capacity(self.data.len() + 80 + root.len());
        out.extend_from_slice(MAGIC);
        for v in [
            self.taken_at,
            self.total_size,
            self.total_files,
            self.total_dirs,
            self.entries,
            flags,
            root.len() as u64,
            self.data.len() as u64,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(root.as_bytes());
        out.extend_from_slice(&self.data);
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// A short, filesystem-safe key that identifies a scanned root, so a store
    /// can keep one history per directory (BLAKE3 of the path, 16 hex chars).
    pub fn root_key(root: &Path) -> String {
        let hex = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
        hex[..16].to_string()
    }

    /// Rebuild the scanned tree from a snapshot that records files (format
    /// 2): the same folders, files and sizes, browsable like a fresh scan.
    /// Logical sizes are not recorded, so they equal the on-disk sizes.
    pub fn to_tree(&self) -> Result<ScanTree, SnapshotError> {
        if !self.has_files {
            return Err(SnapshotError::Corrupt("snapshot has no files".into()));
        }
        let mut entries = Vec::with_capacity(self.entries as usize);
        let mut c = Cursor::new(self)?;
        c.advance()?;
        while !c.done {
            entries.push((c.key.clone(), c.kind, c.size));
            c.advance()?;
        }
        Ok(ScanTree::from_entries(&self.root, entries))
    }

    /// Every directory below the root with its totals, in path order.
    pub fn dirs(&self) -> Result<Vec<DirSize>, SnapshotError> {
        let mut out = Vec::new();
        let mut c = Cursor::new(self)?;
        c.advance()?;
        while !c.done {
            if c.dir {
                out.push(DirSize {
                    path: path_of_key(&self.root, &c.key),
                    size: c.size,
                    file_count: c.files,
                    // Which folders a partial scan read fully is not
                    // recorded: all of them are lower bounds.
                    complete: self.complete,
                });
            }
            c.advance()?;
        }
        Ok(out)
    }
}

/// Depth-first over name-sorted children and files, merged by name — which
/// yields the entries in [`cmp_key`] order.
fn capture_node(tree: &ScanTree, id: usize, key: &mut Vec<u8>, enc: &mut Encoder) {
    let node = &tree.nodes[id];
    let (mut d, mut f) = (0, 0);
    while d < node.children.len() || f < node.files.len() {
        let dir_first = match (node.children.get(d), node.files.get(f)) {
            (Some(&c), Some(file)) => cmp_name(&tree.nodes[c].name, &file.name).is_le(),
            (Some(_), None) => true,
            _ => false,
        };
        let len = key.len();
        if !key.is_empty() {
            key.push(b'/');
        }
        if dir_first {
            let c = node.children[d];
            let child = &tree.nodes[c];
            key.extend_from_slice(child.name.as_encoded_bytes());
            let kind = if child.listed {
                ENTRY_DIR
            } else {
                ENTRY_UNREAD_DIR
            };
            enc.entry(key, kind, child.size, child.file_count);
            capture_node(tree, c, key, enc);
            d += 1;
        } else {
            let file = &node.files[f];
            key.extend_from_slice(file.name.as_encoded_bytes());
            enc.entry(key, ENTRY_FILE, file.size, 0);
            f += 1;
        }
        key.truncate(len);
    }
}

/// One sub-folder whose size changed between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirDelta {
    pub path: PathBuf,
    pub old_size: u64,
    pub new_size: u64,
    /// `new_size - old_size`; positive means it grew.
    pub delta: i64,
    pub old_files: u64,
    pub new_files: u64,
}

/// One file that appeared, disappeared or changed size.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDelta {
    pub path: PathBuf,
    /// 0 for a new file.
    pub old_size: u64,
    /// 0 for a removed file.
    pub new_size: u64,
    pub delta: i64,
}

/// What changed between two snapshots of the same root, seen from one directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffReport {
    /// The directory the comparison is about (the root or a sub-folder).
    pub root: PathBuf,
    pub old_taken_at: u64,
    pub new_taken_at: u64,
    pub old_size: u64,
    pub new_size: u64,
    /// `new_size - old_size` of the whole directory.
    pub delta: i64,
    pub old_files: u64,
    pub new_files: u64,
    pub files_delta: i64,
    /// Immediate sub-folders that grew, biggest gain first.
    pub grown: Vec<DirDelta>,
    /// Immediate sub-folders that shrank, biggest loss first.
    pub shrunk: Vec<DirDelta>,
    /// Immediate sub-folders present now but not before, largest first.
    pub added: Vec<DirSize>,
    /// Immediate sub-folders present before but gone now, largest first.
    pub removed: Vec<DirSize>,
    /// Whether file-level changes are known (both snapshots record files).
    #[serde(default)]
    pub files_compared: bool,
    /// Files anywhere below the directory that are new, biggest first.
    #[serde(default)]
    pub new_files_list: Vec<FileDelta>,
    /// Files that are gone, biggest first.
    #[serde(default)]
    pub removed_files_list: Vec<FileDelta>,
    /// Files whose size changed, biggest change first.
    #[serde(default)]
    pub changed_files_list: Vec<FileDelta>,
    /// How many files are new / gone / changed, and the bytes involved —
    /// always complete, even where the lists above are capped.
    #[serde(default)]
    pub new_files_count: u64,
    #[serde(default)]
    pub new_files_bytes: u64,
    #[serde(default)]
    pub removed_files_count: u64,
    #[serde(default)]
    pub removed_files_bytes: u64,
    #[serde(default)]
    pub changed_files_count: u64,
    #[serde(default)]
    pub changed_files_delta: i64,
}

impl DiffReport {
    /// Keep at most `top` entries in each list.
    pub fn truncate(&mut self, top: usize) {
        self.grown.truncate(top);
        self.shrunk.truncate(top);
        self.added.truncate(top);
        self.removed.truncate(top);
        self.new_files_list.truncate(top);
        self.removed_files_list.truncate(top);
        self.changed_files_list.truncate(top);
    }

    /// True if nothing at all changed under this directory.
    pub fn is_unchanged(&self) -> bool {
        self.delta == 0
            && self.files_delta == 0
            && self.grown.is_empty()
            && self.shrunk.is_empty()
            && self.added.is_empty()
            && self.removed.is_empty()
            && self.new_files_count == 0
            && self.removed_files_count == 0
            && self.changed_files_count == 0
    }
}

fn signed(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// A file change while diffing: (|delta|, key, old size, new size).
type Change = (u64, Vec<u8>, u64, u64);

/// The biggest `DIFF_FILE_LIMIT` file changes by |delta|.
#[derive(Default)]
struct TopFiles(BinaryHeap<Reverse<Change>>);

impl TopFiles {
    fn push(&mut self, key: &[u8], old: u64, new: u64) {
        let mag = old.abs_diff(new);
        if self.0.len() >= DIFF_FILE_LIMIT {
            match self.0.peek() {
                Some(Reverse(min)) if mag > min.0 => {
                    self.0.pop();
                }
                _ => return,
            }
        }
        self.0.push(Reverse((mag, key.to_vec(), old, new)));
    }

    fn into_list(self, root: &Path) -> Vec<FileDelta> {
        let mut v: Vec<_> = self.0.into_iter().map(|Reverse(x)| x).collect();
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| cmp_key(&a.1, &b.1)));
        v.into_iter()
            .map(|(_, key, old, new)| FileDelta {
                path: path_of_key(root, &key),
                old_size: old,
                new_size: new,
                delta: signed(new) - signed(old),
            })
            .collect()
    }
}

/// Compare two snapshots of the same root at directory `at` (the root or any
/// sub-folder of the newer snapshot): totals, the immediate sub-folders that
/// grew, shrank, appeared or vanished, and — when both snapshots record files
/// — every file below `at` that is new, gone or changed in size. A folder
/// missing from the older snapshot counts as having been empty.
pub fn diff(old: &Snapshot, new: &Snapshot, at: &Path) -> Result<DiffReport, SnapshotError> {
    if !old.complete || !new.complete {
        return Err(SnapshotError::Partial);
    }
    if old.root != new.root {
        return Err(SnapshotError::RootMismatch {
            old: old.root.clone(),
            new: new.root.clone(),
        });
    }
    let at_key = key_of(&new.root, at).ok_or_else(|| SnapshotError::NotInSnapshot(at.into()))?;
    let mut acc = Acc {
        root: &new.root,
        at_key: &at_key,
        files_compared: old.has_files && new.has_files,
        old_tot: at_key
            .is_empty()
            .then_some((old.total_size, old.total_files)),
        new_tot: at_key
            .is_empty()
            .then_some((new.total_size, new.total_files)),
        grown: Vec::new(),
        shrunk: Vec::new(),
        added: Vec::new(),
        removed: Vec::new(),
        new_files: TopFiles::default(),
        gone_files: TopFiles::default(),
        changed_files: TopFiles::default(),
        new_count: 0,
        new_bytes: 0,
        gone_count: 0,
        gone_bytes: 0,
        changed_count: 0,
        changed_delta: 0,
    };

    // Merge-join the two sorted streams. A name that is a folder on one side
    // and a file on the other is two different entries: one gone, one new.
    let mut o = Cursor::new(old)?;
    let mut n = Cursor::new(new)?;
    o.advance()?;
    n.advance()?;
    while !o.done || !n.done {
        let ord = match (o.done, n.done) {
            (true, _) => Ordering::Greater,
            (_, true) => Ordering::Less,
            _ => cmp_key(&o.key, &n.key).then(n.dir.cmp(&o.dir)),
        };
        match ord {
            Ordering::Less => {
                acc.one_side(&o, false);
                o.advance()?;
            }
            Ordering::Greater => {
                acc.one_side(&n, true);
                n.advance()?;
            }
            Ordering::Equal => {
                acc.both(&o, &n);
                o.advance()?;
                n.advance()?;
            }
        }
    }
    acc.finish(old, new, at)
}

/// Where an entry sits relative to the directory a diff is about.
enum Rel {
    Outside,
    At,
    Child,
    Deeper,
}

/// Running state of a [`diff`].
struct Acc<'a> {
    root: &'a Path,
    at_key: &'a [u8],
    files_compared: bool,
    old_tot: Option<(u64, u64)>,
    new_tot: Option<(u64, u64)>,
    grown: Vec<DirDelta>,
    shrunk: Vec<DirDelta>,
    added: Vec<DirSize>,
    removed: Vec<DirSize>,
    new_files: TopFiles,
    gone_files: TopFiles,
    changed_files: TopFiles,
    new_count: u64,
    new_bytes: u64,
    gone_count: u64,
    gone_bytes: u64,
    changed_count: u64,
    changed_delta: i64,
}

impl Acc<'_> {
    fn rel(&self, key: &[u8]) -> Rel {
        let at = self.at_key;
        let rest = if at.is_empty() {
            key
        } else if key == at {
            return Rel::At;
        } else if key.len() > at.len() && key.starts_with(at) && key[at.len()] == b'/' {
            &key[at.len() + 1..]
        } else {
            return Rel::Outside;
        };
        if rest.contains(&b'/') {
            Rel::Deeper
        } else {
            Rel::Child
        }
    }

    /// An entry only the older (`is_new == false`) or the newer snapshot has.
    fn one_side(&mut self, c: &Cursor, is_new: bool) {
        match (self.rel(&c.key), c.dir) {
            (Rel::At, true) => {
                let tot = Some((c.size, c.files));
                if is_new {
                    self.new_tot = tot;
                } else {
                    self.old_tot = tot;
                }
            }
            (Rel::Child, true) => {
                let d = DirSize {
                    path: path_of_key(self.root, &c.key),
                    size: c.size,
                    file_count: c.files,
                    complete: true,
                };
                if is_new {
                    self.added.push(d);
                } else {
                    self.removed.push(d);
                }
            }
            (Rel::Child | Rel::Deeper, false) if self.files_compared => {
                if is_new {
                    self.new_count += 1;
                    self.new_bytes += c.size;
                    self.new_files.push(&c.key, 0, c.size);
                } else {
                    self.gone_count += 1;
                    self.gone_bytes += c.size;
                    self.gone_files.push(&c.key, c.size, 0);
                }
            }
            _ => {}
        }
    }

    /// The same entry in both snapshots.
    fn both(&mut self, o: &Cursor, n: &Cursor) {
        match (self.rel(&n.key), n.dir) {
            (Rel::At, true) => {
                self.old_tot = Some((o.size, o.files));
                self.new_tot = Some((n.size, n.files));
            }
            (Rel::Child, true) => {
                let delta = signed(n.size) - signed(o.size);
                if delta == 0 {
                    return;
                }
                let d = DirDelta {
                    path: path_of_key(self.root, &n.key),
                    old_size: o.size,
                    new_size: n.size,
                    delta,
                    old_files: o.files,
                    new_files: n.files,
                };
                if delta > 0 {
                    self.grown.push(d);
                } else {
                    self.shrunk.push(d);
                }
            }
            (Rel::Child | Rel::Deeper, false) if self.files_compared && o.size != n.size => {
                self.changed_count += 1;
                self.changed_delta += signed(n.size) - signed(o.size);
                self.changed_files.push(&n.key, o.size, n.size);
            }
            _ => {}
        }
    }

    fn finish(
        mut self,
        old: &Snapshot,
        new: &Snapshot,
        at: &Path,
    ) -> Result<DiffReport, SnapshotError> {
        let (new_size, new_files) = self
            .new_tot
            .ok_or_else(|| SnapshotError::NotInSnapshot(at.into()))?;
        let (old_size, old_files) = self.old_tot.unwrap_or((0, 0));
        self.grown
            .sort_by_key(|d| (Reverse(d.delta), d.path.clone()));
        self.shrunk.sort_by_key(|d| (d.delta, d.path.clone()));
        self.added
            .sort_by_key(|d| (Reverse(d.size), d.path.clone()));
        self.removed
            .sort_by_key(|d| (Reverse(d.size), d.path.clone()));
        Ok(DiffReport {
            root: at.to_path_buf(),
            old_taken_at: old.taken_at,
            new_taken_at: new.taken_at,
            old_size,
            new_size,
            delta: signed(new_size) - signed(old_size),
            old_files,
            new_files,
            files_delta: signed(new_files) - signed(old_files),
            grown: self.grown,
            shrunk: self.shrunk,
            added: self.added,
            removed: self.removed,
            files_compared: self.files_compared,
            new_files_list: self.new_files.into_list(self.root),
            removed_files_list: self.gone_files.into_list(self.root),
            changed_files_list: self.changed_files.into_list(self.root),
            new_files_count: self.new_count,
            new_files_bytes: self.new_bytes,
            removed_files_count: self.gone_count,
            removed_files_bytes: self.gone_bytes,
            changed_files_count: self.changed_count,
            changed_files_delta: self.changed_delta,
        })
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian (year, month, day).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// Render a Unix timestamp as `YYYY-MM-DD HH:MM UTC`.
pub fn format_timestamp(secs: u64) -> String {
    let days = signed(secs) / 86_400;
    let rem = secs % 86_400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Options, Progress};
    use std::io::Write;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-snap-{}-{label}", std::process::id()));
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

    fn snap(dir: &Path) -> Snapshot {
        Snapshot::capture(&ScanTree::build(
            dir,
            &Options::default(),
            &Progress::default(),
        ))
    }

    #[test]
    fn capture_save_load_roundtrip() {
        let d = tmpdir("roundtrip");
        write(&d.join("a/1.bin"), &[0u8; 100]);
        write(&d.join("a/b/2.bin"), &[0u8; 50]);
        write(&d.join("a b/3.bin"), &[0u8; 10]);
        let s = snap(&d);
        assert_eq!(s.format, SNAPSHOT_FORMAT);
        assert!(s.taken_at > 1_600_000_000);
        assert_eq!(s.root, d);
        assert_eq!(s.total_files, 3);
        assert_eq!(s.total_dirs, 3);
        assert!(s.has_files);
        assert_eq!(s.entries, 6); // 3 dirs + 3 files
        let dirs = s.dirs().unwrap();
        assert_eq!(dirs.len(), 3);
        // A folder's contents follow it directly ("a/b" before "a b").
        assert_eq!(dirs[0].path, d.join("a"));
        assert_eq!(dirs[1].path, d.join("a").join("b"));
        assert_eq!(dirs[2].path, d.join("a b"));
        assert_eq!(dirs[0].file_count, 2);

        let file = d.join("history/snap.dgs");
        s.save(&file).unwrap();
        assert!(!file.with_extension("tmp").exists());
        assert_eq!(std::fs::metadata(&file).unwrap().len(), s.stored_size());
        let back = Snapshot::load(&file).unwrap();
        assert_eq!(back, s);

        // Damaged or foreign files are reported, not silently accepted.
        std::fs::write(&file, br#"{"format": 99}"#).unwrap();
        assert!(matches!(
            Snapshot::load(&file),
            Err(SnapshotError::Format(99))
        ));
        std::fs::write(&file, b"DGSNAP07rest").unwrap();
        assert!(matches!(
            Snapshot::load(&file),
            Err(SnapshotError::Format(7))
        ));
        std::fs::write(&file, b"{ not json").unwrap();
        assert!(matches!(Snapshot::load(&file), Err(SnapshotError::Json(_))));
        std::fs::write(&file, b"garbage").unwrap();
        assert!(matches!(
            Snapshot::load(&file),
            Err(SnapshotError::Corrupt(_))
        ));
        let mut cut = MAGIC.to_vec();
        cut.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&file, cut).unwrap();
        assert!(matches!(
            Snapshot::load(&file),
            Err(SnapshotError::Corrupt(_))
        ));
        assert!(matches!(
            Snapshot::load(&d.join("missing.dgs")),
            Err(SnapshotError::Io(_))
        ));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn diff_reports_grown_shrunk_added_removed() {
        let d = tmpdir("diff");
        write(&d.join("grow/1.bin"), &[0u8; 100]);
        write(&d.join("shrink/1.bin"), &[0u8; 100]);
        write(&d.join("same/1.bin"), &[0u8; 100]);
        write(&d.join("gone/1.bin"), &[0u8; 100]);
        let old = snap(&d);

        write(&d.join("grow/2.bin"), &[0u8; 100_000]);
        std::fs::remove_file(d.join("shrink/1.bin")).unwrap();
        std::fs::remove_dir_all(d.join("gone")).unwrap();
        write(&d.join("fresh/1.bin"), &[0u8; 100]);
        let new = snap(&d);

        let r = diff(&old, &new, &d).unwrap();
        assert_eq!(r.root, d);
        assert_eq!(r.old_taken_at, old.taken_at);
        assert_eq!(r.new_taken_at, new.taken_at);
        assert_eq!(r.old_files, 4);
        assert_eq!(r.new_files, 4);
        assert_eq!(r.files_delta, 0);
        assert_eq!(r.delta, signed(new.total_size) - signed(old.total_size));
        assert!(r.delta > 0);
        assert!(!r.is_unchanged());

        assert_eq!(r.grown.len(), 1);
        assert_eq!(r.grown[0].path, d.join("grow"));
        assert_eq!(r.grown[0].old_files, 1);
        assert_eq!(r.grown[0].new_files, 2);
        assert_eq!(r.shrunk.len(), 1);
        assert_eq!(r.shrunk[0].path, d.join("shrink"));
        assert!(r.shrunk[0].delta < 0);
        assert_eq!(r.shrunk[0].new_files, 0);
        assert_eq!(r.added.len(), 1);
        assert_eq!(r.added[0].path, d.join("fresh"));
        assert_eq!(r.removed.len(), 1);
        assert_eq!(r.removed[0].path, d.join("gone"));

        // File by file: two new, two gone, none resized.
        assert!(r.files_compared);
        assert_eq!(r.new_files_count, 2);
        assert_eq!(r.removed_files_count, 2);
        assert_eq!(r.changed_files_count, 0);
        assert_eq!(r.new_files_list[0].path, d.join("grow").join("2.bin"));
        assert_eq!(r.new_files_list[0].old_size, 0);
        assert!(r
            .removed_files_list
            .iter()
            .any(|f| f.path == d.join("gone").join("1.bin")));
        assert_eq!(
            r.new_files_bytes,
            r.new_files_list.iter().map(|f| f.new_size).sum::<u64>()
        );

        // Identical snapshots: nothing to report.
        let none = diff(&new, &new, &d).unwrap();
        assert!(none.is_unchanged());

        // Truncation caps every list.
        let mut capped = r.clone();
        capped.truncate(0);
        assert!(capped.grown.is_empty() && capped.removed.is_empty());
        assert!(capped.new_files_list.is_empty());
        assert_eq!(capped.new_files_count, 2);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn diff_finds_resized_files_and_kind_changes() {
        let d = tmpdir("resize");
        write(&d.join("a/log.txt"), &[0u8; 10_000]);
        write(&d.join("a/swap"), &[0u8; 10]);
        let old = snap(&d);
        write(&d.join("a/log.txt"), &[0u8; 900_000]);
        std::fs::remove_file(d.join("a/swap")).unwrap();
        write(&d.join("a/swap/inner.bin"), &[0u8; 10]); // a file became a folder
        let new = snap(&d);

        let r = diff(&old, &new, &d.join("a")).unwrap();
        assert_eq!(r.changed_files_count, 1);
        let c = &r.changed_files_list[0];
        assert_eq!(c.path, d.join("a").join("log.txt"));
        assert!(c.delta > 0 && c.new_size > c.old_size);
        assert_eq!(r.changed_files_delta, c.delta);
        assert_eq!(r.removed_files_list[0].path, d.join("a").join("swap"));
        assert_eq!(
            r.new_files_list[0].path,
            d.join("a").join("swap").join("inner.bin")
        );
        assert_eq!(r.added[0].path, d.join("a").join("swap"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn diff_at_a_sub_folder() {
        let d = tmpdir("diffsub");
        write(&d.join("a/x/1.bin"), &[0u8; 100]);
        write(&d.join("b/1.bin"), &[0u8; 100]);
        let old = snap(&d);
        write(&d.join("a/x/2.bin"), &[0u8; 100]);
        write(&d.join("a/y/1.bin"), &[0u8; 100]);
        write(&d.join("b/2.bin"), &[0u8; 100]); // outside "a": not reported there
        let new = snap(&d);

        let r = diff(&old, &new, &d.join("a")).unwrap();
        assert_eq!(r.root, d.join("a"));
        assert_eq!(r.old_files, 1);
        assert_eq!(r.new_files, 3);
        assert_eq!(r.files_delta, 2);
        assert_eq!(r.grown.len(), 1);
        assert_eq!(r.grown[0].path, d.join("a").join("x"));
        assert_eq!(r.added.len(), 1);
        assert_eq!(r.added[0].path, d.join("a").join("y"));
        assert!(r.shrunk.is_empty() && r.removed.is_empty());
        assert_eq!(r.new_files_count, 2);

        // A folder the old snapshot never saw counts as previously empty.
        let y = diff(&old, &new, &d.join("a").join("y")).unwrap();
        assert_eq!(y.old_size, 0);
        assert_eq!(y.old_files, 0);
        assert_eq!(y.new_files, 1);

        // But it must exist in the new one.
        assert!(matches!(
            diff(&old, &new, &d.join("nope")),
            Err(SnapshotError::NotInSnapshot(_))
        ));
        assert!(matches!(
            diff(&old, &new, Path::new("/elsewhere")),
            Err(SnapshotError::NotInSnapshot(_))
        ));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn format_1_json_snapshots_still_compare_by_folder() {
        let d = tmpdir("legacy");
        write(&d.join("a/1.bin"), &[0u8; 100]);
        let json = serde_json::json!({
            "format": 1, "taken_at": 1_600_000_000u64, "root": d,
            "total_size": 100, "total_files": 1, "total_dirs": 1,
            "dirs": [{"path": d.join("a"), "size": 100, "file_count": 1}],
            "top_files": [],
        });
        let file = d.join("old.json");
        std::fs::write(&file, serde_json::to_vec(&json).unwrap()).unwrap();
        let old = Snapshot::load(&file).unwrap();
        assert_eq!(old.format, 1);
        assert!(!old.has_files);
        assert_eq!(old.dirs().unwrap()[0].path, d.join("a"));

        write(&d.join("a/2.bin"), &[0u8; 100_000]);
        let r = diff(&old, &snap(&d), &d).unwrap();
        assert_eq!(r.grown[0].path, d.join("a"));
        // Without files on the old side, no file is claimed as new.
        assert!(!r.files_compared);
        assert_eq!(r.new_files_count, 0);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn a_snapshot_rebuilds_the_same_tree() {
        let d = tmpdir("totree");
        write(&d.join("a/x.TXT"), &[0u8; 100]);
        write(&d.join("a/b/y.bin"), &[0u8; 50_000]);
        write(&d.join("a b/z"), &[0u8; 10]);
        write(&d.join("top.mkv"), &[0u8; 20_000]);
        std::fs::create_dir_all(d.join("empty")).unwrap();
        let tree = ScanTree::build(&d, &Options::default(), &Progress::default());
        let back = Snapshot::capture(&tree).to_tree().unwrap();

        let (r1, r2) = (tree.root_report(10), back.root_report(10));
        assert_eq!(r2.total_size, r1.total_size);
        assert_eq!(r2.total_files, r1.total_files);
        assert_eq!(r2.total_dirs, r1.total_dirs);
        assert_eq!(r2.children, r1.children);
        assert_eq!(r2.root_files_size, r1.root_files_size);
        let paths = |r: &crate::ScanReport| {
            r.top_files
                .iter()
                .map(|f| f.path.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&r2), paths(&r1));
        assert_eq!(back.file_stats(&d), tree.file_stats(&d));
        assert_eq!(
            back.report(&d.join("a").join("b"), 5).unwrap().total_files,
            1
        );
        assert!(back.contains(&d.join("empty")));

        // Folder-only (format 1) snapshots cannot become a tree.
        let mut old = Snapshot::capture(&tree);
        old.has_files = false;
        assert!(old.to_tree().is_err());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn diff_refuses_different_roots() {
        let d1 = tmpdir("root1");
        let d2 = tmpdir("root2");
        let err = diff(&snap(&d1), &snap(&d2), &d1).unwrap_err();
        assert!(matches!(err, SnapshotError::RootMismatch { .. }));
        assert!(err.to_string().contains("different directories"));
        std::fs::remove_dir_all(&d1).ok();
        std::fs::remove_dir_all(&d2).ok();
    }

    #[test]
    fn keys_sort_folder_contents_right_after_the_folder() {
        let mut keys: Vec<&[u8]> = vec![b"a b", b"a/z", b"a", b"a-b", b"b", b"a/b/c"];
        keys.sort_by(|a, b| cmp_key(a, b));
        assert_eq!(
            keys,
            vec![&b"a"[..], b"a/b/c", b"a/z", b"a b", b"a-b", b"b"]
        );
        let mut buf = Vec::new();
        for v in [0u64, 127, 128, 300, u64::MAX] {
            buf.clear();
            put_varint(&mut buf, v);
            assert_eq!(get_varint(&mut &buf[..]).unwrap(), v);
        }
    }

    #[test]
    fn error_messages_are_descriptive() {
        assert!(SnapshotError::Format(7).to_string().contains("format 7"));
        assert!(SnapshotError::NotInSnapshot(PathBuf::from("x"))
            .to_string()
            .contains("not a directory"));
        assert!(SnapshotError::Corrupt("x".into())
            .to_string()
            .contains("invalid"));
        let io: SnapshotError = std::io::Error::other("boom").into();
        assert_eq!(io.to_string(), "boom");
        let json: SnapshotError = serde_json::from_str::<serde_json::Value>("{")
            .unwrap_err()
            .into();
        assert!(json.to_string().starts_with("invalid snapshot:"));
        assert!(std::error::Error::source(&json).is_none());
    }

    #[test]
    fn root_key_is_stable_and_short() {
        let a = Snapshot::root_key(Path::new("/some/dir"));
        assert_eq!(a.len(), 16);
        assert_eq!(a, Snapshot::root_key(Path::new("/some/dir")));
        assert_ne!(a, Snapshot::root_key(Path::new("/other/dir")));
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn timestamps_render_as_utc() {
        assert_eq!(format_timestamp(0), "1970-01-01 00:00 UTC");
        assert_eq!(format_timestamp(1_600_000_000), "2020-09-13 12:26 UTC");
        assert_eq!(format_timestamp(951_782_400), "2000-02-29 00:00 UTC");
        assert_eq!(format_timestamp(1_704_067_199), "2023-12-31 23:59 UTC");
    }
}
