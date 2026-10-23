//! Diskghost CLI — fast disk usage & duplicate finder.
//!
//! Human-readable by default; `--json` emits machine-readable output for
//! scripts and agents (headless use).

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use diskghost_core::{
    diff, find_duplicates_cached, format_timestamp, human_delta, human_size, phase, reclaim,
    remove_path, validate_globs, DiffReport, DupGroup, HashCache, Options, Progress, ReclaimAction,
    ReclaimReport, RemoveMode, RemoveReport, ScanReport, ScanTree, Snapshot,
};
use serde::Serialize;

/// Run `job` with a `Progress`, printing a live line to stderr while it works
/// (only when stderr is a terminal, so JSON/pipe output stays clean).
fn with_progress<T>(job: impl FnOnce(&Progress) -> T) -> T {
    let progress = Progress::default();
    if !std::io::stderr().is_terminal() {
        return job(&progress);
    }
    let done = Arc::new(AtomicBool::new(false));
    let (p, d) = (progress.clone(), done.clone());
    let handle = std::thread::spawn(move || {
        while !d.load(Ordering::Relaxed) {
            match p.phase() {
                phase::SAMPLE => eprint!(
                    "\r  comparing file ends… {} / {} files   ",
                    p.files(),
                    p.total_files()
                ),
                phase::HASH => eprint!(
                    "\r  hashing… {} / {} files, {} / {}   ",
                    p.files(),
                    p.total_files(),
                    human_size(p.bytes()),
                    human_size(p.total_bytes())
                ),
                _ => eprint!(
                    "\r  working… {} files, {}   ",
                    p.files(),
                    human_size(p.bytes())
                ),
            }
            let _ = std::io::stderr().flush();
            std::thread::sleep(std::time::Duration::from_millis(120));
        }
    });
    let out = job(&progress);
    done.store(true, Ordering::Relaxed);
    let _ = handle.join();
    eprint!("\r\x1b[2K"); // clear the progress line
    let _ = std::io::stderr().flush();
    out
}

/// Refuse invalid exclude globs up front: skipping one would scan, or reclaim,
/// exactly what the caller meant to leave out.
fn check_globs(patterns: &[String]) -> bool {
    let bad = validate_globs(patterns);
    if !bad.is_empty() {
        eprintln!("error: invalid exclude glob(s): {}", bad.join(", "));
    }
    bad.is_empty()
}

#[derive(Parser)]
#[command(
    name = "diskghost",
    version,
    about = "Fast disk usage & duplicate finder 👻"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Emit machine-readable JSON (for scripts / agents).
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a directory: total size, biggest sub-folders and biggest files.
    Scan {
        /// Directory to scan.
        path: PathBuf,
        /// How many top entries to show.
        #[arg(long, default_value_t = 20)]
        top: usize,
        #[command(flatten)]
        walk: WalkArgs,
        /// Save the scan as a snapshot (every folder and file, compressed) for a
        /// later `--since` or `diff`.
        #[arg(long, value_name = "FILE")]
        save: Option<PathBuf>,
        /// Compare against a snapshot saved earlier: what grew, shrank,
        /// appeared or vanished since then.
        #[arg(long, value_name = "FILE")]
        since: Option<PathBuf>,
    },
    /// Compare two snapshots saved with `scan --save`.
    Diff {
        /// The older snapshot.
        old: PathBuf,
        /// The newer snapshot.
        new: PathBuf,
        /// Compare at this sub-folder instead of the scanned root.
        #[arg(long, value_name = "DIR")]
        at: Option<PathBuf>,
        /// How many entries to show per list.
        #[arg(long, default_value_t = 20)]
        top: usize,
    },
    /// Find duplicate (byte-identical) files; optionally reclaim their space.
    Dupes {
        /// Directory to scan.
        path: PathBuf,
        /// Ignore files smaller than this many megabytes.
        #[arg(long, default_value_t = 1)]
        min_mb: u64,
        #[command(flatten)]
        walk: WalkArgs,
        /// Reclaim redundant copies (one file per group is always kept).
        /// Nothing is changed unless you also pass --apply.
        #[arg(long, value_enum)]
        reclaim: Option<ReclaimArg>,
        /// Actually perform the reclaim (default: dry-run).
        #[arg(long)]
        apply: bool,
        /// Keep file hashes in this file between runs: a repeated search only
        /// reads files whose size or modification time changed.
        #[arg(long, value_name = "FILE")]
        cache: Option<PathBuf>,
    },
    /// Delete a file or folder — permanently, or to the OS trash. Dry-run by default.
    Rm {
        /// File or directory to remove.
        path: PathBuf,
        /// Send to the OS trash / recycle bin (reversible) instead of deleting.
        #[arg(long)]
        trash: bool,
        /// Actually remove (default: dry-run that only reports what would go).
        #[arg(long)]
        apply: bool,
    },
}

/// Flags shared by scan + dupes that control how the tree is walked.
#[derive(Args)]
struct WalkArgs {
    /// Glob(s) to exclude (repeatable), matched on path/component/name.
    /// e.g. --exclude "*.tmp" --exclude node_modules
    #[arg(long)]
    exclude: Vec<String>,
    /// Limit recursion depth below the root (1 = the root's direct children).
    #[arg(long)]
    max_depth: Option<usize>,
    /// Follow symbolic links / junctions.
    #[arg(long)]
    follow_symlinks: bool,
    /// Threads listing directories (default: twice the CPU cores; 2-4 suits a
    /// spinning disk).
    #[arg(long)]
    threads: Option<usize>,
    /// Read the NTFS Master File Table instead of listing directories (much
    /// faster on Windows; needs an administrator console, falls back to the
    /// directory walk otherwise).
    #[arg(long)]
    mft: bool,
}

impl WalkArgs {
    fn to_options(&self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude.clone(),
            threads: self.threads,
            mft: self.mft,
        }
    }
}

#[derive(Clone, ValueEnum)]
enum ReclaimArg {
    /// Permanently delete redundant copies.
    Delete,
    /// Move redundant copies to the OS trash / recycle bin.
    Trash,
    /// Replace redundant copies with a hard link to the kept file.
    Hardlink,
}

impl From<ReclaimArg> for ReclaimAction {
    fn from(a: ReclaimArg) -> Self {
        match a {
            ReclaimArg::Delete => ReclaimAction::Delete,
            ReclaimArg::Trash => ReclaimAction::Trash,
            ReclaimArg::Hardlink => ReclaimAction::Hardlink,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Scan {
            path,
            top,
            walk,
            save,
            since,
        } => {
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            if !check_globs(&walk.exclude) {
                return ExitCode::FAILURE;
            }
            let opts = walk.to_options();
            // Load the old snapshot before scanning, so a bad file fails fast.
            let old = match since.as_deref().map(Snapshot::load) {
                Some(Ok(s)) => Some(s),
                Some(Err(e)) => {
                    eprintln!("error: cannot read snapshot: {e}");
                    return ExitCode::FAILURE;
                }
                None => None,
            };
            let started = std::time::Instant::now();
            let tree = with_progress(|p| ScanTree::build(&path, &opts, p));
            let elapsed = started.elapsed();
            let report = tree.root_report(top);
            let snapshot = (old.is_some() || save.is_some()).then(|| Snapshot::capture(&tree));
            let changes = match (&old, &snapshot) {
                (Some(old), Some(new)) => match diff(old, new, &report.root) {
                    Ok(mut d) => {
                        d.truncate(top);
                        Some(d)
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        return ExitCode::FAILURE;
                    }
                },
                _ => None,
            };
            // Save before printing, so a failure leaves stdout empty.
            let saved = match (&save, &snapshot) {
                (Some(file), Some(snap)) => {
                    if let Err(e) = snap.save(file) {
                        eprintln!("error: cannot save snapshot: {e}");
                        return ExitCode::FAILURE;
                    }
                    Some(file.as_path())
                }
                _ => None,
            };
            if cli.json {
                let out = ScanOutput {
                    report: &report,
                    diff: changes.as_ref(),
                    engine: tree.engine(),
                    mft_fallback: tree.mft_fallback(),
                    snapshot_saved: saved,
                };
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else {
                print_scan(&report);
                if let Some(d) = &changes {
                    println!();
                    print_diff(d);
                }
                let engine = if tree.engine() == "mft" {
                    " (NTFS MFT)"
                } else {
                    ""
                };
                println!("\nScanned in {}{engine}", fmt_duration(elapsed));
                if let Some(why) = tree.mft_fallback() {
                    eprintln!("note: --mft not used: {why}");
                }
                if let Some(file) = saved {
                    eprintln!("snapshot saved to {}", file.display());
                }
            }
        }
        Command::Diff { old, new, at, top } => {
            let (old, new) = match (Snapshot::load(&old), Snapshot::load(&new)) {
                (Ok(o), Ok(n)) => (o, n),
                (Err(e), _) | (_, Err(e)) => {
                    eprintln!("error: cannot read snapshot: {e}");
                    return ExitCode::FAILURE;
                }
            };
            let at = at.unwrap_or_else(|| new.root.clone());
            let mut report = match diff(&old, &new, &at) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::FAILURE;
                }
            };
            report.truncate(top);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_diff(&report);
            }
        }
        Command::Dupes {
            path,
            min_mb,
            walk,
            reclaim: reclaim_arg,
            apply,
            cache,
        } => {
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            if !check_globs(&walk.exclude) {
                return ExitCode::FAILURE;
            }
            let opts = walk.to_options();
            let hashes = cache.as_deref().map(HashCache::load).unwrap_or_default();
            let groups = with_progress(|p| {
                find_duplicates_cached(&path, min_mb * 1024 * 1024, &opts, p, &hashes)
            });
            // With --json, warnings go into the output instead of stderr.
            let mut warnings = Vec::new();
            if let Some(file) = &cache {
                if let Err(e) = hashes.save(file) {
                    let w = format!("cannot save hash cache: {e}");
                    if cli.json {
                        warnings.push(w);
                    } else {
                        eprintln!("warning: {w}");
                    }
                }
            }
            if !cli.json {
                print_dupes(&groups);
            }
            let reclaimed = reclaim_arg.map(|arg| run_reclaim(&groups, arg.into(), !apply));
            if cli.json {
                let out = DupesOutput {
                    groups: &groups,
                    reclaim: reclaimed.as_ref(),
                    warnings,
                };
                println!("{}", serde_json::to_string_pretty(&out).unwrap());
            } else if let Some(r) = &reclaimed {
                print_reclaim(r);
            }
            if reclaimed.is_some_and(|r| !r.errors.is_empty()) {
                return ExitCode::FAILURE;
            }
        }
        Command::Rm { path, trash, apply } => {
            if !path.exists() {
                eprintln!("error: no such path: {}", path.display());
                return ExitCode::FAILURE;
            }
            // Refuse a filesystem / drive root (e.g. `/` or `C:\`) — a path with
            // no parent — so a stray `rm C:\ --apply` can't nuke the volume.
            if path.parent().is_none() {
                eprintln!(
                    "error: refusing to remove a filesystem root: {}",
                    path.display()
                );
                return ExitCode::FAILURE;
            }
            let mode = if trash {
                RemoveMode::Trash
            } else {
                RemoveMode::Delete
            };
            let started = std::time::Instant::now();
            let report = with_progress(|p| remove_path(&path, mode, !apply, p));
            let elapsed = started.elapsed();
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_remove(&report, trash);
                if !report.dry_run {
                    println!("  done in {}", fmt_duration(elapsed));
                }
            }
            if !report.errors.is_empty() {
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
}

/// `scan --json` output: the report's fields at the top level, plus `diff`
/// when `--since` was given.
#[derive(Serialize)]
struct ScanOutput<'a> {
    #[serde(flatten)]
    report: &'a ScanReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    diff: Option<&'a DiffReport>,
    /// "walk" or "mft".
    engine: &'a str,
    /// Why --mft was asked for but not used.
    #[serde(skip_serializing_if = "Option::is_none")]
    mft_fallback: Option<&'a str>,
    /// Where `--save` wrote the snapshot.
    #[serde(skip_serializing_if = "Option::is_none")]
    snapshot_saved: Option<&'a Path>,
}

/// `dupes --json` output: the groups, plus `reclaim` when `--reclaim` was
/// given and `warnings` for problems that did not stop the search.
#[derive(Serialize)]
struct DupesOutput<'a> {
    groups: &'a [DupGroup],
    #[serde(skip_serializing_if = "Option::is_none")]
    reclaim: Option<&'a ReclaimReport>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

fn print_diff(d: &DiffReport) {
    println!(
        "Changes in {} since {} (now {})",
        d.root.display(),
        format_timestamp(d.old_taken_at),
        format_timestamp(d.new_taken_at)
    );
    println!(
        "  total: {} ({} -> {}), {:+} file(s)",
        human_delta(d.delta),
        human_size(d.old_size),
        human_size(d.new_size),
        d.files_delta
    );
    if d.is_unchanged() {
        println!("  nothing changed");
        return;
    }
    if !d.grown.is_empty() {
        println!("\nGrew:");
        for g in &d.grown {
            println!(
                "  {:>10}  {}  ({} -> {})",
                human_delta(g.delta),
                g.path.display(),
                human_size(g.old_size),
                human_size(g.new_size)
            );
        }
    }
    if !d.shrunk.is_empty() {
        println!("\nShrank:");
        for s in &d.shrunk {
            println!(
                "  {:>10}  {}  ({} -> {})",
                human_delta(s.delta),
                s.path.display(),
                human_size(s.old_size),
                human_size(s.new_size)
            );
        }
    }
    if !d.added.is_empty() {
        println!("\nNew folders:");
        for a in &d.added {
            println!("  {:>10}  {}", human_size(a.size), a.path.display());
        }
    }
    if !d.removed.is_empty() {
        println!("\nRemoved folders:");
        for r in &d.removed {
            println!("  {:>10}  {}", human_size(r.size), r.path.display());
        }
    }
}

fn print_remove(r: &RemoveReport, trash: bool) {
    let dest = if trash { "trash" } else { "delete" };
    let what = format!(
        "{} file(s), {} dir(s), {}",
        r.files,
        r.dirs,
        human_size(r.bytes)
    );
    if r.dry_run {
        println!("DRY-RUN [{dest}] {}: would remove {what}", r.root.display());
        println!("  pass --apply to actually remove");
    } else {
        println!("[{dest}] {}: removed {what}", r.root.display());
    }
    for e in &r.errors {
        eprintln!("  ! {e}");
    }
    if !r.errors.is_empty() {
        eprintln!("  {} error(s)", r.errors.len());
    }
}

/// Human-friendly elapsed time: sub-second in ms, seconds with one decimal,
/// minutes+seconds when it runs long.
fn fmt_duration(d: std::time::Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{} ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let m = (secs / 60.0).floor() as u64;
        let s = secs - (m as f64) * 60.0;
        format!("{m}m {s:.0}s")
    }
}

fn print_scan(r: &ScanReport) {
    println!("Scan of {}", r.root.display());
    println!(
        "  total: {} across {} files in {} dirs",
        human_size(r.total_size),
        r.total_files,
        r.total_dirs
    );
    if r.disk_total > 0 {
        let used = r.disk_total.saturating_sub(r.disk_free);
        let pct = (used as f64 / r.disk_total as f64) * 100.0;
        println!(
            "  disk: {} free of {} ({:.0}% used)",
            human_size(r.disk_free),
            human_size(r.disk_total),
            pct
        );
    }
    if r.skipped > 0 {
        println!("  skipped: {} unreadable entries (permissions?)", r.skipped);
    }
    if r.cancelled {
        println!("  CANCELLED: partial result, the numbers cover only what was scanned");
    }
    println!();

    println!("Biggest sub-folders:");
    for d in &r.children {
        println!("  {:>10}  {}", human_size(d.size), d.path.display());
    }
    if r.root_files_count > 0 {
        println!(
            "  {:>10}  ({} file(s) directly in root)",
            human_size(r.root_files_size),
            r.root_files_count
        );
    }

    println!("\nBiggest files:");
    for f in &r.top_files {
        println!("  {:>10}  {}", human_size(f.size), f.path.display());
    }
}

fn print_dupes(groups: &[DupGroup]) {
    if groups.is_empty() {
        println!("No duplicates found.");
        return;
    }
    let total: u64 = groups.iter().map(|g| g.wasted).sum();
    println!(
        "Found {} duplicate group(s) — {} reclaimable:\n",
        groups.len(),
        human_size(total)
    );
    for g in groups {
        println!(
            "  {} x {} ({} wasted) [{}]",
            g.files.len(),
            human_size(g.size),
            human_size(g.wasted),
            &g.hash[..12]
        );
        for f in &g.files {
            println!("      {}", f.display());
        }
        println!();
    }
}

/// Reclaim space across all groups, keeping the first file of each group.
fn run_reclaim(groups: &[DupGroup], action: ReclaimAction, dry_run: bool) -> ReclaimReport {
    let mut total = ReclaimReport {
        removed: 0,
        reclaimed: 0,
        errors: Vec::new(),
        dry_run,
    };
    for g in groups {
        if g.files.len() < 2 {
            continue;
        }
        let keep = &g.files[0];
        let report = reclaim(keep, &g.files[1..], g.size, action, dry_run);
        total.removed += report.removed;
        total.reclaimed += report.reclaimed;
        total.errors.extend(report.errors);
    }
    total
}

fn print_reclaim(r: &ReclaimReport) {
    for e in &r.errors {
        eprintln!("  ! {e}");
    }
    let mode = if r.dry_run {
        "DRY-RUN — nothing changed; pass --apply to act"
    } else {
        "applied"
    };
    print!(
        "\nReclaim [{mode}]: {} file(s), {} reclaimable",
        r.removed,
        human_size(r.reclaimed)
    );
    if !r.errors.is_empty() {
        print!(", {} error(s)", r.errors.len());
    }
    println!();
}
