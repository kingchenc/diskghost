//! The elevated MFT helper.
//!
//! The window never runs as administrator: Windows blocks drag & drop from
//! Explorer into an elevated window. Reading the NTFS Master File Table does
//! need those rights, so for the "Ultra" engine the app starts its own
//! executable a second time — elevated, without a window, after the user
//! consents once per session — and lets it do the scans. The two talk through
//! files in a private folder:
//!
//! | file                 | from   | content                               |
//! |----------------------|--------|---------------------------------------|
//! | `ready-<token>`      | helper | it is up                              |
//! | `beat`               | helper | touched every second while it runs    |
//! | `alive`              | app    | touched every few seconds; stale = quit |
//! | `request-<id>.json`  | app    | token, folder, walk options           |
//! | `progress-<id>.json` | helper | files and bytes so far                |
//! | `cancel-<id>`        | app    | stop that scan                        |
//! | `result-<id>.dgs`    | helper | the scan as a snapshot                |
//! | `done-<id>.json`     | helper | outcome                               |
//!
//! Every request must carry a random token that only the app and the helper
//! know (it travels on the elevated command line, which non-elevated programs
//! cannot read). The helper only ever reads disks and writes into this folder
//! under fixed names, so even a forged request could do no harm.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use diskghost_core::{Progress, ScanTree, Snapshot};
use serde::{Deserialize, Serialize};

use crate::WalkOpts;

/// The helper quits when the app has not touched `alive` for this long.
const APP_TIMEOUT: Duration = Duration::from_secs(20);
/// The app gives up when the helper has not touched `beat` for this long.
const HELPER_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the user has to answer the consent prompt.
#[cfg(windows)]
const CONSENT_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Serialize, Deserialize)]
struct Request {
    token: String,
    path: PathBuf,
    opts: WalkOpts,
}

#[derive(Serialize, Deserialize, Default)]
struct Counts {
    files: u64,
    bytes: u64,
}

#[derive(Serialize, Deserialize)]
struct Done {
    ok: bool,
    cancelled: bool,
    error: Option<String>,
    mft: bool,
    fallback: Option<String>,
}

fn age(file: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(file).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

fn touch(file: &Path) {
    let _ = std::fs::write(file, b"");
}

/// Write `bytes` to `file` so a reader never sees half of it.
fn write_atomic(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(tmp, file)
}

// ------------------------------- helper side -------------------------------

/// Run as the helper (`--mft-helper <dir> <token>`) until the app is gone.
pub fn serve(dir: &Path, token: &str) {
    let beat = dir.join("beat");
    touch(&beat);
    touch(&dir.join("alive")); // the app touches it from now on
    touch(&dir.join(format!("ready-{token}")));
    loop {
        touch(&beat);
        if age(&dir.join("alive")).is_none_or(|a| a > APP_TIMEOUT) {
            return;
        }
        let requests: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().into_string().ok()?;
                let id = name.strip_prefix("request-")?.strip_suffix(".json")?;
                // Ids are digits only: they become parts of file names.
                id.bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| (id.to_string(), e.path()))
            })
            .collect();
        for (id, file) in requests {
            let req = std::fs::read(&file)
                .ok()
                .and_then(|b| serde_json::from_slice::<Request>(&b).ok());
            let _ = std::fs::remove_file(&file);
            match req {
                Some(r) if r.token == token => handle(dir, &id, r),
                _ => {} // foreign or broken: ignored
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn handle(dir: &Path, id: &str, req: Request) {
    let progress = Progress::default();
    let done = Arc::new(AtomicBool::new(false));
    let reporter = {
        let (progress, done) = (progress.clone(), done.clone());
        let (counts, cancel, beat) = (
            dir.join(format!("progress-{id}.json")),
            dir.join(format!("cancel-{id}")),
            dir.join("beat"),
        );
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let c = Counts {
                    files: progress.files(),
                    bytes: progress.bytes(),
                };
                let _ = write_atomic(&counts, &serde_json::to_vec(&c).unwrap_or_default());
                if cancel.exists() {
                    progress.cancel();
                }
                touch(&beat);
                std::thread::sleep(Duration::from_millis(150));
            }
        })
    };
    let mut opts = req.opts.to_options();
    opts.mft = true;
    let tree = ScanTree::build(&req.path, &opts, &progress);
    done.store(true, Ordering::Relaxed);
    let _ = reporter.join();
    let saved = Snapshot::capture(&tree).save(&dir.join(format!("result-{id}.dgs")));
    let outcome = Done {
        ok: saved.is_ok(),
        cancelled: tree.cancelled(),
        error: saved.err().map(|e| e.to_string()),
        mft: tree.engine() == "mft",
        fallback: tree.mft_fallback().map(str::to_string),
    };
    let _ = write_atomic(
        &dir.join(format!("done-{id}.json")),
        &serde_json::to_vec(&outcome).unwrap_or_default(),
    );
}

// --------------------------------- app side ---------------------------------

/// A running helper, as the app sees it.
pub struct Helper {
    dir: PathBuf,
    token: String,
}

/// A random token: hashes keyed by the OS random source.
#[cfg(windows)]
fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        h.finish()
    };
    format!("{:016x}{:016x}", part(), part())
}

impl Helper {
    /// Start the helper elevated (Windows asks the user) and wait until it is
    /// up. An error if consent was refused or it never came up.
    #[cfg(windows)]
    pub fn start(dir: PathBuf) -> Result<Helper, String> {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "shell32")]
        extern "system" {
            fn ShellExecuteW(
                hwnd: *mut std::ffi::c_void,
                op: *const u16,
                file: *const u16,
                params: *const u16,
                dir: *const u16,
                show: i32,
            ) -> isize;
        }
        // A fresh folder: nothing old can be mistaken for an answer.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let token = random_token();
        touch(&dir.join("alive"));

        let wide = |s: &std::ffi::OsStr| -> Vec<u16> {
            s.encode_wide().chain(std::iter::once(0)).collect()
        };
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let params = format!("--mft-helper \"{}\" {token}", dir.display());
        let (op, file, params) = (
            wide("runas".as_ref()),
            wide(exe.as_os_str()),
            wide(params.as_ref()),
        );
        let r = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                op.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                0, // SW_HIDE
            )
        };
        if r <= 32 {
            return Err("administrator rights were not granted".into());
        }
        let helper = Helper { dir, token };
        let ready = helper.dir.join(format!("ready-{}", helper.token));
        let started = std::time::Instant::now();
        while !ready.exists() {
            if started.elapsed() > CONSENT_TIMEOUT {
                return Err("the MFT helper did not start".into());
            }
            touch(&helper.dir.join("alive"));
            std::thread::sleep(Duration::from_millis(100));
        }
        // Keep it alive for as long as this app runs.
        let alive = helper.dir.join("alive");
        std::thread::spawn(move || loop {
            touch(&alive);
            std::thread::sleep(Duration::from_secs(3));
        });
        Ok(helper)
    }

    #[cfg(not(windows))]
    pub fn start(_dir: PathBuf) -> Result<Helper, String> {
        Err("the MFT engine exists on Windows only".into())
    }

    /// Whether the helper is still running.
    pub fn alive(&self) -> bool {
        age(&self.dir.join("beat")).is_some_and(|a| a < HELPER_TIMEOUT)
    }

    /// Scan `path` in the helper as job `id`, mirroring its progress into
    /// `progress` (and a cancel back). Returns the tree, its snapshot, and
    /// why the MFT was not used, if so.
    pub fn scan(
        &self,
        id: u64,
        path: &Path,
        opts: &WalkOpts,
        progress: &Progress,
    ) -> Result<(ScanTree, Snapshot, Option<String>), String> {
        let file = |what: &str| self.dir.join(what.replace("{id}", &id.to_string()));
        let req = Request {
            token: self.token.clone(),
            path: path.to_path_buf(),
            opts: opts.clone(),
        };
        let bytes = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
        write_atomic(&file("request-{id}.json"), &bytes).map_err(|e| e.to_string())?;
        let done = loop {
            if let Ok(b) = std::fs::read(file("done-{id}.json")) {
                break serde_json::from_slice::<Done>(&b).map_err(|e| e.to_string())?;
            }
            if progress.cancelled() {
                touch(&file("cancel-{id}"));
            }
            if let Some(c) = std::fs::read(file("progress-{id}.json"))
                .ok()
                .and_then(|b| serde_json::from_slice::<Counts>(&b).ok())
            {
                progress.set_counts(c.files, c.bytes);
            }
            if !self.alive() {
                return Err("the MFT helper stopped".into());
            }
            std::thread::sleep(Duration::from_millis(120));
        };
        let result = file("result-{id}.dgs");
        let loaded = if done.ok {
            Snapshot::load(&result).map_err(|e| e.to_string())
        } else {
            Err(done.error.unwrap_or_else(|| "the MFT helper failed".into()))
        };
        for f in [
            "request-{id}.json",
            "progress-{id}.json",
            "cancel-{id}",
            "done-{id}.json",
            "result-{id}.dgs",
        ] {
            let _ = std::fs::remove_file(file(f));
        }
        let snap = loaded?;
        let mut tree = snap.to_tree().map_err(|e| e.to_string())?;
        tree.set_engine(done.mft, done.fallback.clone());
        if done.cancelled {
            tree.mark_cancelled();
        }
        tree.refresh_disk_space();
        Ok((tree, snap, done.fallback))
    }
}
