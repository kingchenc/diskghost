<h1 align="center">Diskghost 👻</h1>

<p align="center"><b>Fast disk-usage &amp; duplicate finder, written in Rust.</b><br>
Find where your space went, hunt down byte-identical duplicates, and reclaim the space — from a CLI, a JSON API for agents, or a modern desktop app.</p>

---

## Features

- **Parallel scan** — total size, biggest sub-folders and biggest files,
  files-directly-in-root, and a count of unreadable/skipped entries. Also
  reports the drive's **free / total space** and how long the scan took. Sizes
  come straight from the directory listing (no file is opened), every scan has
  its own thread pool, and the tree is built while the walk is still running:
  4.9 M files / 1.2 TB on an NVMe SSD in about 6 s.
- **Correct, fast duplicate detection** — group by size → open every
  candidate **once** (collapsing **hard links**, so `wasted` is real, and
  hashing its first and last 16 KB) → full **BLAKE3** only for what still
  matches; every step parallel per file, big files hashed on several threads.
  An optional **hash cache** (`--cache`, automatic in the GUI) remembers hashes
  by path, size and modification time: searching the same place again only
  reads files that changed (a 461-group Steam library: 1.2 s → 0.1 s).
- **Reclaim duplicates** — delete, send to the **OS trash/recycle bin**, or
  replace a copy with a **hard link**. Dry-run by default.
- **Delete a file or folder** — `diskghost rm <path>`, permanently or to the OS
  trash. Parallel unlink for speed, never follows symlinks (can't escape the
  target), refuses a drive root, dry-run by default, reports how long it took.
- **Scan options** — exclude globs (`node_modules`, `*.tmp`; case-insensitive
  on Windows), max depth, follow-symlinks, `--threads` (2-4 suit a spinning
  disk), `--mft`.
- **NTFS MFT engine** (`--mft`, Windows, administrator) — reads the volume's
  Master File Table in large parallel blocks and builds the tree straight from
  its records instead of opening every folder. Measured on a system drive:
  `C:\Users\me` 196 s → 37 s, all of C: (4.8 M files, 4.9 M folders) in about
  30 s. On a SATA SSD with millions of files the table is bigger than the
  folders and the normal walk can be quicker. Cloud placeholders and OneDrive
  folders are sized and listed through Windows; without rights or NTFS it falls
  back to the walk and says why.
- **Live progress + cancel** — long scans report files/bytes and can be stopped;
  a cancelled scan is clearly labelled as a partial result.
- **One walk, whole tree** — the scan builds an in-memory tree, so the GUI
  drills into any sub-folder and back up instantly, without touching the disk
  again. Deleting from the GUI updates the tree in place.
- **What grew since last time** — save a scan as a snapshot and compare a
  later one against it: which folders grew or shrank, which are new, which are
  gone, and **every file** that appeared, disappeared or changed size. Every
  folder and file is recorded, sorted, front-coded and zstd-compressed (about
  8 bytes per file: 42 MB for 4.9 M files), and two snapshots are compared in a
  single streaming pass. The GUI keeps a history per scanned folder
  automatically.
- **Three faces, one Rust core**:
  - a **CLI** with `--json` output for scripts and agents (headless);
  - a **modern desktop GUI** (Tauri) with a dashboard per scan, several scans
    side by side, drive overview, exclude templates and one-click reclaim.

## Install

```bash
# Install the `diskghost` command globally, straight from GitHub (no crates.io):
cargo install --git https://github.com/kingchenc/diskghost diskghost-cli

# ...or from a local checkout:
cargo install --path crates/diskghost-cli

# ...or grab a prebuilt binary from the GitHub Releases page (Linux/macOS/Windows,
# each with a SHA256 checksum + build-provenance attestation) — no toolchain needed.
```

## CLI usage

```bash
# Biggest sub-folders and files
diskghost scan "C:\Users\me\Desktop" --top 20

# Scan options
diskghost scan . --exclude node_modules --exclude "*.tmp" --max-depth 3 --follow-symlinks

# Duplicates (ignore anything under 5 MB)
diskghost dupes "D:\Media" --min-mb 5

# Reclaim: dry-run first (default), then apply
diskghost dupes "D:\Media" --min-mb 5 --reclaim trash          # shows what would happen
diskghost dupes "D:\Media" --min-mb 5 --reclaim trash --apply  # actually acts
#   --reclaim delete | trash | hardlink   (one file per group is always kept)

# Delete a file or folder — dry-run first (default), then apply
diskghost rm "D:\old_project\target"            # shows how many files/dirs/bytes would go
diskghost rm "D:\old_project\target" --apply    # permanently delete (files unlinked in parallel)
diskghost rm "D:\old_project\target" --trash --apply   # send to the OS recycle bin instead
#   never follows symlinks (can't escape the target); refuses a drive root

# What grew since last time: save a snapshot, compare against it later
diskghost scan "C:\Users\me" --save before.dgs
diskghost scan "C:\Users\me" --since before.dgs          # scan + what grew/shrank/appeared/vanished, file by file
diskghost scan "C:\Users\me" --since before.dgs --save after.dgs
diskghost diff before.dgs after.dgs                      # compare two saved snapshots
diskghost diff before.dgs after.dgs --at "C:\Users\me\Downloads"   # ...at a sub-folder
```

A scheduled task (cron, Task Scheduler) that runs
`diskghost scan <dir> --since last.dgs --save last.dgs --json` gives you a
daily "what grew" report with nothing else to set up. Snapshots saved by
0.8 and earlier (JSON, folders only) are still read; against those, only
folder changes can be reported.

### Headless / agent mode

Add `--json` to any command for machine-readable output:

```bash
diskghost scan . --json          # total_size, children, top_files, root_files, skipped, cancelled …
diskghost dupes . --min-mb 10 --json   # duplicate groups with reclaimable bytes
diskghost scan . --since old.json --json   # the report plus a "diff" object (grown/shrunk/added/removed)
diskghost diff old.json new.json --json    # just the diff
```

Perfect for wiring into an automation/agent that decides what to clean.

## Desktop app (GUI)

A modern Tauri UI lives in `gui/`:

- **Many scans at once** — drop any number of folders onto the window, pick
  several with **Browse**, click a drive, or **Scan all drives**. Each scan gets
  its own entry in the sidebar with live progress (drive scans show an estimate
  against the drive's used space); a configurable number run in parallel, the
  rest wait in a queue — drives nothing is reading yet go first, so "Scan all
  drives" reads every disk at once. Switch between finished scans without
  losing your place; cancelling stops a walk at once.
- **Dashboard per scan** — headline cards (size and share of the drive, files and
  average size, folders, biggest folder and file, free space, scan speed and
  skipped entries), a **two-level treemap** (each big folder frames its own
  biggest sub-folders, with hover details), the drive split into this folder /
  everything else / free, and **what it is made of** by file type. Tabs below
  list folders, the biggest files (filterable), every extension, the spread over
  size ranges, and the changes since an earlier scan. Click anything to drill in
  — served from the in-memory tree, so it is instant; go back with **Back**, the
  mouse's back button or Alt+←, and up with **Up**, a right-click on the map,
  Backspace or Alt+↑. **Rescan** walks again.
- **Exclude templates** — switch on well-known build output, caches and system
  files per ecosystem (Node, Python, Rust, .NET, Gradle, C/C++, VCS, IDE folders,
  temp files, Windows/macOS system files …), turn single patterns off, or add
  your own; patterns are validated before they are used.
- **Settings** — scan engine (**Fast** for SSD/NVMe: many directories at once;
  **Gentle** for spinning disks, network drives or scanning in the background;
  **Ultra** = the NTFS MFT engine, with a *restart as administrator* button),
  list length, max depth, symlinks, parallel scans, snapshots kept per folder,
  duplicate minimum size, Recycle Bin vs. permanent delete, window size at start
  (1440p if it fits the screen, otherwise maximized; or 1080p / 4K / maximized),
  language. The version is shown next to the name.
- **Find duplicates** — every group is byte-identical content (BLAKE3 over the
  whole file), whatever the names. Copies inside installed programs and games
  (Steam, Epic, Battle.net, Program Files, …; DLLs, game archives) are there on
  purpose, so they are hidden by default, shown with a lock when asked for, and
  never removed while hidden (by default not even read: the search skips
  them); the program's copy is the one kept. The search shows its three phases
  with progress, and hashes are cached between runs. **Folder
  pairs** sums up which folders share identical files; filter by category or
  name, then reclaim with one click (dry-run first).
- **What changed** — every complete scan is kept as a snapshot (in the
  background, so the result shows at once), and right after a scan a **Since
  last scan** card compares it with the previous one. **Compare** / **Changes**
  lists the snapshots with their size on disk, compares the folder on screen
  against any of them — folders and single files — and tints the treemap and
  folder rows by what grew or shrank.
- **Never blocking** — every command runs off the UI thread and each scan has
  its own lock, so browsing one scan never waits for another.

From `gui/src-tauri`:

```bash
cargo tauri dev      # run the app
cargo tauri build    # bundle an installer
```

## Why it's fast — and correct

- Parallel directory walk on a thread pool per scan; a **single pass** counts
  files *and* directories, and builds the whole tree in memory (names only, not
  full paths) so navigating it costs no further I/O.
- File sizes come from the directory listing itself — on Windows no file is
  opened at all, which used to be most of a scan's cost.
- Where a disk holds millions of tiny folders, opening the folders themselves
  becomes the limit: reading the NTFS master file table directly would be faster
  still, but needs administrator rights, so Diskghost does not do it.
- Duplicate detection never hashes more than it must: size buckets first, hard
  links collapsed, a cheap first-block pre-hash, then BLAKE3 — streamed, so large
  files never load into RAM.
- Hard links are not counted as duplicates, so the "reclaimable" number is the
  space you actually get back.

## Layout

```
Diskghost/
├── crates/
│   ├── diskghost-core/   the engine: scan tree, duplicates, reclaim, snapshots + diff (a library)
│   └── diskghost-cli/    the `diskghost` command
└── gui/                  Tauri desktop app (modern dark UI; ui/lib.js has Node unit tests)
```

## Roadmap

- [x] Core: parallel scan, folder/file sizes, hard-link-aware duplicate detection
- [x] CLI with human + JSON output, scan options, reclaim (delete/trash/hardlink)
- [x] Live progress + cancellation
- [x] Modern GUI: treemap, folder picker, drag &amp; drop, one-click reclaim
- [x] Delete files/folders (CLI `rm` + GUI) — permanent or to the OS trash, parallel unlink
- [x] Drive free/total space + scan &amp; delete timings (CLI + GUI)
- [x] CI (fmt/clippy/test/bench on 3 OS) + release binaries with checksums + GUI installers
- [x] Interactive treemap drill-up / breadcrumbs, served from an in-memory tree
- [x] Snapshots &amp; "what grew since last time" (CLI `--save`/`--since`/`diff`, GUI history)
- [x] GUI dashboard: parallel scans + queue, drives, nested treemap, file-type and size-range charts, exclude templates, settings
- [x] Own parallel walker (no per-file open), file-level snapshots (zstd, streaming diff), non-blocking GUI
- [x] Automatic "since last scan" comparison; duplicate finder that protects program and game files, folder pairs
- [x] NTFS MFT engine (administrator), per-file parallel duplicate pipeline with a persistent hash cache

## Development

```bash
cargo test --workspace                                   # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings    # lints (CI gate)
cargo bench -p diskghost-core                             # criterion benchmarks
node --test gui/ui/lib.test.js                            # GUI helper unit tests
```

## License

MIT — see [LICENSE](LICENSE).
