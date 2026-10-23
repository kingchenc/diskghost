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
fn dupes_json_has_groups() {
    let out = Command::new(bin())
        .args(["dupes", sample_dir(), "--min-mb", "999999", "--json"])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&s).expect("valid JSON");
    assert!(v["groups"].as_array().unwrap().is_empty());
    assert!(v.get("reclaim").is_none() && v.get("warnings").is_none());
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

/// A successful `--json` run writes one JSON document to stdout and nothing to
/// stderr, so the output stays valid even when both streams are merged.
fn json_only(out: &std::process::Output) -> serde_json::Value {
    assert!(out.status.success(), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.is_empty(), "stderr should be empty:\n{err}");
    serde_json::from_slice(&out.stdout).expect("stdout is pure JSON")
}

#[test]
fn scan_json_save_reports_in_json() {
    let d = scratch("jsonsave");
    let data = d.join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("a.bin"), [0u8; 100]).unwrap();
    let snap = d.join("snap.json");

    let out = Command::new(bin())
        .args([
            "scan",
            data.to_str().unwrap(),
            "--json",
            "--save",
            snap.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    let v = json_only(&out);
    assert!(snap.is_file(), "snapshot file should exist");
    assert!(v.get("total_size").is_some());
    assert_eq!(v["snapshot_saved"].as_str(), snap.to_str());

    // Without --save there is no such key.
    let out = Command::new(bin())
        .args(["scan", data.to_str().unwrap(), "--json"])
        .output()
        .expect("run diskghost");
    assert!(json_only(&out).get("snapshot_saved").is_none());

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn scan_json_save_failure_leaves_stdout_empty() {
    let d = scratch("jsonsavefail");
    // A directory where the snapshot file should go cannot be overwritten.
    let taken = d.join("taken");
    std::fs::create_dir_all(&taken).unwrap();

    let out = Command::new(bin())
        .args([
            "scan",
            sample_dir(),
            "--json",
            "--save",
            taken.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty(), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot save snapshot"));

    std::fs::remove_dir_all(&d).ok();
}

/// Three copies of one file plus a unique one, under a fresh scratch dir.
fn dupes_fixture(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let d = scratch(label);
    let data = d.join("data");
    std::fs::create_dir_all(&data).unwrap();
    for name in ["a.bin", "b.bin", "c.bin"] {
        std::fs::write(data.join(name), [7u8; 50_000]).unwrap();
    }
    std::fs::write(data.join("unique.bin"), [9u8; 50_000]).unwrap();
    (d, data)
}

#[test]
fn dupes_json_reclaim_reports_in_json() {
    let (d, data) = dupes_fixture("jsonreclaim");

    let out = Command::new(bin())
        .args([
            "dupes",
            data.to_str().unwrap(),
            "--min-mb",
            "0",
            "--reclaim",
            "delete",
            "--json",
        ])
        .output()
        .expect("run diskghost");
    let v = json_only(&out);
    let groups = v["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0]["files"].as_array().unwrap().len(), 3);
    let r = &v["reclaim"];
    assert_eq!(r["dry_run"], true);
    assert_eq!(r["removed"], 2);
    assert_eq!(r["reclaimed"], 100_000);
    assert!(r["errors"].as_array().unwrap().is_empty());
    // A dry run changes nothing.
    for name in ["a.bin", "b.bin", "c.bin", "unique.bin"] {
        assert!(data.join(name).is_file(), "{name} should still exist");
    }

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn dupes_json_cache_warning_goes_into_json() {
    let (d, data) = dupes_fixture("jsonwarn");
    // A directory where the cache file should go cannot be overwritten.
    let taken = d.join("taken");
    std::fs::create_dir_all(&taken).unwrap();

    let out = Command::new(bin())
        .args([
            "dupes",
            data.to_str().unwrap(),
            "--min-mb",
            "0",
            "--cache",
            taken.to_str().unwrap(),
            "--json",
        ])
        .output()
        .expect("run diskghost");
    let v = json_only(&out);
    assert_eq!(v["groups"].as_array().unwrap().len(), 1);
    let warnings = v["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0]
        .as_str()
        .unwrap()
        .starts_with("cannot save hash cache"));

    // Without --json the warning stays on stderr.
    let out = Command::new(bin())
        .args([
            "dupes",
            data.to_str().unwrap(),
            "--min-mb",
            "0",
            "--cache",
            taken.to_str().unwrap(),
        ])
        .output()
        .expect("run diskghost");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("warning: cannot save hash cache"));

    std::fs::remove_dir_all(&d).ok();
}

#[test]
fn invalid_exclude_glob_fails() {
    for cmd in ["scan", "dupes"] {
        let out = Command::new(bin())
            .args([cmd, sample_dir(), "--exclude", "a[", "--json"])
            .output()
            .expect("run diskghost");
        assert!(!out.status.success(), "{cmd} should fail");
        assert!(out.stdout.is_empty(), "{cmd}: {out:?}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("invalid exclude glob(s): a["), "{cmd}: {err}");
    }
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
