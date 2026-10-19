//! Scale tests: the Phase 1 resume and rescan invariants on a large tree.
//!
//! Both tests are `#[ignore]` and user-run:
//!
//!   DEDUPE_SCALE_N=1000000 cargo test -p dedupe-core --test scale -- --ignored
//!
//! `DEDUPE_SCALE_N` defaults to 1,000,000. Files are a few bytes each, so the
//! tree costs about one filesystem block per file on disk.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
use dedupe_core::traits::*;

// ---------------------------------------------------------------------------
// Test backends (copied from phase1.rs; kept local so this file stands alone)
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

#[allow(unused_mut, unused_assignments)]
fn to_meta(md: &fs::Metadata) -> FileMeta {
    let ft = md.file_type();
    let kind = if ft.is_file() {
        EntryKind::RegularFile
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else {
        EntryKind::Other
    };
    let mut file_id = None;
    let mut nlink = 1;
    let mut attr_bits = 0;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        file_id = Some(md.ino().to_le_bytes().to_vec());
        nlink = md.nlink() as i64;
        if nlink > 1 {
            attr_bits |= attrs::HARDLINK;
        }
    }
    let mtime_ns = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
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

struct FsReader {
    opens: AtomicU64,
}

impl FsReader {
    fn new() -> Self {
        FsReader {
            opens: AtomicU64::new(0),
        }
    }
}

impl ContentReader for FsReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let f = fs::File::open(path)?;
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(pread(&f, buf, offset)?)
    }
    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        let md = fs::symlink_metadata(path)?;
        Ok(to_meta(&md))
    }
    fn opens(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }
}

struct FakeVol {
    uuid: String,
}

impl FakeVol {
    fn new(uuid: &str) -> Self {
        FakeVol {
            uuid: uuid.to_string(),
        }
    }
}

impl VolumeIdentity for FakeVol {
    fn identify(&self, path: &Path) -> Result<VolumeInfo> {
        Ok(VolumeInfo {
            uuid: self.uuid.clone(),
            uuid_kind: UuidKind::FsUuid,
            // Mount at the scan root so the volume root folder == scan root.
            mount: Codec.path_to_raw(path),
            fs_type: FsType::Ext4,
            label: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn scale_n() -> usize {
    match std::env::var("DEDUPE_SCALE_N") {
        Ok(s) => s
            .parse()
            .expect("DEDUPE_SCALE_N must be a non-negative integer"),
        Err(_) => 1_000_000,
    }
}

/// `n` small files in folders of 1,000. Files with the same `i / 3` share
/// content (dup groups of 2 or 3), except every tenth file, which is unique.
fn populate_n(root: &Path, n: usize) {
    for d in 0..n.div_ceil(1000) {
        fs::create_dir_all(root.join(format!("d{d:05}"))).unwrap();
    }
    for i in 0..n {
        let body = if i % 10 == 9 {
            format!("unique {i}\n")
        } else {
            let g = i / 3;
            format!("group {g}\n{}", "x".repeat(g % 40))
        };
        let path = root
            .join(format!("d{:05}", i / 1000))
            .join(format!("f{i}.txt"));
        fs::write(path, body).unwrap();
    }
}

#[allow(clippy::too_many_arguments)]
fn scan(
    db: &Path,
    root: &Path,
    cfg: &Config,
    vi: &dyn VolumeIdentity,
    reader: &dyn ContentReader,
    resume: bool,
    stop: Option<i64>,
) -> Result<i64> {
    let walker = Walker;
    let codec = Codec;
    let hasher = Blake3Hasher;
    let cancel = Token::new();
    let sink = dedupe_core::progress::null_sink();
    let svc = Services {
        enumr: &walker,
        reader,
        codec: &codec,
        volume: vi,
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
            resume,
            stop_after: stop,
            no_lock: false,
        },
    )
}

fn report_json(db: &Path) -> String {
    let conn = Db::open_readonly(db).unwrap().conn;
    Report::from_db(&conn, 0).unwrap().to_json()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
#[ignore = "scale: user-run"]
fn one_million_files_resume_at_every_boundary_equals_fresh() {
    let n = scale_n();
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = dir.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    populate_n(&root, n);

    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let reader = FsReader::new();

    let full_db = dir.path().join("full.db");
    scan(&full_db, &root, &cfg, &vi, &reader, false, None).unwrap();
    let full = report_json(&full_db);

    for k in 0..=6i64 {
        let db = dir.path().join(format!("r{k}.db"));
        scan(&db, &root, &cfg, &vi, &reader, false, Some(k)).unwrap();
        scan(&db, &root, &cfg, &vi, &reader, true, None).unwrap();
        // assert! rather than assert_eq!: a mismatch would dump two huge reports.
        assert!(
            report_json(&db) == full,
            "resume at pass {k} diverged from fresh scan (N={n})"
        );
    }
}

#[test]
#[ignore = "scale: user-run"]
fn rescan_of_unchanged_million_files_does_zero_content_reads() {
    let n = scale_n();
    let dir = tempfile::tempdir().unwrap();
    let root: PathBuf = dir.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    populate_n(&root, n);

    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let db = dir.path().join("d.db");
    scan(&db, &root, &cfg, &vi, &FsReader::new(), false, None).unwrap();

    let reader = FsReader::new();
    scan(&db, &root, &cfg, &vi, &reader, false, None).unwrap();
    assert_eq!(
        reader.opens(),
        0,
        "unchanged rescan of {n} files must not read file content"
    );
}
