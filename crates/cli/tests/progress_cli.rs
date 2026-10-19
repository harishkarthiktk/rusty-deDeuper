//! `dedupe scan --format json --progress always`: JSON stdout stays pure and
//! stderr carries no progress-bar output, even when progress is forced on.

use std::fs;
use std::path::{Path, PathBuf};

use assert_cmd::Command;

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
fn json_output_suppresses_progress_even_when_forced() {
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("tree");
    copy_dir(&fixture_tree(), &tree);
    let db = dir.path().join("x.db");

    let out = Command::new(BIN)
        .arg("scan")
        .arg(&tree)
        .arg("--db")
        .arg(&db)
        .args(["--format", "json", "--progress", "always"])
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let stdout = String::from_utf8(out.stdout).unwrap();
    serde_json::from_str::<serde_json::Value>(&stdout).expect("stdout is not valid JSON");

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains('\x1b'),
        "stderr has ANSI escape bytes: {stderr:?}"
    );
    // indicatif spinner/bar templates print "found [", "[bar]" and "pos/len".
    assert!(!stderr.contains('['), "stderr has bar text: {stderr:?}");
    assert!(!stderr.contains('%'), "stderr has percent text: {stderr:?}");
}
