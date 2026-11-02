//! Kill -9 fuzz (Unix): spawn the CLI as a subprocess, SIGKILL it mid-scan, then
//! `--resume` and assert the raw JSON report converges to a full scan.
//!
//! Unix-only; on other platforms the process is terminated via `Child::kill`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_dedupe");

fn build_tree(root: &Path) {
    fs::create_dir_all(root).unwrap();
    // Enough data that a scan is reliably still running when we kill it.
    let block: Vec<u8> = (0..64 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    for i in 0..4 {
        fs::write(root.join(format!("dup_a_{i}.bin")), &block).unwrap();
        fs::write(root.join(format!("dup_b_{i}.bin")), &block).unwrap();
    }
    let mut other = block.clone();
    other[0] = other[0].wrapping_add(1);
    fs::write(root.join("unique.bin"), &other).unwrap();
    fs::write(root.join("small.txt"), b"hello world\n").unwrap();
    fs::write(root.join("small_copy.txt"), b"hello world\n").unwrap();
}

fn scan(db: &Path, root: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("scan")
        .arg(root)
        .arg("--db")
        .arg(db)
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.output().expect("run scan")
}

fn report(db: &Path) -> serde_json::Value {
    let out = Command::new(BIN)
        .args(["report", "--db"])
        .arg(db)
        .args(["--format", "json"])
        // the comparison needs real paths, not per-run mask tokens
        .arg("--unmasked")
        .output()
        .expect("run report");
    assert!(
        out.status.success(),
        "report failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("report stdout should be JSON");
    assert!(
        value.get("groups").is_some(),
        "report JSON should be an object with \"groups\": {value}"
    );
    value
}

/// Deterministic small random delay per iteration (no external rng).
fn delay_ms(iter: u64) -> u64 {
    let mut x = iter.wrapping_mul(2654435761).wrapping_add(12345);
    x ^= x >> 13;
    x = x.wrapping_mul(0x9E3779B1);
    100 + x % 200
}

#[test]
#[ignore = "nightly: 20-iteration kill-9 fuzz, too slow for per-push CI"]
fn kill9_then_resume_converges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_tree(&root);

    // Reference: one uninterrupted scan.
    let ref_db = dir.path().join("ref.db");
    let out = scan(&ref_db, &root, &[]);
    assert!(
        out.status.success(),
        "reference scan failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let reference = report(&ref_db);
    assert!(
        reference["groups"]
            .as_array()
            .is_some_and(|g| !g.is_empty()),
        "reference report should contain duplicate groups: {reference}"
    );
    let (mut interrupted, mut finished_early) = (0u32, 0u32);

    for iter in 0..20u64 {
        let db = dir.path().join(format!("fuzz{iter}.db"));
        let _ = fs::remove_file(&db);

        // Spawn and kill mid-scan.
        let mut child = Command::new(BIN)
            .arg("scan")
            .arg(&root)
            .arg("--db")
            .arg(&db)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn scan");
        sleep(Duration::from_millis(delay_ms(iter)));
        if child.try_wait().expect("poll scan").is_some() {
            finished_early += 1;
        } else {
            let _ = child.kill();
            let _ = child.wait();
            interrupted += 1;
        }

        // A scan that finished before the kill leaves no running run, so `--resume` fails with
        // "nothing to resume"; the DB is complete and the report check below still applies.
        let out = scan(&db, &root, &["--resume"]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success() || stderr.contains("nothing to resume"),
            "iteration {iter}: resume failed: {stderr}"
        );
        let got = report(&db);
        assert_eq!(got, reference, "iteration {iter}: resume diverged");
    }

    assert!(
        interrupted > 0,
        "no iteration interrupted the scan ({finished_early} finished early); enlarge the tree or shorten delays"
    );
}
