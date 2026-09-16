//! Integration tests: drive the actual compiled `diskghost` binary.
//! Cargo exposes its path via CARGO_BIN_EXE_<name>.

use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_diskghost")
}

/// A directory that always exists (this crate's own source tree).
fn sample_dir() -> &'static str {
    env!("CARGO_MANIFEST_DIR")
}

#[test]
fn scan_human_output() {
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--top", "3"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("total:"), "missing total line:\n{s}");
    assert!(s.contains("Biggest sub-folders:"));
}

#[test]
fn scan_json_is_valid() {
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v.get("total_size").is_some());
    assert!(v.get("root_files_count").is_some());
    assert!(v.get("skipped").is_some());
}

#[test]
fn dupes_json_is_array() {
    let out = Command::new(bin())
        .args(["dupes", sample_dir(), "--min-mb", "999999", "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v.is_array());
}

#[test]
fn exclude_flag_is_accepted() {
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--exclude", "target", "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
}

#[test]
fn bad_path_fails() {
    let out = Command::new(bin())
        .args(["scan", "definitely-not-a-real-path-xyz-123"])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
}

#[test]
fn reclaim_defaults_to_dry_run() {
    // Without --apply, a reclaim must not change anything and must say DRY-RUN.
    let out = Command::new(bin())
        .args([
            "dupes",
            sample_dir(),
            "--min-mb",
            "0",
            "--reclaim",
            "delete",
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("DRY-RUN"), "expected dry-run notice:\n{s}");
}

/// A throwaway directory unique to this test process.
fn scratch(label: &str) -> std::path::PathBuf {
    let mut d = std::env::temp_dir();
    d.push(format!("diskghost-cli-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn scan_save_then_since_and_diff() {
    let d = scratch("snap");
    let data = d.join("data");
    std::fs::create_dir_all(data.join("keep")).unwrap();
    std::fs::write(data.join("keep/a.bin"), [0u8; 100]).unwrap();
    let old = d.join("old.json");
    let new = d.join("new.json");

    // First scan saves a snapshot; nothing to compare yet.
    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--save",
            old.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    assert!(old.is_file(), "snapshot file should exist");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("snapshot saved"), "stderr:\n{err}");

    // Grow one folder, add another, then compare while saving again.
    std::fs::write(data.join("keep/b.bin"), [0u8; 100_000]).unwrap();
    std::fs::create_dir_all(data.join("fresh")).unwrap();
    std::fs::write(data.join("fresh/c.bin"), [0u8; 100]).unwrap();
    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--since",
            old.to_str().unwrap(),
            "--save",
            new.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Changes in"), "missing diff section:\n{s}");
    assert!(s.contains("Grew:"), "keep should have grown:\n{s}");
    assert!(s.contains("New folders:"), "fresh should be new:\n{s}");

    // JSON mode nests the diff next to the report fields.
    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--since",
            old.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
    assert!(v.get("total_size").is_some());
    let diff = v.get("diff").expect("diff key");
    assert_eq!(diff["grown"].as_array().unwrap().len(), 1);
    assert_eq!(diff["added"].as_array().unwrap().len(), 1);
    assert!(diff["delta"].as_i64().unwrap() > 0);

    // Standalone diff of the two saved snapshots, human and JSON.
    let out = Command::new(bin())
        .args(["diff", old.to_str().unwrap(), new.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("Grew:") && s.contains("New folders:"), "{s}");

    let out = Command::new(bin())
        .args([
            "diff",
            old.to_str().unwrap(),
            new.to_str().unwrap(),
            "--at",
            data.join("keep").to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("valid JSON");
    assert_eq!(v["files_delta"].as_i64().unwrap(), 1);
    assert!(v["grown"].as_array().unwrap().is_empty()); // keep has no sub-folders

    // Comparing identical snapshots says so.
    let out = Command::new(bin())
        .args(["diff", new.to_str().unwrap(), new.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("nothing changed"));

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn snapshot_errors_are_reported() {
    let d = scratch("snaperr");
    let bad = d.join("bad.json");
    std::fs::write(&bad, "not json").unwrap();

    // A broken --since fails before scanning.
    let out = Command::new(bin())
        .args(["scan", sample_dir(), "--since", bad.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot read snapshot"));

    // A missing file for diff fails too.
    let out = Command::new(bin())
        .args([
            "diff",
            bad.to_str().unwrap(),
            d.join("none.json").to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());

    // Snapshots of different roots cannot be compared.
    let a = d.join("a");
    let b = d.join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let sa = d.join("a.json");
    let sb = d.join("b.json");
    for (dir, file) in [(&a, &sa), (&b, &sb)] {
        let out = Command::new(bin())
            .args([
                "scan",
                dir.to_str().unwrap(),
                "--save",
                file.to_str().unwrap(),
            ])
            .output()
            .expect("run diskghost");
        assert!(out.status.success());
    }
    let out = Command::new(bin())
        .args(["diff", sa.to_str().unwrap(), sb.to_str().unwrap()])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("different directories"));

    // --at must name a folder inside the newer snapshot.
    let out = Command::new(bin())
        .args([
            "diff",
            sa.to_str().unwrap(),
            sa.to_str().unwrap(),
            "--at",
            a.join("nope").to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory in the snapshot"));

    std::fs::remove_dir_all(&d).ok();
}
