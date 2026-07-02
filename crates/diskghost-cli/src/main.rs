//! Diskghost CLI — fast disk usage & duplicate finder.
//!
//! Human-readable by default; `--json` emits machine-readable output for
//! scripts and agents (headless use).

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use diskghost_core::{find_duplicates, human_size, scan, DupGroup, ScanReport};

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
    },
    /// Find duplicate (byte-identical) files.
    Dupes {
        /// Directory to scan.
        path: PathBuf,
        /// Ignore files smaller than this many megabytes.
        #[arg(long, default_value_t = 1)]
        min_mb: u64,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();

    match cli.command {
        Command::Scan { path, top } => {
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            let report = scan(&path, top);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                print_scan(&report);
            }
        }
        Command::Dupes { path, min_mb } => {
            if !path.is_dir() {
                eprintln!("error: not a directory: {}", path.display());
                return ExitCode::FAILURE;
            }
            let groups = find_duplicates(&path, min_mb * 1024 * 1024);
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&groups).unwrap());
            } else {
                print_dupes(&groups);
            }
        }
    }

    ExitCode::SUCCESS
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
