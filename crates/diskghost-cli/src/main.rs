//! Diskghost CLI — fast disk usage & duplicate finder.
//!
//! Human-readable by default; `--json` emits machine-readable output for
//! scripts and agents (headless use).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand, ValueEnum};
use diskghost_core::{
    find_duplicates_with_progress, human_size, reclaim, remove_path, scan_with_progress,
    validate_globs, DupGroup, Options, Progress, ReclaimAction, RemoveMode, RemoveReport,
    ScanReport,
};

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
            eprint!(
                "\r  working… {} files, {}   ",
                p.files(),
                human_size(p.bytes())
            );
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

fn warn_invalid_globs(patterns: &[String]) {
    let bad = validate_globs(patterns);
    if !bad.is_empty() {
        eprintln!(
            "warning: ignoring invalid exclude glob(s): {}",
            bad.join(", ")
        );
    }
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
}

impl WalkArgs {
    fn to_options(&self) -> Options {
        Options {
            max_depth: self.max_depth,
            follow_symlinks: self.follow_symlinks,
            exclude: self.exclude.clone(),
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
        Command::Scan { path, top, walk } => {
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            warn_invalid_globs(&walk.exclude);
            let opts = walk.to_options();
            let report = with_progress(|p| scan_with_progress(&path, top, &opts, p));
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_scan(&report);
            }
        }
        Command::Dupes {
            path,
            min_mb,
            walk,
            reclaim: reclaim_arg,
            apply,
        } => {
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            warn_invalid_globs(&walk.exclude);
            let opts = walk.to_options();
            let groups = with_progress(|p| {
                find_duplicates_with_progress(&path, min_mb * 1024 * 1024, &opts, p)
            });
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&groups).unwrap());
            } else {
                print_dupes(&groups);
            }
            if let Some(arg) = reclaim_arg {
                run_reclaim(&groups, arg.into(), !apply);
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
            let report = with_progress(|p| remove_path(&path, mode, !apply, p));
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_remove(&report, trash);
            }
            if !report.errors.is_empty() {
                return ExitCode::FAILURE;
            }
        }
    }

    ExitCode::SUCCESS
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

fn print_scan(r: &ScanReport) {
    println!("Scan of {}", r.root.display());
    println!(
        "  total: {} across {} files in {} dirs",
        human_size(r.total_size),
        r.total_files,
        r.total_dirs
    );
    if r.skipped > 0 {
        println!("  skipped: {} unreadable entries (permissions?)", r.skipped);
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
fn run_reclaim(groups: &[DupGroup], action: ReclaimAction, dry_run: bool) {
    let mut removed = 0usize;
    let mut reclaimed = 0u64;
    let mut errors = 0usize;
    for g in groups {
        if g.files.len() < 2 {
            continue;
        }
        let keep = &g.files[0];
        let report = reclaim(keep, &g.files[1..], g.size, action, dry_run);
        removed += report.removed;
        reclaimed += report.reclaimed;
        errors += report.errors.len();
        for e in &report.errors {
            eprintln!("  ! {e}");
        }
    }
    let mode = if dry_run {
        "DRY-RUN — nothing changed; pass --apply to act"
    } else {
        "applied"
    };
    print!(
        "\nReclaim [{mode}]: {removed} file(s), {} reclaimable",
        human_size(reclaimed)
    );
    if errors > 0 {
        print!(", {errors} error(s)");
    }
    println!();
}
