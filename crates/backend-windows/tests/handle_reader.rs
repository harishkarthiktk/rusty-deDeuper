#![cfg(windows)]
//! Windows-only tests for `HandleReader` and `OverlappedFile`.
//!
//! They open real files in the std temp directory. The file-id tests need that directory on NTFS,
//! which is the default for a Windows user profile. These tests run on a Windows machine, since
//! the host cannot execute them. Run with `cargo test -p dedupe-backend-windows --test handle_reader`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dedupe_backend_generic::FsContentReader;
use dedupe_backend_windows::volume_io::frn_of;
use dedupe_backend_windows::{HandleReader, OverlappedFile};
use dedupe_core::traits::{AccessPattern, ContentReader, FileHandle, ReadTarget};

const CHUNK: usize = 64 * 1024;
const DEPTH: usize = 4;

/// A temp file removed on drop. The name combines process id, test name and time, so parallel
/// tests do not collide.
struct TempFile(PathBuf);

impl TempFile {
    fn with_bytes(test_name: &str, bytes: &[u8]) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let path = std::env::temp_dir().join(format!(
            "dedupe-handle-{}-{test_name}-{nanos}.bin",
            std::process::id()
        ));
        fs::write(&path, bytes).expect("temp file should be writable");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // Best effort. A file that is already gone means the test removed it itself.
        let _ = fs::remove_file(&self.0);
    }
}

/// Bytes with a period that does not divide the chunk size, so a read at the wrong offset
/// shows up as a mismatch.
fn pattern_bytes(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from(i % 251).unwrap_or_default())
        .collect()
}

/// Reads from offset 0 with buffers of `step` bytes until the handle reports end of file.
fn read_sequentially(handle: &mut dyn FileHandle, step: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; step];
    let mut offset = 0u64;
    loop {
        let n = handle
            .read_at(offset, &mut buf)
            .expect("sequential read should succeed");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        offset += u64::try_from(n).expect("a read count fits in u64");
    }
    out
}

#[test]
fn overlapped_file_should_match_std_read_when_sequential_reads_use_odd_buffer_sizes() {
    let content = pattern_bytes(CHUNK * 5 + 1234);
    let file = TempFile::with_bytes("odd_sizes", &content);
    let expected = fs::read(file.path()).expect("temp file should be readable");
    for step in [4099, CHUNK + 1] {
        let mut handle = OverlappedFile::open(file.path(), DEPTH, CHUNK)
            .expect("overlapped open should succeed");
        let got = read_sequentially(&mut handle, step);
        assert_eq!(
            got, expected,
            "sequential reads with {step}-byte buffers should match std::fs::read"
        );
    }
}

#[test]
fn overlapped_file_should_return_whole_file_when_file_is_shorter_than_chunk() {
    let content = pattern_bytes(100);
    let file = TempFile::with_bytes("short_file", &content);
    let mut handle =
        OverlappedFile::open(file.path(), DEPTH, CHUNK).expect("overlapped open should succeed");
    let mut buf = [0u8; 4096];
    let n = handle
        .read_at(0, &mut buf)
        .expect("read of a short file should succeed");
    assert_eq!(
        n,
        content.len(),
        "a read past a short file should stop at its end"
    );
    assert_eq!(
        &buf[..n],
        content.as_slice(),
        "bytes of a short file should match"
    );
}

#[test]
fn overlapped_file_should_return_requested_bytes_when_offset_jumps_after_sequential_reads() {
    let content = pattern_bytes(CHUNK * 4);
    let file = TempFile::with_bytes("offset_jump", &content);
    let mut handle =
        OverlappedFile::open(file.path(), DEPTH, CHUNK).expect("overlapped open should succeed");
    let mut first = [0u8; 1024];
    handle
        .read_at(0, &mut first)
        .expect("first sequential read should succeed");
    let jump = 2 * CHUNK + 17;
    let mut jumped = [0u8; 1024];
    let n = handle
        .read_at(jump as u64, &mut jumped)
        .expect("read after a jump should succeed");
    assert_eq!(
        &jumped[..n],
        &content[jump..jump + n],
        "a read at a non-sequential offset should return the bytes at that offset"
    );
}

#[test]
fn overlapped_file_should_not_hang_when_dropped_with_reads_in_flight() {
    let content = pattern_bytes(CHUNK * 8);
    let file = TempFile::with_bytes("drop_in_flight", &content);
    let mut handle =
        OverlappedFile::open(file.path(), DEPTH, CHUNK).expect("overlapped open should succeed");
    let mut one_byte = [0u8; 1];
    handle
        .read_at(0, &mut one_byte)
        .expect("first read should succeed and leave later chunks in flight");

    // Dropping on another thread also checks that the handle may move between threads.
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        drop(handle);
        // The receiver only waits for this message, so a send error is not a failure.
        let _ = done_tx.send(());
    });
    assert!(
        done_rx.recv_timeout(Duration::from_secs(30)).is_ok(),
        "dropping an OverlappedFile with reads in flight should finish within 30 s"
    );
}

#[test]
fn handle_reader_open_should_match_path_bytes_when_opened_by_file_id() {
    let content = pattern_bytes(CHUNK * 3 + 99);
    let file = TempFile::with_bytes("by_id", &content);
    let frn = frn_of(file.path()).expect("file reference should be readable");
    let id = frn.to_le_bytes();
    let reader = HandleReader::new(DEPTH, CHUNK);
    let mut handle = reader
        .open_target(ReadTarget {
            path: file.path(),
            file_id: Some(&id),
            access: AccessPattern::Sequential,
        })
        .expect("open by file id should succeed");
    let got = read_sequentially(&mut *handle, 4099);
    assert_eq!(
        got,
        fs::read(file.path()).expect("temp file should be readable"),
        "bytes read through the file id should match the bytes read by path"
    );
}

#[test]
fn handle_reader_open_target_should_return_path_bytes_when_file_id_is_garbage() {
    let content = pattern_bytes(2048);
    let file = TempFile::with_bytes("garbage_id", &content);
    let garbage = [0xFFu8; 8];
    let reader = HandleReader::new(DEPTH, CHUNK);
    let mut handle = reader
        .open_target(ReadTarget {
            path: file.path(),
            file_id: Some(&garbage),
            access: AccessPattern::Random,
        })
        .expect("a garbage file id should fall back to the path");
    let got = read_sequentially(&mut *handle, 512);
    assert_eq!(
        got, content,
        "fallback to the path should return the file's bytes"
    );
}

#[test]
fn handle_reader_open_target_should_return_path_bytes_when_file_id_is_stale() {
    let old_content = pattern_bytes(2048);
    let new_content: Vec<u8> = pattern_bytes(2048).into_iter().rev().collect();
    let file = TempFile::with_bytes("stale_id", &old_content);
    let stale_id = frn_of(file.path())
        .expect("file reference should be readable")
        .to_le_bytes();
    // Recreate the file at the same path. The old reference no longer names it.
    fs::remove_file(file.path()).expect("temp file should be removable");
    fs::write(file.path(), &new_content).expect("replacement temp file should be writable");

    let reader = HandleReader::new(DEPTH, CHUNK);
    let mut handle = reader
        .open_target(ReadTarget {
            path: file.path(),
            file_id: Some(&stale_id),
            access: AccessPattern::Random,
        })
        .expect("a stale file id should fall back to the path");
    let got = read_sequentially(&mut *handle, 512);
    assert_eq!(
        got, new_content,
        "a stale file id should read the file now at the path"
    );
}

#[test]
fn handle_reader_metadata_should_match_fs_reader_metadata() {
    let content = pattern_bytes(777);
    let file = TempFile::with_bytes("metadata", &content);
    let from_handle = HandleReader::new(DEPTH, CHUNK)
        .metadata(file.path())
        .expect("metadata through HandleReader should succeed");
    let from_fs = FsContentReader::new()
        .metadata(file.path())
        .expect("metadata through FsContentReader should succeed");
    assert_eq!(
        from_handle, from_fs,
        "HandleReader metadata should match FsContentReader metadata for the same file"
    );
}
