//! Criterion benchmarks for the two hot paths: scanning and duplicate finding.
//! Run with `cargo bench -p diskghost-core`.

use std::hint::black_box;
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion};
use diskghost_core::{find_duplicates, scan};

/// Build a throwaway tree: 1000 files across 20 sub-folders, with deliberate
/// duplicate content so the dup finder has real work.
fn setup() -> PathBuf {
    let mut dir = std::env::temp_dir();
    dir.push(format!("diskghost-bench-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for i in 0..1000u32 {
        let sub = dir.join(format!("dir{:02}", i % 20));
        std::fs::create_dir_all(&sub).unwrap();
        // ~50 distinct contents -> lots of duplicates to hash.
        let body = format!("diskghost benchmark payload block number {:04}", i % 50);
        std::fs::write(sub.join(format!("f{i}.bin")), body.as_bytes()).unwrap();
    }
    dir
}

fn benchmarks(c: &mut Criterion) {
    let dir = setup();

    c.bench_function("scan", |b| b.iter(|| scan(black_box(&dir), 20)));
    c.bench_function("find_duplicates", |b| {
        b.iter(|| find_duplicates(black_box(&dir), 1))
    });

    let _ = std::fs::remove_dir_all(&dir);
}

criterion_group!(benches, benchmarks);
criterion_main!(benches);
