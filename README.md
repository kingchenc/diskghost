<h1 align="center">Diskghost 👻</h1>

<p align="center"><b>Fast disk-usage &amp; duplicate finder, written in Rust.</b><br>
Find where your space went, hunt down byte-identical duplicates, and reclaim the space — from a CLI, a JSON API for agents, or a modern desktop app.</p>

---

## Features

- **Parallel scan** (`jwalk` + `rayon`) — total size, biggest sub-folders and
  biggest files, files-directly-in-root, and a count of unreadable/skipped entries.
- **Correct duplicate detection** — group by size → **collapse hard links**
  (same physical file, so `wasted` is real) → **first-block pre-hash** → full
  **BLAKE3** hash, all in parallel.
- **Reclaim space** — delete, send to the **OS trash/recycle bin**, or replace a
  copy with a **hard link**. Dry-run by default.
- **Scan options** — exclude globs (`node_modules`, `*.tmp`), max depth,
  follow-symlinks.
- **Live progress + cancel** — long scans report files/bytes and can be stopped.
- **Three faces, one Rust core**:
  - a **CLI** with `--json` output for scripts and agents (headless);
  - a **modern desktop GUI** (Tauri) with a treemap, folder picker, drag &amp; drop
    and one-click reclaim.

## Install

```bash
cargo install --path crates/diskghost-cli    # the `diskghost` command
# or grab a prebuilt binary from the GitHub Releases page (Linux/macOS/Windows,
# each with a SHA256 checksum + build-provenance attestation).
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
```

### Headless / agent mode

Add `--json` to any command for machine-readable output:

```bash
diskghost scan . --json          # total_size, children, top_files, root_files, skipped …
diskghost dupes . --min-mb 10 --json   # duplicate groups with reclaimable bytes
```

Perfect for wiring into an automation/agent that decides what to clean.

## Desktop app (GUI)

A modern Tauri UI lives in `gui/` — **Browse** or drag a folder onto the window,
hit **Scan size** for a treemap + biggest folders/files (click a folder to drill
in), or **Find duplicates** to sort/filter groups and reclaim their space with one
click. Scans show live progress and can be cancelled. From `gui/src-tauri`:

```bash
cargo tauri dev      # run the app
cargo tauri build    # bundle an installer
```

## Why it's fast — and correct

- Parallel directory walk across all cores; a **single pass** counts files *and*
  directories.
- Duplicate detection never hashes more than it must: size buckets first, hard
  links collapsed, a cheap first-block pre-hash, then BLAKE3 — streamed, so large
  files never load into RAM.
- Hard links are not counted as duplicates, so the "reclaimable" number is the
  space you actually get back.

## Layout

```
Diskghost/
├── crates/
│   ├── diskghost-core/   the engine: scan + duplicate detection + reclaim (a library)
│   └── diskghost-cli/    the `diskghost` command
└── gui/                  Tauri desktop app (modern dark UI)
```

## Roadmap

- [x] Core: parallel scan, folder/file sizes, hard-link-aware duplicate detection
- [x] CLI with human + JSON output, scan options, reclaim (delete/trash/hardlink)
- [x] Live progress + cancellation
- [x] Modern GUI: treemap, folder picker, drag &amp; drop, one-click reclaim
- [x] CI (fmt/clippy/test/bench on 3 OS) + signed release binaries
- [ ] Interactive treemap drill-up / breadcrumbs
- [ ] Scheduled scans &amp; "what grew since last time"

## Development

```bash
cargo test --workspace                                   # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings    # lints (CI gate)
cargo bench -p diskghost-core                             # criterion benchmarks
```

## License

MIT — see [LICENSE](LICENSE).
