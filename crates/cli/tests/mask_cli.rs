//! End-to-end mask flow: `report` masks by default, `report --unmasked` shows real
//! paths, and `unmask` rebuilds them from the vault. Covers the scope gate and `--ext-map`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{json, Value};

const BIN: &str = env!("CARGO_BIN_EXE_dedupe");

fn fixture_tree() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/tree")
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let target = dst.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn json_stdout(cmd: &mut Command) -> Value {
    let out = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).unwrap()
}

fn member_paths(doc: &Value) -> Vec<String> {
    let mut paths: Vec<String> = doc["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["members"].as_array().unwrap().iter())
        .map(|m| m["path"].as_str().unwrap().to_owned())
        .collect();
    paths.sort();
    paths
}

#[test]
fn masked_report_unmasks_to_real_paths() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    copy_dir(&fixture_tree(), &tree);
    let db = dir.path().join("x.db");

    Command::new(BIN)
        .arg("scan")
        .arg(&tree)
        .arg("--db")
        .arg(&db)
        .assert()
        .success();

    // Masked by default: export id present, no hashes, no real folder names.
    let masked = json_stdout(
        Command::new(BIN)
            .arg("report")
            .arg("--db")
            .arg(&db)
            .arg("--json"),
    );
    let masked_text = masked.to_string();
    assert!(masked.get("mask_export").is_some());
    assert!(!masked_text.contains("full_hash"));
    for name in ["nested", "deep", "sub"] {
        assert!(!masked_text.contains(name), "masked output leaks {name:?}");
    }

    let real = json_stdout(
        Command::new(BIN)
            .arg("report")
            .arg("--db")
            .arg(&db)
            .arg("--json")
            .arg("--unmasked"),
    );

    // Rebuild the masked plan. Its real paths must match the unmasked report.
    let masked_file = dir.path().join("masked.json");
    fs::write(&masked_file, serde_json::to_vec(&masked).unwrap()).unwrap();
    let unmasked = json_stdout(
        Command::new(BIN)
            .arg("unmask")
            .arg("--dir")
            .arg(&db)
            .arg(&masked_file),
    );
    assert_eq!(member_paths(&unmasked), member_paths(&real));
    assert!(unmasked.get("mask_export").is_none());

    // A token-free path to a unique file is outside every duplicate group, so it is refused.
    let export = masked["mask_export"].as_i64().unwrap();
    let only = tree.canonicalize().unwrap().join("only.txt");
    let spliced = json!({ "mask_export": export, "p": only.to_str().unwrap() });
    let spliced_file = dir.path().join("spliced.json");
    fs::write(&spliced_file, serde_json::to_vec(&spliced).unwrap()).unwrap();
    Command::new(BIN)
        .arg("unmask")
        .arg("--dir")
        .arg(&db)
        .arg(&spliced_file)
        .assert()
        .failure()
        .stderr(predicate::str::contains("not in a current duplicate group"));

    // The extension map needs no input file.
    Command::new(BIN)
        .arg("unmask")
        .arg("--dir")
        .arg(&db)
        .arg("--ext-map")
        .assert()
        .success();
}
