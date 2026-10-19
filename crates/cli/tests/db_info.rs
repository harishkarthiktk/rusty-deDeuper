//! `dedupe db info` after a real scan: asserts the labels and numeric fields
//! printed by `crates/cli/src/db.rs::info`.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;

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

#[test]
fn db_info_reports_stats_after_scan() {
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

    Command::new(BIN)
        .arg("db")
        .arg("--db")
        .arg(&db)
        .arg("info")
        .assert()
        .success()
        // schema_version: {version}
        .stdout(predicate::str::is_match(r"schema_version: \d+").unwrap())
        // groups_total:   {groups}
        .stdout(predicate::str::is_match(r"groups_total:\s+\d+").unwrap())
        // volume {id}: uuid={uuid} kind={uuid_kind} fs=... profile=... mount=...
        .stdout(predicate::str::is_match(r"volume \d+: uuid=\S* kind=\S+ ").unwrap())
        // "    states: {state}:{count} ..." (state is an integer code)
        .stdout(predicate::str::is_match(r"states: (-?\d+:\d+ ?)+").unwrap())
        // "    latest_run: {run}  stale_rows: {n}"
        .stdout(predicate::str::is_match(r"latest_run: \d+\s+stale_rows: \d+").unwrap());
}
