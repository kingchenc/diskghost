//! Scanning an NTFS volume by reading its Master File Table directly.
//!
//! Listing a directory costs Windows an open, a query and a close; on a system
//! drive with millions of folders that — not the disk — is the limit. Every
//! file and folder of an NTFS volume is one 1 KB record in the MFT, so reading
//! that table sequentially in big blocks (and parsing the records in parallel)
//! yields the whole tree in a few seconds. It needs administrator rights (the
//! volume itself is opened) and works on local NTFS volumes only; callers fall
//! back to the directory walk otherwise.
//!
//! What is counted matches the walk: the unnamed data stream of each file
//! (logical size, or the allocated size for sparse / compressed / cloud files),
//! one entry per hard link, symbolic links and junctions skipped, NTFS
//! metafiles (`$MFT`, `$Extend`, …) left out.

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::sync::atomic::Ordering;

use rayon::prelude::*;

use crate::walk::Batch;
use crate::{build_globset, is_excluded, FileEntry, Options, Progress};

/// Record number of the volume's root directory.
const ROOT: u64 = 5;
/// Records below this are NTFS metafiles (except the root).
const FIRST_USER_RECORD: u64 = 24;
/// Bytes read from the volume per request.
const CHUNK: u64 = 8 << 20;

const ATTR_STANDARD_INFORMATION: u32 = 0x10;
const ATTR_FILE_NAME: u32 = 0x30;
const ATTR_DATA: u32 = 0x80;
const ATTR_REPARSE_POINT: u32 = 0xC0;
const ATTR_END: u32 = 0xFFFF_FFFF;

/// File attributes that make the on-disk size differ from the logical size
/// (the same set the walk asks `GetCompressedFileSizeW` about).
const WEIRD: u32 = 0x200 | 0x400 | 0x800 | 0x1000 | 0x4_0000 | 0x40_0000;

/// Why the MFT could not be used.
#[derive(Debug)]
pub(crate) enum MftError {
    /// Not a local drive letter path.
    NotADrive,
    /// The volume could not be opened (usually: not running as administrator).
    Open(std::io::Error),
    /// Not NTFS, or the table could not be understood.
    NotNtfs(&'static str),
    Io(std::io::Error),
}

impl std::fmt::Display for MftError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MftError::NotADrive => write!(f, "not a local drive"),
            MftError::Open(e) => write!(
                f,
                "cannot open the volume, administrator rights needed ({e})"
            ),
            MftError::NotNtfs(why) => write!(f, "{why}"),
            MftError::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl From<std::io::Error> for MftError {
    fn from(e: std::io::Error) -> Self {
        MftError::Io(e)
    }
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

/// The drive letter of `root` (`C:\Users` -> `C`).
fn drive_letter(root: &Path) -> Option<char> {
    match root.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d as char),
            _ => None,
        },
        _ => None,
    }
}

/// Geometry from the NTFS boot sector.
struct Geometry {
    cluster: u64,
    record: u64,
    mft_lcn: u64,
}

fn geometry(boot: &[u8]) -> Result<Geometry, MftError> {
    if boot.get(3..11) != Some(b"NTFS    ") {
        return Err(MftError::NotNtfs("not an NTFS volume"));
    }
    let sector = u64::from(u16_at(boot, 0x0B).unwrap_or(0));
    let per_cluster = u64::from(boot[0x0D]);
    let cluster = sector * per_cluster;
    let mft_lcn = u64_at(boot, 0x30).unwrap_or(0);
    let raw = boot[0x40] as i8;
    let record = if raw < 0 {
        1u64 << (-raw as u32)
    } else {
        raw as u64 * cluster
    };
    if cluster == 0 || !(512..=65536).contains(&record) || mft_lcn == 0 {
        return Err(MftError::NotNtfs("unexpected boot sector"));
    }
    Ok(Geometry {
        cluster,
        record,
        mft_lcn,
    })
}

/// Undo the update-sequence protection of one record in place. `false` if
/// the record is torn or not a FILE record.
fn fixup(rec: &mut [u8]) -> bool {
    if rec.get(0..4) != Some(b"FILE") {
        return false;
    }
    let (Some(off), Some(count)) = (u16_at(rec, 4), u16_at(rec, 6)) else {
        return false;
    };
    let (off, count) = (off as usize, count as usize);
    let Some(usn) = u16_at(rec, off) else {
        return false;
    };
    for i in 1..count {
        let end = i * 512;
        if end > rec.len() || u16_at(rec, end - 2) != Some(usn) {
            return false;
        }
        let Some(fix) = u16_at(rec, off + 2 * i) else {
            return false;
        };
        rec[end - 2..end].copy_from_slice(&fix.to_le_bytes());
    }
    true
}

/// One attribute of a record.
struct Attr<'a> {
    kind: u32,
    named: bool,
    flags: u16,
    /// Resident value, or `None` for a non-resident attribute.
    value: Option<&'a [u8]>,
    /// Whole attribute (for non-resident fields).
    raw: &'a [u8],
}

fn attrs(rec: &[u8]) -> impl Iterator<Item = Attr<'_>> {
    let mut off = u16_at(rec, 0x14).map_or(usize::MAX, usize::from);
    let used = u32_at(rec, 0x18).map_or(0, |u| u as usize).min(rec.len());
    std::iter::from_fn(move || {
        let kind = u32_at(rec, off)?;
        if kind == ATTR_END || off >= used {
            return None;
        }
        let len = u32_at(rec, off + 4)? as usize;
        if len < 0x18 || off + len > used {
            return None;
        }
        let raw = &rec[off..off + len];
        off += len;
        let non_resident = raw[8] != 0;
        let value = if non_resident {
            None
        } else {
            let vlen = u32_at(raw, 0x10)? as usize;
            let voff = u16_at(raw, 0x14)? as usize;
            Some(raw.get(voff..voff + vlen)?)
        };
        Some(Attr {
            kind,
            named: raw[9] != 0,
            flags: u16_at(raw, 0x0C)?,
            value,
            raw,
        })
    })
}

/// Data runs of a non-resident attribute as (first cluster, cluster count).
fn runs(raw: &[u8]) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let Some(mut p) = u16_at(raw, 0x20).map(usize::from) else {
        return out;
    };
    let mut lcn: i64 = 0;
    while let Some(&head) = raw.get(p) {
        if head == 0 {
            break;
        }
        let (len_size, off_size) = ((head & 0x0F) as usize, (head >> 4) as usize);
        let Some(bytes) = raw.get(p + 1..p + 1 + len_size + off_size) else {
            break;
        };
        let mut length = 0u64;
        for (i, b) in bytes[..len_size].iter().enumerate() {
            length |= u64::from(*b) << (8 * i);
        }
        if off_size > 0 {
            let mut delta = 0i64;
            for (i, b) in bytes[len_size..].iter().enumerate() {
                delta |= i64::from(*b) << (8 * i);
            }
            // Sign-extend the delta.
            let shift = 64 - 8 * off_size as u32;
            delta = (delta << shift) >> shift;
            lcn += delta;
            out.push((lcn as u64, length));
        } // a run without offset is sparse: not part of the table
        p += 1 + len_size + off_size;
    }
    out
}

/// What one record (or an extension record of it) contributes.
#[derive(Default)]
struct Rec {
    /// Base record this belongs to (itself for a base record).
    base: u64,
    is_dir: bool,
    /// (parent record, name) per non-DOS file name — several for hard links.
    names: Vec<(u64, OsString)>,
    attributes: u32,
    modified: u64,
    /// A symbolic link, junction or other name surrogate: never followed.
    surrogate: bool,
    /// Any reparse point (cloud placeholders included): its on-disk size is
    /// the allocated one, like the walk's `GetCompressedFileSizeW`.
    reparse: bool,
    /// From the unnamed data stream's first extent: (logical, allocated).
    data: Option<(u64, u64)>,
}

/// Parse one fixed-up record.
fn parse(index: u64, rec: &[u8]) -> Option<Rec> {
    let flags = u16_at(rec, 0x16)?;
    if flags & 1 == 0 {
        return None; // not in use
    }
    let base = u64_at(rec, 0x20)? & 0xFFFF_FFFF_FFFF;
    let mut r = Rec {
        base: if base == 0 { index } else { base },
        is_dir: flags & 2 != 0,
        ..Rec::default()
    };
    let mut dos_names: Vec<(u64, OsString)> = Vec::new();
    let mut units: Vec<u16> = Vec::new();
    for a in attrs(rec) {
        match a.kind {
            ATTR_STANDARD_INFORMATION => {
                if let Some(v) = a.value {
                    // FILETIME (100 ns since 1601) -> ns since 1970.
                    let ft = u64_at(v, 0x08).unwrap_or(0);
                    r.modified = ft
                        .saturating_sub(116_444_736_000_000_000)
                        .saturating_mul(100);
                    r.attributes = u32_at(v, 0x20).unwrap_or(0);
                }
            }
            ATTR_FILE_NAME => {
                let Some(v) = a.value else { continue };
                let parent = u64_at(v, 0)? & 0xFFFF_FFFF_FFFF;
                let len = *v.get(0x40)? as usize;
                let space = *v.get(0x41)?;
                units.clear();
                units.extend(
                    v.get(0x42..0x42 + 2 * len)?
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]])),
                );
                let name = OsString::from_wide(&units);
                if space == 2 {
                    dos_names.push((parent, name)); // 8.3 alias of a long name
                } else {
                    r.names.push((parent, name));
                }
            }
            ATTR_DATA if !a.named => {
                let size = match a.value {
                    Some(v) => Some((v.len() as u64, v.len() as u64)),
                    // Sizes are only valid in the extent starting at VCN 0.
                    None if u64_at(a.raw, 0x10) == Some(0) => {
                        let logical = u64_at(a.raw, 0x30)?;
                        let allocated = u64_at(a.raw, 0x28)?;
                        let compressed_or_sparse = a.flags & 0x0001 != 0 || a.flags & 0x8000 != 0;
                        let on_disk = if compressed_or_sparse {
                            u64_at(a.raw, 0x40).unwrap_or(allocated)
                        } else {
                            allocated
                        };
                        Some((logical, on_disk))
                    }
                    None => None,
                };
                if size.is_some() {
                    r.data = size;
                }
            }
            ATTR_REPARSE_POINT => {
                let tag = a.value.and_then(|v| u32_at(v, 0)).unwrap_or(0);
                r.reparse = true;
                r.surrogate = tag & 0x2000_0000 != 0; // symlink, junction, …
            }
            _ => {}
        }
    }
    if r.names.is_empty() {
        r.names = dos_names; // a name that only exists in 8.3 form
    }
    Some(r)
}

/// A merged base record.
struct Node {
    is_dir: bool,
    names: Vec<(u64, OsString)>,
    modified: u64,
    surrogate: bool,
    /// A reparse point that is not a link — a cloud placeholder (OneDrive …):
    /// only its filter driver knows what is stored locally.
    cloud: bool,
    /// (on-disk, logical) size of the unnamed data stream.
    size: (u64, u64),
}

/// Prints phase timings to stderr when `DISKGHOST_TRACE` is set.
struct Trace(Option<std::time::Instant>);

impl Trace {
    fn new() -> Trace {
        Trace(std::env::var_os("DISKGHOST_TRACE").map(|_| std::time::Instant::now()))
    }
    fn mark(&mut self, what: &str) {
        if let Some(t) = self.0 {
            eprintln!(
                "mft: {what:<28} {:>8.0} ms",
                t.elapsed().as_secs_f64() * 1000.0
            );
            self.0 = Some(std::time::Instant::now());
        }
    }
}

/// A zeroed, page-aligned buffer (volume reads must be sector-aligned).
struct Aligned {
    buf: Vec<u8>,
    start: usize,
    len: usize,
}

impl Aligned {
    fn new(len: usize) -> Aligned {
        let buf = vec![0u8; len + 4096];
        let start = buf.as_ptr().align_offset(4096);
        Aligned { buf, start, len }
    }
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buf[self.start..self.start + self.len]
    }
}

/// Volume handles read from in parallel: synchronous reads on one handle are
/// served one after another, so each reader thread gets its own.
const HANDLES: usize = 4;

/// Read and parse the whole MFT of the volume holding `root`; the result is
/// indexed by record number.
fn read_table(
    root: &Path,
    progress: &Progress,
    trace: &mut Trace,
) -> Result<Vec<Option<Node>>, MftError> {
    let letter = drive_letter(root).ok_or(MftError::NotADrive)?;
    // Buffered on purpose: reading without the cache was measured to be no
    // faster the first time, and through it a repeated scan comes from RAM.
    let open = || {
        std::fs::OpenOptions::new()
            .read(true)
            .share_mode(7) // read | write | delete: the volume stays usable
            .open(format!(r"\\.\{letter}:"))
            .map_err(MftError::Open)
    };
    let volume = open()?;
    trace.mark("open volume");

    let mut boot = Aligned::new(4096);
    volume.seek_read(boot.as_mut(), 0)?;
    let g = geometry(boot.as_mut())?;

    // Record 0 describes the MFT itself: where its pieces are on the volume.
    let mut first_buf = Aligned::new(g.record.max(g.cluster).max(4096) as usize);
    volume.seek_read(first_buf.as_mut(), g.mft_lcn * g.cluster)?;
    let first = &mut first_buf.as_mut()[..g.record as usize];
    if !fixup(first) {
        return Err(MftError::NotNtfs("unreadable $MFT record"));
    }
    let mut table_size = 0;
    let mut extents = Vec::new();
    for a in attrs(first) {
        if a.kind == ATTR_DATA && !a.named && a.value.is_none() {
            if u64_at(a.raw, 0x10) == Some(0) {
                table_size = u64_at(a.raw, 0x30).unwrap_or(0);
            }
            extents.extend(runs(a.raw));
        }
    }
    if extents.is_empty() || table_size == 0 {
        return Err(MftError::NotNtfs("no $MFT data runs"));
    }

    // Byte ranges of the table on the volume, cut into chunks; each chunk
    // knows the record number it starts with.
    let mut chunks: Vec<(u64, u64, u64)> = Vec::new(); // (volume offset, bytes, first record)
    let mut record_no = 0u64;
    let mut left = table_size;
    for (lcn, clusters) in extents {
        let mut off = lcn * g.cluster;
        let mut bytes = (clusters * g.cluster).min(left);
        left -= bytes;
        while bytes > 0 {
            let n = bytes.min(CHUNK);
            chunks.push((off, n, record_no));
            record_no += n / g.record;
            off += n;
            bytes -= n;
        }
        if left == 0 {
            break;
        }
    }
    let mut handles = vec![volume];
    for _ in 1..HANDLES {
        handles.push(open()?);
    }
    trace.mark("boot sector + other handles");

    let record = g.record as usize;
    let parsed: Vec<Vec<(u64, Rec)>> = chunks
        .par_iter()
        .enumerate()
        .map(|(c, &(off, n, first_no))| {
            if progress.cancelled() {
                return Ok(Vec::new());
            }
            let volume = &handles[c % HANDLES];
            // Whole clusters (the table may end mid-cluster).
            let len = n.div_ceil(g.cluster) * g.cluster;
            let mut buf = Aligned::new(len as usize);
            let buf = buf.as_mut();
            let mut got = 0;
            while got < buf.len() {
                let r = volume.seek_read(&mut buf[got..], off + got as u64)?;
                if r == 0 {
                    break;
                }
                got += r;
            }
            let got = got.min(n as usize);
            let mut out = Vec::with_capacity(got / record);
            for (i, rec) in buf[..got].chunks_exact_mut(record).enumerate() {
                let index = first_no + i as u64;
                if fixup(rec) {
                    if let Some(r) = parse(index, rec) {
                        out.push((index, r));
                    }
                }
            }
            progress
                .files
                .fetch_add(out.len() as u64, Ordering::Relaxed);
            Ok(out)
        })
        .collect::<Result<_, std::io::Error>>()?;
    trace.mark(&format!("read + parse {} MB", table_size >> 20));

    // Merge extension records into their base records.
    let mut nodes: Vec<Option<Node>> = Vec::new();
    nodes.resize_with(record_no as usize, || None);
    let mut extensions = Vec::new();
    for (index, r) in parsed.into_iter().flatten() {
        if r.base != index {
            extensions.push(r);
            continue;
        }
        let Some(slot) = nodes.get_mut(index as usize) else {
            continue;
        };
        *slot = Some(Node {
            is_dir: r.is_dir,
            names: r.names,
            modified: r.modified,
            surrogate: r.surrogate,
            cloud: r.reparse && !r.surrogate,
            size: size_of(r.data, r.attributes, r.reparse),
        });
    }
    for r in extensions {
        if let Some(Some(n)) = nodes.get_mut(r.base as usize) {
            n.names.extend(r.names);
            if r.data.is_some() && n.size == (0, 0) {
                n.size = size_of(r.data, r.attributes, r.reparse);
            }
            n.surrogate |= r.surrogate;
            n.cloud |= r.reparse && !r.surrogate;
        }
    }
    trace.mark("merge records");
    Ok(nodes)
}

/// (on-disk, logical) size like the walk reports it: the logical length,
/// except where it differs from what the disk holds (sparse, compressed,
/// cloud placeholders and other reparse points) — then the allocated size.
fn size_of(data: Option<(u64, u64)>, attributes: u32, reparse: bool) -> (u64, u64) {
    let (logical, allocated) = data.unwrap_or((0, 0));
    if attributes & WEIRD != 0 || reparse {
        (allocated, logical)
    } else {
        (logical, logical)
    }
}

/// One directory entry: (parent record, child record, which of the child's
/// names).
pub(crate) type Link = (u32, u32, u32);

/// The parsed table of a volume plus a parent -> children index.
pub(crate) struct Table {
    nodes: Vec<Option<Node>>,
    /// Folders synced by OneDrive: its filter hides and resizes files there,
    /// so they are listed through Windows like other cloud folders.
    synced: Vec<u64>,
    /// Every entry, sorted by parent: a directory's entries are one slice.
    links: Vec<Link>,
    trace: Trace,
}

/// What a tree needs to know about one entry.
pub(crate) struct Entry<'a> {
    pub(crate) record: u64,
    pub(crate) name: &'a OsString,
    pub(crate) is_dir: bool,
    /// A cloud placeholder (file or folder): sizes and contents must be
    /// asked from Windows.
    pub(crate) cloud: bool,
    pub(crate) size: u64,
    pub(crate) logical: u64,
    pub(crate) modified: u64,
}

impl Table {
    /// Read the MFT of the volume holding `root`.
    pub(crate) fn read(root: &Path, progress: &Progress) -> Result<Table, MftError> {
        let mut trace = Trace::new();
        let nodes = read_table(root, progress, &mut trace)?;
        let mut links: Vec<Link> = Vec::new();
        for (index, n) in nodes.iter().enumerate() {
            let Some(n) = n else { continue };
            let index = index as u64;
            if (index < FIRST_USER_RECORD && index != ROOT) || n.surrogate {
                continue;
            }
            for (k, (parent, _)) in n.names.iter().enumerate() {
                if *parent != index && *parent <= u64::from(u32::MAX) {
                    links.push((*parent as u32, index as u32, k as u32));
                }
            }
        }
        links.par_sort_unstable_by_key(|l| l.0);
        trace.mark("index children");
        let mut table = Table {
            nodes,
            synced: Vec::new(),
            links,
            trace,
        };
        let letter = drive_letter(root);
        for var in ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"] {
            let Some(dir) = std::env::var_os(var).map(PathBuf::from) else {
                continue;
            };
            if drive_letter(&dir).map(|d| d.to_ascii_uppercase())
                == letter.map(|d| d.to_ascii_uppercase())
            {
                if let Ok(record) = table.find(&dir) {
                    table.synced.push(record);
                }
            }
        }
        Ok(table)
    }

    /// The entries of directory `dir`.
    pub(crate) fn children(&self, dir: u64) -> impl Iterator<Item = Entry<'_>> {
        let lo = self.links.partition_point(|l| u64::from(l.0) < dir);
        let hi = self.links.partition_point(|l| u64::from(l.0) <= dir);
        self.links[lo..hi].iter().filter_map(|l| {
            let n = self.nodes[l.1 as usize].as_ref()?;
            Some(Entry {
                record: u64::from(l.1),
                name: &n.names[l.2 as usize].1,
                is_dir: n.is_dir,
                cloud: n.cloud || self.synced.contains(&u64::from(l.1)),
                size: n.size.0,
                logical: n.size.1,
                modified: n.modified,
            })
        })
    }

    /// The record of the folder `root` names (matched without regard to
    /// case, like Windows does) and its path as spelled on disk.
    pub(crate) fn find(&self, root: &Path) -> Result<u64, MftError> {
        let mut at = ROOT;
        for c in root.components() {
            match c {
                Component::Prefix(_) | Component::RootDir => {}
                Component::Normal(want) => {
                    let want = want.to_string_lossy().to_lowercase();
                    let next = self
                        .children(at)
                        .find(|e| e.is_dir && e.name.to_string_lossy().to_lowercase() == want);
                    at = next
                        .ok_or(MftError::NotNtfs("folder not found in the MFT"))?
                        .record;
                }
                _ => return Err(MftError::NotADrive),
            }
        }
        Ok(at)
    }

    /// Whether `record` is a cloud placeholder or a OneDrive-synced folder.
    pub(crate) fn is_cloud(&self, record: u64) -> bool {
        self.synced.contains(&record)
            || self
                .nodes
                .get(record as usize)
                .and_then(Option::as_ref)
                .is_some_and(|n| n.cloud)
    }

    pub(crate) fn mark(&mut self, what: &str) {
        self.trace.mark(what);
    }
}

/// Walk `root` from the MFT, handing one [`Batch`] per directory to `sink` —
/// the same contract as [`crate::walk::walk`], for callers that need paths
/// (exclusions, duplicate search). Honours exclusions and max depth (links
/// are never followed); cloud folders are listed the ordinary way.
pub(crate) fn walk(
    root: &Path,
    opts: &Options,
    progress: &Progress,
    sink: &mut dyn FnMut(Batch),
) -> Result<(), MftError> {
    let mut table = Table::read(root, progress)?;
    if progress.cancelled() {
        return Ok(());
    }
    let start = table.find(root)?;
    if table.is_cloud(start) {
        // The folder itself is synced: all of it through Windows.
        walk_cloud(&[(root.to_path_buf(), 0)], opts, progress, sink);
        return Ok(());
    }

    let glob = build_globset(&opts.exclude);
    let mut cloud_dirs: Vec<(PathBuf, usize)> = Vec::new();
    let mut queue = vec![(start, root.to_path_buf(), 0usize)];
    let mut bytes = 0u64;
    while let Some((dir, path, depth)) = queue.pop() {
        if progress.cancelled() {
            return Ok(());
        }
        let mut b = Batch {
            dir: path.clone(),
            files: Vec::new(),
            modified: Vec::new(),
            subdirs: Vec::new(),
            others: Vec::new(),
            skipped: 0,
        };
        let child_depth = depth + 1;
        if opts.max_depth.is_some_and(|m| child_depth > m) {
            sink(b);
            continue;
        }
        let descend = opts.max_depth.map_or(true, |m| child_depth < m);
        for e in table.children(dir) {
            let p = path.join(e.name);
            if !glob.is_empty() && is_excluded(&glob, root, &p) {
                continue;
            }
            if e.is_dir {
                if descend {
                    if e.cloud {
                        // A cloud folder: list it the ordinary way, which
                        // asks the cloud filter what really is there.
                        cloud_dirs.push((p.clone(), child_depth));
                    } else {
                        queue.push((e.record, p.clone(), child_depth));
                    }
                }
                b.subdirs.push(p);
            } else {
                let size = if e.cloud {
                    crate::compressed_size(&p).unwrap_or(e.size)
                } else {
                    e.size
                };
                bytes += size;
                b.files.push(FileEntry {
                    path: p,
                    size,
                    logical_size: e.logical,
                });
                b.modified.push(e.modified);
            }
        }
        progress.bytes.store(bytes, Ordering::Relaxed);
        sink(b);
    }
    table.mark("emit directories");
    walk_cloud(&cloud_dirs, opts, progress, sink);
    table.mark("walk cloud folders");
    Ok(())
}

/// List cloud folders found in the MFT the ordinary way, carrying the depth
/// limit on.
pub(crate) fn walk_cloud(
    dirs: &[(PathBuf, usize)],
    opts: &Options,
    progress: &Progress,
    sink: &mut dyn FnMut(Batch),
) {
    for (dir, depth) in dirs {
        if progress.cancelled() {
            break;
        }
        let local = Options {
            mft: false,
            max_depth: opts.max_depth.map(|m| m.saturating_sub(*depth)),
            ..opts.clone()
        };
        if local.max_depth == Some(0) {
            continue;
        }
        let _ = crate::walk::walk_dyn(dir, &local, progress, sink);
    }
}
