//! Duplicate detection: files with byte-identical content.
//!
//! 1. The walk lists every file at least `min_size` bytes long; only sizes
//!    that occur more than once can hold duplicates.
//! 2. Every same-size candidate is opened once — in parallel, file by file
//!    — to learn its identity (so hard links to one physical file are never
//!    called duplicates) and to hash its first and last 16 KB. Files up to
//!    32 KB are thereby hashed completely.
//! 3. Only files whose size and both ends match another one are hashed in
//!    full with BLAKE3, again in parallel per file (big files additionally
//!    use several threads each, through a memory map).
//!
//! A [`HashCache`] remembers identity and hashes by path, size and
//! modification time, so searching the same place again reads only files
//! that changed.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;

use rayon::prelude::*;

use crate::snapshot::{get_varint, put_varint};
use crate::walk::walk;
use crate::{phase, DupGroup, Options, Progress};

/// Folders a duplicate search never enters (below its root): the recycle bin
/// and trash of each platform, and Windows' restore-point store. Files there
/// are copies of something that was just deleted, not duplicates to clean up.
pub const TRASH_DIRS: [&str; 6] = [
    "$Recycle.Bin",
    "RECYCLER",
    "System Volume Information",
    ".Trash",
    ".Trashes",
    ".Trash-*",
];

/// Bytes read from each end of a candidate for the quick comparison.
const SAMPLE: u64 = 16 * 1024;

/// Files at least this big are hashed with several threads each.
const PARALLEL_HASH_MIN: u64 = 64 << 20;

const CACHE_MAGIC: &[u8; 8] = b"DGHASH01";

/// What is known about one file's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Known {
    size: u64,
    /// Modification time (ns since the epoch); a cached entry only counts
    /// while size and time are unchanged.
    modified: u64,
    /// (volume, file index) on Windows, (device, inode) elsewhere.
    id: (u64, u64),
    /// BLAKE3 of the first and last [`SAMPLE`] bytes.
    sample: [u8; 32],
    /// BLAKE3 of the whole file, once computed.
    full: Option<[u8; 32]>,
}

/// Hashes of files seen by earlier duplicate searches, keyed by path. Share
/// one between searches (it is internally synchronised) and persist it with
/// [`HashCache::save`] / [`HashCache::load`].
#[derive(Default)]
pub struct HashCache {
    entries: Mutex<HashMap<PathBuf, Known>>,
}

impl HashCache {
    /// An empty cache.
    pub fn new() -> HashCache {
        HashCache::default()
    }

    /// Number of files the cache knows.
    pub fn len(&self) -> usize {
        self.entries.lock().map_or(0, |e| e.len())
    }

    /// Whether the cache knows no file.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, path: &Path, size: u64, modified: u64) -> Option<Known> {
        if modified == 0 {
            return None; // no reliable time: always look at the file
        }
        let e = self.entries.lock().ok()?;
        e.get(path)
            .filter(|k| k.size == size && k.modified == modified)
            .copied()
    }

    /// Replace everything known below `root` with `fresh` — files that are
    /// gone or changed drop out, entries elsewhere stay.
    fn replace_under(&self, root: &Path, fresh: Vec<(PathBuf, Known)>) {
        if let Ok(mut e) = self.entries.lock() {
            e.retain(|p, _| !p.starts_with(root));
            e.extend(fresh);
        }
    }

    /// Read a cache written by [`HashCache::save`]. A missing, foreign or
    /// damaged file yields an empty cache: it only ever costs a re-read.
    pub fn load(path: &Path) -> HashCache {
        let cache = HashCache::new();
        if let Ok(map) = Self::read(path) {
            if let Ok(mut e) = cache.entries.lock() {
                *e = map;
            }
        }
        cache
    }

    fn read(path: &Path) -> std::io::Result<HashMap<PathBuf, Known>> {
        let bad = || std::io::Error::new(std::io::ErrorKind::InvalidData, "bad hash cache");
        let bytes = std::fs::read(path)?;
        let body = bytes.strip_prefix(CACHE_MAGIC).ok_or_else(bad)?;
        let mut r = BufReader::new(zstd::stream::read::Decoder::with_buffer(body)?);
        let n = get_varint(&mut r).map_err(|_| bad())?;
        let mut map = HashMap::with_capacity(n.min(1 << 24) as usize);
        let mut name = Vec::new();
        for _ in 0..n {
            let mut num = || get_varint(&mut r).map_err(|_| bad());
            let len = num()? as usize;
            let (size, modified, id0, id1, has_full) = (num()?, num()?, num()?, num()?, num()?);
            if len > 1 << 16 {
                return Err(bad());
            }
            name.resize(len, 0);
            r.read_exact(&mut name)?;
            let mut sample = [0u8; 32];
            r.read_exact(&mut sample)?;
            let full = if has_full == 1 {
                let mut f = [0u8; 32];
                r.read_exact(&mut f)?;
                Some(f)
            } else {
                None
            };
            let path = PathBuf::from(String::from_utf8(name.clone()).map_err(|_| bad())?);
            map.insert(
                path,
                Known {
                    size,
                    modified,
                    id: (id0, id1),
                    sample,
                    full,
                },
            );
        }
        let _ = r.fill_buf();
        Ok(map)
    }

    /// Write the cache (zstd-compressed), replacing `path` atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| std::io::Error::other("hash cache poisoned"))?;
        let mut raw = Vec::with_capacity(entries.len() * 96);
        let named: Vec<(&str, &Known)> = entries
            .iter()
            .filter_map(|(p, k)| Some((p.to_str()?, k)))
            .collect();
        put_varint(&mut raw, named.len() as u64);
        for (p, k) in named {
            for v in [
                p.len() as u64,
                k.size,
                k.modified,
                k.id.0,
                k.id.1,
                u64::from(k.full.is_some()),
            ] {
                put_varint(&mut raw, v);
            }
            raw.extend_from_slice(p.as_bytes());
            raw.extend_from_slice(&k.sample);
            if let Some(f) = k.full {
                raw.extend_from_slice(&f);
            }
        }
        drop(entries);
        let mut out = CACHE_MAGIC.to_vec();
        out.extend(zstd::bulk::compress(&raw, 3)?);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::File::create(&tmp)?.write_all(&out)?;
        std::fs::rename(tmp, path)
    }
}

/// One same-size candidate.
struct Cand {
    path: PathBuf,
    size: u64,
    modified: u64,
    known: Option<Known>,
}

/// Identity of an open file: (volume, file index) / (device, inode).
#[cfg(windows)]
fn file_id(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::windows::io::AsRawHandle;
    #[repr(C)]
    #[derive(Default)]
    struct ByHandleFileInformation {
        attributes: u32,
        creation: [u32; 2],
        access: [u32; 2],
        write: [u32; 2],
        volume_serial: u32,
        size_high: u32,
        size_low: u32,
        links: u32,
        index_high: u32,
        index_low: u32,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetFileInformationByHandle(
            file: *mut std::ffi::c_void,
            info: *mut ByHandleFileInformation,
        ) -> i32;
    }
    let mut info = ByHandleFileInformation::default();
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } != 0;
    ok.then(|| {
        (
            u64::from(info.volume_serial),
            (u64::from(info.index_high) << 32) | u64::from(info.index_low),
        )
    })
}

#[cfg(unix)]
fn file_id(file: &std::fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata().ok()?;
    Some((m.dev(), m.ino()))
}

#[cfg(not(any(windows, unix)))]
fn file_id(_file: &std::fs::File) -> Option<(u64, u64)> {
    None
}

/// Open the file once: identity plus the hash of both ends (the whole file
/// when it is small enough).
fn sample(c: &Cand) -> Option<Known> {
    let mut f = std::fs::File::open(&c.path).ok()?;
    let id = file_id(&f).unwrap_or((u64::MAX, 0));
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; SAMPLE as usize];
    let whole = c.size <= 2 * SAMPLE;
    if whole {
        let n = std::io::copy(&mut f, &mut h).ok()?;
        if n != c.size {
            return None; // changed under us
        }
    } else {
        f.read_exact(&mut buf).ok()?;
        h.update(&buf);
        f.seek(SeekFrom::Start(c.size - SAMPLE)).ok()?;
        f.read_exact(&mut buf).ok()?;
        h.update(&buf);
    }
    let sample = *h.finalize().as_bytes();
    Some(Known {
        size: c.size,
        modified: c.modified,
        id,
        sample,
        full: whole.then_some(sample),
    })
}

/// BLAKE3 of the whole file: memory-mapped (several threads for big files),
/// streamed where mapping is not possible.
fn full_hash(path: &Path, size: u64) -> Option<[u8; 32]> {
    let mut h = blake3::Hasher::new();
    let mapped = if size >= PARALLEL_HASH_MIN {
        h.update_mmap_rayon(path)
    } else {
        h.update_mmap(path)
    };
    if mapped.is_err() {
        h = blake3::Hasher::new();
        let mut f = std::fs::File::open(path).ok()?;
        std::io::copy(&mut f, &mut h).ok()?;
    }
    Some(*h.finalize().as_bytes())
}

/// See the module docs. `cache` is read and updated.
pub(crate) fn find(
    root: &Path,
    min_size: u64,
    opts: &Options,
    progress: &Progress,
    cache: &HashCache,
) -> Vec<DupGroup> {
    // 1. Every file big enough; duplicates group by *logical* (content) size.
    // Trash folders hold what was just reclaimed: never duplicates to act on.
    let opts = &Options {
        exclude: opts
            .exclude
            .iter()
            .cloned()
            .chain(TRASH_DIRS.iter().map(|d| d.to_string()))
            .collect(),
        ..opts.clone()
    };
    let mut cands: Vec<Cand> = Vec::new();
    let _ = walk(root, opts, progress, |b| {
        for (f, modified) in b.files.into_iter().zip(b.modified) {
            if f.logical_size >= min_size && f.logical_size > 0 {
                cands.push(Cand {
                    path: f.path,
                    size: f.logical_size,
                    modified,
                    known: None,
                });
            }
        }
    });
    if progress.cancelled() {
        return Vec::new();
    }
    let mut per_size: HashMap<u64, u32> = HashMap::new();
    for c in &cands {
        *per_size.entry(c.size).or_default() += 1;
    }
    cands.retain(|c| per_size[&c.size] > 1);
    drop(per_size);

    // 2. Identity and both ends, file by file in parallel (cache first).
    let sample_bytes = cands.iter().map(|c| c.size.min(2 * SAMPLE)).sum();
    progress.enter(phase::SAMPLE, cands.len() as u64, sample_bytes);
    cands.par_iter_mut().for_each(|c| {
        if progress.cancelled() {
            return;
        }
        c.known = cache.get(&c.path, c.size, c.modified).or_else(|| sample(c));
        progress.files.fetch_add(1, Ordering::Relaxed);
        progress
            .bytes
            .fetch_add(c.size.min(2 * SAMPLE), Ordering::Relaxed);
    });
    if progress.cancelled() {
        return Vec::new();
    }

    // Collapse hard links (one physical file reached by several paths), then
    // keep only files whose size and both ends match another file.
    let mut seen_ids: HashMap<(u64, (u64, u64)), ()> = HashMap::new();
    let mut by_sample: HashMap<(u64, [u8; 32]), Vec<usize>> = HashMap::new();
    for (i, c) in cands.iter().enumerate() {
        let Some(k) = c.known else { continue };
        if k.id.0 != u64::MAX && seen_ids.insert((c.size, k.id), ()).is_some() {
            continue;
        }
        by_sample.entry((c.size, k.sample)).or_default().push(i);
    }
    let mut alike: Vec<usize> = by_sample
        .into_values()
        .filter(|v| v.len() > 1)
        .flatten()
        .collect();

    // 3. Whole-file hashes for whatever still looks alike, in parallel.
    let need: Vec<usize> = alike
        .iter()
        .copied()
        .filter(|&i| cands[i].known.is_some_and(|k| k.full.is_none()))
        .collect();
    let need_bytes = need.iter().map(|&i| cands[i].size).sum();
    progress.enter(phase::HASH, need.len() as u64, need_bytes);
    let fulls: Vec<(usize, Option<[u8; 32]>)> = need
        .par_iter()
        .map(|&i| {
            if progress.cancelled() {
                return (i, None);
            }
            let c = &cands[i];
            let h = full_hash(&c.path, c.size);
            progress.files.fetch_add(1, Ordering::Relaxed);
            progress.bytes.fetch_add(c.size, Ordering::Relaxed);
            (i, h)
        })
        .collect();
    for (i, h) in fulls {
        if let Some(k) = cands[i].known.as_mut() {
            k.full = h;
        }
    }

    // Remember what was learned (also for files that turned out unique).
    cache.replace_under(
        root,
        cands
            .iter()
            .filter(|c| !progress.cancelled() || c.known.is_some_and(|k| k.full.is_some()))
            .filter_map(|c| Some((c.path.clone(), c.known?)))
            .collect(),
    );
    if progress.cancelled() {
        return Vec::new();
    }

    // Group by size and full hash.
    alike.sort_unstable();
    let mut by_full: HashMap<(u64, [u8; 32]), Vec<PathBuf>> = HashMap::new();
    for i in alike {
        let c = &cands[i];
        if let Some(full) = c.known.and_then(|k| k.full) {
            by_full
                .entry((c.size, full))
                .or_default()
                .push(c.path.clone());
        }
    }
    let mut groups: Vec<DupGroup> = by_full
        .into_iter()
        .filter(|(_, files)| files.len() > 1)
        .map(|((size, full), mut files)| {
            files.sort(); // stable order so "keep the first" is deterministic
            DupGroup {
                hash: blake3::Hash::from(full).to_hex().to_string(),
                size,
                wasted: size.saturating_mul(files.len() as u64 - 1),
                files,
            }
        })
        .collect();
    groups.sort_by(|a, b| b.wasted.cmp(&a.wasted).then_with(|| a.files.cmp(&b.files)));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(label: &str) -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!("diskghost-dupes-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write(path: &Path, content: &[u8]) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn same_ends_different_middle_is_not_a_duplicate() {
        // Big files that agree in their first and last 16 KB but differ in
        // the middle must survive the quick check and fail the full hash.
        let d = tmpdir("middle");
        let mut a = vec![7u8; 200_000];
        let b = a.clone();
        write(&d.join("a.bin"), &a);
        write(&d.join("b.bin"), &b); // true duplicate of a
        a[100_000] = 8;
        write(&d.join("c.bin"), &a); // same ends, other middle
        let cache = HashCache::new();
        let g = find(&d, 1, &Options::default(), &Progress::default(), &cache);
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].files, vec![d.join("a.bin"), d.join("b.bin")]);
        assert_eq!(g[0].wasted, 200_000);
        assert_eq!(cache.len(), 3);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn the_recycle_bin_is_not_searched() {
        // A copy sent to the recycle bin must not reappear as a duplicate of
        // the file that was kept.
        let d = tmpdir("trash");
        write(&d.join("keep/a.bin"), &[3u8; 50_000]);
        write(
            &d.join("$Recycle.Bin/S-1-5-21/$RABC123.bin"),
            &[3u8; 50_000],
        );
        write(&d.join(".Trash-1000/files/a.bin"), &[3u8; 50_000]);
        let g = find(
            &d,
            1,
            &Options::default(),
            &Progress::default(),
            &HashCache::new(),
        );
        assert!(g.is_empty(), "{g:?}");
        // Searching inside the trash itself still works.
        write(
            &d.join("$Recycle.Bin/S-1-5-21/$RDEF456.bin"),
            &[3u8; 50_000],
        );
        let inside = find(
            &d.join("$Recycle.Bin"),
            1,
            &Options::default(),
            &Progress::default(),
            &HashCache::new(),
        );
        assert_eq!(inside.len(), 1);
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn cache_round_trip_and_reuse() {
        let d = tmpdir("cache");
        write(&d.join("x/1.bin"), &[1u8; 50_000]);
        write(&d.join("y/1.bin"), &[1u8; 50_000]);
        write(&d.join("small1"), b"hello");
        write(&d.join("small2"), b"hello");
        let cache = HashCache::new();
        let first = find(&d, 1, &Options::default(), &Progress::default(), &cache);
        assert_eq!(first.len(), 2);

        let file = d.join("cache/hashes.bin");
        cache.save(&file).unwrap();
        let back = HashCache::load(&file);
        assert_eq!(back.len(), 4);

        // With everything cached the second search reads no file content:
        // the hash phase has nothing to do and the result is the same.
        let p = Progress::default();
        let again = find(&d, 1, &Options::default(), &p, &back);
        assert_eq!(again, first);
        assert_eq!(p.phase(), phase::HASH);
        assert_eq!(p.total_files(), 0);

        // A changed file is looked at again.
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(&d.join("y/1.bin"), &[2u8; 50_000]);
        let changed = find(&d, 1, &Options::default(), &Progress::default(), &back);
        assert_eq!(changed.len(), 1);

        // Damaged or foreign cache files just mean an empty cache.
        std::fs::write(&file, b"garbage").unwrap();
        assert!(HashCache::load(&file).is_empty());
        assert!(HashCache::load(&d.join("missing")).is_empty());
        std::fs::remove_dir_all(&d).ok();
    }
}
