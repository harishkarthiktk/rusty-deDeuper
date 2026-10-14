//! Kill -9 fuzz (Unix): spawn the CLI as a subprocess, SIGKILL it mid-scan, then
//! `--resume` and assert the normalized report converges to a full scan.
//!
//! Unix-only; on other platforms the process is terminated via `Child::kill`.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread::sleep;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_dupfind");

fn build_tree(root: &Path) {
    fs::create_dir_all(root).unwrap();
    // Enough data that a scan is reliably still running when we kill it.
    let block: Vec<u8> = (0..4 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
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
        .stderr(Stdio::null());
    cmd.output().expect("run scan")
}

fn report(db: &Path) -> String {
    let out = Command::new(BIN)
        .args(["report", "--db"])
        .arg(db)
        .args(["--format", "json"])
        .output()
        .expect("run report");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Deterministic small random delay per iteration (no external rng).
fn delay_ms(iter: u64) -> u64 {
    let mut x = iter.wrapping_mul(2654435761).wrapping_add(12345);
    x ^= x >> 13;
    x = x.wrapping_mul(0x9E3779B1);
    2 + x % 40
}

#[test]
fn kill9_then_resume_converges() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    build_tree(&root);

    // Reference: one uninterrupted scan.
    let ref_db = dir.path().join("ref.db");
    let out = scan(&ref_db, &root, &[]);
    assert!(out.status.success(), "reference scan failed");
    let reference = report(&ref_db);

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
        let _ = child.kill();
        let _ = child.wait();

        // Resume; if the scan had already finished, `--resume` reports nothing to
        // resume, which is fine: the DB is complete either way.
        let _ = scan(&db, &root, &["--resume"]);
        let got = report(&db);
        assert_eq!(got, reference, "iteration {iter}: resume diverged");
    }
}
