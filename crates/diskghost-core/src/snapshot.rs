//! Snapshots persist a scan so a later one can be compared against it:
//! "what grew since last time".
//!
//! A [`Snapshot`] records *every* directory of a [`ScanTree`] with its subtree
//! totals (not just the biggest ones), so [`diff`] can answer for the root or
//! for any sub-folder which children grew, shrank, appeared or vanished.
//! Snapshots are plain JSON, versioned by [`SNAPSHOT_FORMAT`].

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{DirSize, FileEntry, ScanTree};

/// On-disk format version of [`Snapshot`]; bumped on incompatible changes.
pub const SNAPSHOT_FORMAT: u32 = 1;

/// Why a snapshot could not be read, written or compared.
#[derive(Debug)]
pub enum SnapshotError {
    Io(std::io::Error),
    Json(serde_json::Error),
    /// The file is a snapshot of another format version.
    Format(u32),
    /// The two snapshots were taken of different directories.
    RootMismatch {
        old: PathBuf,
        new: PathBuf,
    },
    /// The directory to compare at is not part of the newer snapshot.
    NotInSnapshot(PathBuf),
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "{e}"),
            SnapshotError::Json(e) => write!(f, "invalid snapshot: {e}"),
            SnapshotError::Format(v) => write!(
                f,
                "unsupported snapshot format {v} (this build reads format {SNAPSHOT_FORMAT})"
            ),
            SnapshotError::RootMismatch { old, new } => write!(
                f,
                "snapshots are of different directories: {} vs {}",
                old.display(),
                new.display()
            ),
            SnapshotError::NotInSnapshot(p) => {
                write!(f, "not a directory in the snapshot: {}", p.display())
            }
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

/// A persisted scan: totals of the root, every directory below it, and the
/// biggest files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub format: u32,
    /// When the scan was taken, in seconds since the Unix epoch.
    pub taken_at: u64,
    pub root: PathBuf,
    pub total_size: u64,
    pub total_files: u64,
    pub total_dirs: u64,
    /// Every directory below the root with its subtree totals, sorted by path.
    pub dirs: Vec<DirSize>,
    /// Largest files anywhere under the root, largest first.
    pub top_files: Vec<FileEntry>,
}

/// Only the field needed to reject a foreign format before parsing the rest.
#[derive(Deserialize)]
struct Header {
    format: u32,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Snapshot {
    /// Capture `tree` as it is now, keeping the `top_files` biggest files.
    pub fn capture(tree: &ScanTree, top_files: usize) -> Snapshot {
        let r = tree.root_report(top_files);
        Snapshot {
            format: SNAPSHOT_FORMAT,
            taken_at: now_unix(),
            root: r.root,
            total_size: r.total_size,
            total_files: r.total_files,
            total_dirs: r.total_dirs,
            dirs: tree.dir_sizes(),
            top_files: r.top_files,
        }
    }

    /// Read a snapshot written by [`Snapshot::save`].
    pub fn load(path: &Path) -> Result<Snapshot, SnapshotError> {
        let text = std::fs::read_to_string(path)?;
        let header: Header = serde_json::from_str(&text)?;
        if header.format != SNAPSHOT_FORMAT {
            return Err(SnapshotError::Format(header.format));
        }
        Ok(serde_json::from_str(&text)?)
    }

    /// Write the snapshot as JSON, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<(), SnapshotError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec(self)?)?;
        Ok(())
    }

    /// A short, filesystem-safe key that identifies a scanned root, so a store
    /// can keep one history per directory (BLAKE3 of the path, 16 hex chars).
    pub fn root_key(root: &Path) -> String {
        let hex = blake3::hash(root.to_string_lossy().as_bytes()).to_hex();
        hex[..16].to_string()
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
}

impl DiffReport {
    /// Keep at most `top` entries in each list.
    pub fn truncate(&mut self, top: usize) {
        self.grown.truncate(top);
        self.shrunk.truncate(top);
        self.added.truncate(top);
        self.removed.truncate(top);
    }

    /// True if nothing at all changed under this directory.
    pub fn is_unchanged(&self) -> bool {
        self.delta == 0
            && self.files_delta == 0
            && self.grown.is_empty()
            && self.shrunk.is_empty()
            && self.added.is_empty()
            && self.removed.is_empty()
    }
}

fn signed(x: u64) -> i64 {
    i64::try_from(x).unwrap_or(i64::MAX)
}

/// The immediate sub-folders of `at` in a snapshot, keyed by path.
fn children_of<'a>(snap: &'a Snapshot, at: &Path) -> HashMap<&'a Path, &'a DirSize> {
    snap.dirs
        .iter()
        .filter(|d| d.path.parent() == Some(at))
        .map(|d| (d.path.as_path(), d))
        .collect()
}

/// Compare two snapshots of the same root at directory `at` (the root or any
/// sub-folder of the newer snapshot). A folder missing from the older snapshot
/// counts as having been empty.
pub fn diff(old: &Snapshot, new: &Snapshot, at: &Path) -> Result<DiffReport, SnapshotError> {
    if old.root != new.root {
        return Err(SnapshotError::RootMismatch {
            old: old.root.clone(),
            new: new.root.clone(),
        });
    }
    let totals = |snap: &Snapshot| -> Option<(u64, u64)> {
        if at == snap.root {
            return Some((snap.total_size, snap.total_files));
        }
        snap.dirs
            .iter()
            .find(|d| d.path == at)
            .map(|d| (d.size, d.file_count))
    };
    let (new_size, new_files) =
        totals(new).ok_or_else(|| SnapshotError::NotInSnapshot(at.to_path_buf()))?;
    let (old_size, old_files) = totals(old).unwrap_or((0, 0));

    let old_kids = children_of(old, at);
    let new_kids = children_of(new, at);

    let mut grown = Vec::new();
    let mut shrunk = Vec::new();
    let mut added = Vec::new();
    let mut removed = Vec::new();

    for (path, n) in &new_kids {
        match old_kids.get(path) {
            Some(o) => {
                let delta = signed(n.size) - signed(o.size);
                if delta == 0 {
                    continue;
                }
                let entry = DirDelta {
                    path: n.path.clone(),
                    old_size: o.size,
                    new_size: n.size,
                    delta,
                    old_files: o.file_count,
                    new_files: n.file_count,
                };
                if delta > 0 {
                    grown.push(entry);
                } else {
                    shrunk.push(entry);
                }
            }
            None => added.push((*n).clone()),
        }
    }
    for (path, o) in &old_kids {
        if !new_kids.contains_key(path) {
            removed.push((*o).clone());
        }
    }

    grown.sort_by_key(|d| (std::cmp::Reverse(d.delta), d.path.clone()));
    shrunk.sort_by_key(|d| (d.delta, d.path.clone()));
    added.sort_by_key(|d| (std::cmp::Reverse(d.size), d.path.clone()));
    removed.sort_by_key(|d| (std::cmp::Reverse(d.size), d.path.clone()));

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
        grown,
        shrunk,
        added,
        removed,
    })
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
        Snapshot::capture(
            &ScanTree::build(dir, &Options::default(), &Progress::default()),
            5,
        )
    }

    #[test]
    fn capture_save_load_roundtrip() {
        let d = tmpdir("roundtrip");
        write(&d.join("a/1.bin"), &[0u8; 100]);
        write(&d.join("a/b/2.bin"), &[0u8; 50]);
        let s = snap(&d);
        assert_eq!(s.format, SNAPSHOT_FORMAT);
        assert!(s.taken_at > 1_600_000_000);
        assert_eq!(s.root, d);
        assert_eq!(s.total_files, 2);
        assert_eq!(s.total_dirs, 2);
        assert_eq!(s.dirs.len(), 2);
        assert_eq!(s.top_files.len(), 2);

        let file = d.join("history/snap.json");
        s.save(&file).unwrap();
        let back = Snapshot::load(&file).unwrap();
        assert_eq!(back, s);

        // Wrong format and broken JSON are reported, not silently accepted.
        std::fs::write(&file, br#"{"format": 99}"#).unwrap();
        assert!(matches!(
            Snapshot::load(&file),
            Err(SnapshotError::Format(99))
        ));
        std::fs::write(&file, b"not json").unwrap();
        assert!(matches!(Snapshot::load(&file), Err(SnapshotError::Json(_))));
        assert!(matches!(
            Snapshot::load(&d.join("missing.json")),
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
        assert_eq!(
            r.grown[0].delta,
            signed(r.grown[0].new_size) - signed(r.grown[0].old_size)
        );
        assert_eq!(r.shrunk.len(), 1);
        assert_eq!(r.shrunk[0].path, d.join("shrink"));
        assert!(r.shrunk[0].delta < 0);
        assert_eq!(r.shrunk[0].new_files, 0);
        assert_eq!(r.added.len(), 1);
        assert_eq!(r.added[0].path, d.join("fresh"));
        assert_eq!(r.removed.len(), 1);
        assert_eq!(r.removed[0].path, d.join("gone"));

        // Identical snapshots: nothing to report.
        let none = diff(&new, &new, &d).unwrap();
        assert!(none.is_unchanged());

        // Truncation caps every list.
        let mut capped = r.clone();
        capped.truncate(0);
        assert!(capped.grown.is_empty() && capped.removed.is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn diff_at_a_sub_folder() {
        let d = tmpdir("diffsub");
        write(&d.join("a/x/1.bin"), &[0u8; 100]);
        let old = snap(&d);
        write(&d.join("a/x/2.bin"), &[0u8; 100]);
        write(&d.join("a/y/1.bin"), &[0u8; 100]);
        let new = snap(&d);

        let r = diff(&old, &new, &d.join("a")).unwrap();
        assert_eq!(r.root, d.join("a"));
        assert_eq!(r.old_files, 1);
        assert_eq!(r.new_files, 3);
        assert_eq!(r.files_delta, 2);
        assert_eq!(r.grown.len(), 1);
        assert_eq!(r.grown[0].path, d.join("a/x"));
        assert_eq!(r.added.len(), 1);
        assert_eq!(r.added[0].path, d.join("a/y"));
        assert!(r.shrunk.is_empty() && r.removed.is_empty());

        // A folder the old snapshot never saw counts as previously empty.
        let y = diff(&old, &new, &d.join("a/y")).unwrap();
        assert_eq!(y.old_size, 0);
        assert_eq!(y.old_files, 0);
        assert_eq!(y.new_files, 1);

        // But it must exist in the new one.
        assert!(matches!(
            diff(&old, &new, &d.join("nope")),
            Err(SnapshotError::NotInSnapshot(_))
        ));
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
    fn error_messages_are_descriptive() {
        assert!(SnapshotError::Format(7).to_string().contains("format 7"));
        assert!(SnapshotError::NotInSnapshot(PathBuf::from("x"))
            .to_string()
            .contains("not a directory"));
        let io: SnapshotError = std::io::Error::other("boom").into();
        assert_eq!(io.to_string(), "boom");
        let json: SnapshotError = serde_json::from_str::<Snapshot>("{").unwrap_err().into();
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
