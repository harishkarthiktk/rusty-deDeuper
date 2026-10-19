//! Hash-pool determinism: `storage_profile.queue_depth` must change speed only,
//! never results. A scan with depth 4 must match a scan with depth 1.
//!
//! The harness (walker, codec, real-FS reader, fake volume) is copied from
//! `phase1.rs` on purpose. Each test file stands alone, so no shared test module
//! couples unrelated tests.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;

use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::db::Db;
use dedupe_core::error::Result;
use dedupe_core::hash::Blake3Hasher;
#[cfg(unix)]
use dedupe_core::model::attrs;
use dedupe_core::model::{FsType, UuidKind};
use dedupe_core::path::RawPath;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::Services;
use dedupe_core::report::Report;
use dedupe_core::storage_profile::Profile;
use dedupe_core::traits::*;
use rusqlite::Connection;

type TestResult = std::result::Result<(), Box<dyn Error>>;

// ---------------------------------------------------------------------------
// Test backends
// ---------------------------------------------------------------------------

struct Codec;

impl PathCodec for Codec {
    #[cfg(unix)]
    fn to_raw(&self, path: &OsStr) -> RawPath {
        use std::os::unix::ffi::OsStrExt;
        RawPath::new(path.as_bytes().to_vec())
    }
    // Windows names are UTF-16; the test fixtures only use UTF-8 names there.
    #[cfg(windows)]
    fn to_raw(&self, path: &OsStr) -> RawPath {
        RawPath::new(path.to_string_lossy().as_bytes().to_vec())
    }
    #[cfg(unix)]
    fn to_native(&self, path: &RawPath) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(path.0.clone())
    }
    #[cfg(windows)]
    fn to_native(&self, path: &RawPath) -> OsString {
        OsString::from(String::from_utf8_lossy(&path.0).into_owned())
    }
    fn split_component(&self, path: &Path) -> (RawPath, RawPath) {
        let parent = path
            .parent()
            .map(|p| self.path_to_raw(p))
            .unwrap_or_default();
        let name = path.file_name().map(|n| self.to_raw(n)).unwrap_or_default();
        (parent, name)
    }
}

struct Walker;

impl Enumerator for Walker {
    fn walk(
        &self,
        root: &Path,
        cfg: &Config,
        visit: &mut dyn FnMut(WalkItem) -> Result<()>,
    ) -> Result<()> {
        let root_dev = fs::metadata(root).ok().map(|md| dev_of(&md));
        visit(WalkItem::Dir {
            path: Codec.path_to_raw(root),
        })?;
        recurse(root, root_dev, cfg, visit)
    }
}

fn recurse(
    dir: &Path,
    root_dev: Option<u64>,
    cfg: &Config,
    visit: &mut dyn FnMut(WalkItem) -> Result<()>,
) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        let md = fs::symlink_metadata(&p)?;
        if md.is_dir() {
            if cfg.one_file_system && root_dev.is_some_and(|d| dev_of(&md) != d) {
                continue;
            }
            visit(WalkItem::Dir {
                path: Codec.path_to_raw(&p),
            })?;
            recurse(&p, root_dev, cfg, visit)?;
        } else {
            visit(WalkItem::File {
                path: Codec.path_to_raw(&p),
                meta: to_meta(&md),
            })?;
        }
    }
    Ok(())
}

#[cfg(unix)]
fn dev_of(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.dev()
}

#[cfg(not(unix))]
fn dev_of(_md: &fs::Metadata) -> u64 {
    0
}

fn to_meta(md: &fs::Metadata) -> FileMeta {
    let ft = md.file_type();
    let kind = if ft.is_file() {
        EntryKind::RegularFile
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    #[cfg(unix)]
    let (file_id, nlink, attr_bits) = {
        use std::os::unix::fs::MetadataExt;
        let nlink = md.nlink() as i64;
        let bits = if nlink > 1 { attrs::HARDLINK } else { 0 };
        (Some(md.ino().to_le_bytes().to_vec()), nlink, bits)
    };
    #[cfg(not(unix))]
    let (file_id, nlink, attr_bits) = (None, 1, 0);
    FileMeta {
        dev: None,
        file_id,
        nlink,
        size: md.len() as i64,
        mtime_ns,
        ctime_ns: None,
        attrs: attr_bits,
        kind,
    }
}

/// Real-FS reader. Opens the file on every `read_at`.
/// Positional read with no shared cursor. `pread` on unix, `seek_read` on Windows.
#[cfg(unix)]
fn pread(f: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::unix::fs::FileExt;
    f.read_at(buf, offset)
}

#[cfg(windows)]
fn pread(f: &fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_read(buf, offset)
}

struct FsReader;

impl ContentReader for FsReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let f = fs::File::open(path)?;
        Ok(pread(&f, buf, offset)?)
    }
    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        let md = fs::symlink_metadata(path)?;
        Ok(to_meta(&md))
    }
}

/// Every scan uses the same fake volume, mounted at the scan root.
struct FakeVol;

impl VolumeIdentity for FakeVol {
    fn identify(&self, path: &Path) -> Result<VolumeInfo> {
        Ok(VolumeInfo {
            uuid: "vol-A".to_string(),
            uuid_kind: UuidKind::FsUuid,
            mount: Codec.path_to_raw(path),
            fs_type: FsType::Ext4,
            label: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Writes `rel` under `root`, creating parent directories.
fn put(root: &Path, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(p, bytes)
}

/// A tree that covers the cases the pool must not reorder or lose:
/// duplicate small files in nested folders, a unique-size file, an empty file,
/// a same-size pair, a large duplicate pair spanning several read chunks, and a
/// large same-size file that differs only in the middle (equal prefix, unequal
/// full hash).
fn populate(root: &Path) -> std::io::Result<()> {
    put(root, "a.txt", b"hello world\n")?;
    put(root, "sub/a_copy.txt", b"hello world\n")?;
    put(root, "nested/deep/dup.txt", b"hello world\n")?;
    put(root, "b.txt", b"unique content\n")?;
    put(root, "empty.txt", b"")?;
    put(root, "f1.bin", &[b'A'; 100])?;
    put(root, "f2.bin", &[b'A'; 100])?;
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
    put(root, "big1.bin", &big)?;
    put(root, "big2.bin", &big)?;
    let mut mid = big.clone();
    mid[150_000] = mid[150_000].wrapping_add(1);
    put(root, "big_mid.bin", &mid)?;
    Ok(())
}

/// Default config with only the queue depth changed. Everything else stays at
/// the library defaults, so the two scans differ in nothing but the pool.
fn cfg_with_depth(queue_depth: usize) -> Config {
    Config {
        profile: Profile {
            queue_depth,
            ..Profile::default()
        },
        ..Config::default()
    }
}

fn scan_run(db: &Path, root: &Path, cfg: &Config) -> Result<i64> {
    let walker = Walker;
    let codec = Codec;
    let hasher = Blake3Hasher;
    let volume = FakeVol;
    let reader = FsReader;
    let cancel = Token::new();
    let sink = dedupe_core::progress::null_sink();
    let svc = Services {
        enumr: &walker,
        reader: &reader,
        codec: &codec,
        volume: &volume,
        hasher: &hasher,
        cancel: &cancel,
        sink: &sink,
    };
    run_scan(
        db,
        &[root.to_path_buf()],
        cfg,
        &svc,
        &ScanOptions {
            cli_args: "test".to_string(),
            resume: false,
            stop_after: None,
        },
    )
}

fn report_json(db: &Path) -> std::result::Result<String, Box<dyn Error>> {
    let conn = Db::open_readonly(db)?.conn;
    Ok(Report::from_db(&conn, 0)?.to_json())
}

/// `(state, full_hash)` for every `files` row, sorted. Names and ids are left out
/// on purpose: they are path- and insert-order-dependent, and the claim is about
/// content results only.
fn state_hashes(db: &Path) -> rusqlite::Result<Vec<(i64, Option<Vec<u8>>)>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT state, full_hash FROM files ORDER BY state, full_hash")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn full_hash_should_match_sequential_when_queue_depth_is_four() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("tree");
    fs::create_dir_all(&root)?;
    populate(&root)?;
    let sequential_db = dir.path().join("sequential.db");
    let pooled_db = dir.path().join("pooled.db");

    scan_run(&sequential_db, &root, &cfg_with_depth(1))?;
    scan_run(&pooled_db, &root, &cfg_with_depth(4))?;

    // Guard: an empty result on both sides would make the comparison vacuous.
    let sequential_rows = state_hashes(&sequential_db)?;
    assert!(
        !sequential_rows.is_empty(),
        "sequential scan produced no file rows, so the comparison would prove nothing"
    );

    let sequential_json = report_json(&sequential_db)?;
    let pooled_json = report_json(&pooled_db)?;
    assert_eq!(
        pooled_json, sequential_json,
        "JSON report with queue_depth 4 differs from queue_depth 1"
    );

    let pooled_rows = state_hashes(&pooled_db)?;
    assert_eq!(
        pooled_rows, sequential_rows,
        "(state, full_hash) rows with queue_depth 4 differ from queue_depth 1"
    );
    Ok(())
}
