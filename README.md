<h1 align="center">Diskghost 👻</h1>

<p align="center"><b>Fast disk-usage &amp; duplicate finder, written in Rust.</b><br>
Find where your space went and which files are wasting it — in seconds.</p>

---

Diskghost walks your filesystem in parallel, tells you which folders and files
eat the most space, and hunts down byte-identical duplicates. It has two faces:

- a **CLI** with `--json` output, so scripts and agents can drive it headless;
- a **modern GUI** *(coming next)* for pointing, clicking and reclaiming space.

Same fast Rust core behind both.

## Why it's fast

- **Parallel directory walk** (`jwalk` + `rayon`) — all cores, not one.
- **Cheap duplicate detection** — files are grouped by size first, and only the
  size-collisions are hashed. Hashing uses **BLAKE3** and runs in parallel.
- No indexing daemon, no database. Point it at a path, get an answer.

## Install

```bash
cargo install --path crates/diskghost-cli
# or build a release binary:
cargo build --release       # -> target/release/diskghost
```

## Usage

```bash
# Biggest sub-folders and files under a path
diskghost scan "C:\Users\me\Desktop" --top 20

# Duplicate files, ignoring anything under 5 MB
diskghost dupes "D:\Media" --min-mb 5
```

### Headless / agent mode

Add `--json` to any command for machine-readable output:

```bash
diskghost scan . --json
diskghost dupes . --min-mb 10 --json
```

`scan --json` returns total size, per-child-folder sizes and the biggest files;
`dupes --json` returns duplicate groups with their reclaimable bytes. Perfect
for wiring into an automation/agent that decides what to clean.

## Layout

```
Diskghost/
├── crates/
│   ├── diskghost-core/   the engine: scan + duplicate detection (a library)
│   └── diskghost-cli/    the `diskghost` command
└── (GUI lands here in phase 2)
```

## Roadmap

- [x] Core: parallel scan, folder/file sizes, duplicate detection
- [x] CLI with human + JSON output
- [ ] Modern GUI (treemap + duplicate browser)
- [ ] Interactive delete / move (with a dry-run guard)
- [ ] Signed release binaries for Windows/macOS/Linux

## License

MIT — see [LICENSE](LICENSE).
