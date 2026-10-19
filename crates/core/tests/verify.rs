//! Verify-mode integration tests: byte verification (`VerifyMode::Byte`) and the
//! promotion of pending rows when verification is off.
//!
//! The harness (walker, codec, real-FS reader, fake volume) is copied from
//! `phase1.rs` on purpose. Each test file stands alone, so no shared test module
//! couples unrelated tests.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use dedupe_core::cancel::Token;
use dedupe_core::config::{Config, VerifyMode};
use dedupe_core::db::Db;
use dedupe_core::error::Result;
use dedupe_core::hash::Blake3Hasher;
#[cfg(unix)]
use dedupe_core::model::attrs;
use dedupe_core::model::{ErrorKind, FsType, Phase, State, UuidKind};
use dedupe_core::path::RawPath;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::Services;
use dedupe_core::traits::*;
use rusqlite::Connection;

type TestResult = std::result::Result<(), Box<dyn Error>>;

/// Maximum bytes a [`ShortReader`] returns per call. Odd and small on purpose,
/// so every chunk needs several calls.
const SHORT_READ_MAX: usize = 7;

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
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
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

/// Real-FS reader. Opens the file on every `read_at`, which is the default
/// `ContentReader::open` behavior.
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

/// Returns at most [`SHORT_READ_MAX`] bytes per `read_at` call, as a backend may
/// on a partial read. Everything else is delegated to the real-FS reader.
struct ShortReader {
    inner: FsReader,
}

impl ContentReader for ShortReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let n = buf.len().min(SHORT_READ_MAX);
        self.inner.read_at(path, offset, &mut buf[..n])
    }
    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        self.inner.metadata(path)
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

/// A temp dir holding a scan root `tree/` and a DB path next to it.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: PathBuf,
}

impl Fixture {
    fn new() -> std::io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("tree");
        fs::create_dir_all(&root)?;
        let db = dir.path().join("d.db");
        Ok(Self {
            _dir: dir,
            root,
            db,
        })
    }

    /// Writes a file directly under the scan root.
    fn put(&self, rel: &str, bytes: &[u8]) -> std::io::Result<()> {
        fs::write(self.root.join(rel), bytes)
    }
}

/// Config with byte verification on.
fn byte_cfg() -> Config {
    Config {
        verify: VerifyMode::Byte,
        ..Config::default()
    }
}

fn scan_run(
    db: &Path,
    root: &Path,
    cfg: &Config,
    reader: &dyn ContentReader,
    resume: bool,
    stop_after: Option<i64>,
) -> Result<i64> {
    let walker = Walker;
    let codec = Codec;
    let hasher = Blake3Hasher;
    let volume = FakeVol;
    let cancel = Token::new();
    let sink = dedupe_core::progress::null_sink();
    let svc = Services {
        enumr: &walker,
        reader,
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
            resume,
            stop_after,
        },
    )
}

fn open_ro(db: &Path) -> Result<Connection> {
    Ok(Db::open_readonly(db)?.conn)
}

/// Every `files.state`, sorted ascending.
fn state_list(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT state FROM files ORDER BY state")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `(name, state)` for every `files` row, sorted by name.
fn states_by_name(conn: &Connection) -> rusqlite::Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare("SELECT name, state FROM files ORDER BY name")?;
    let rows = stmt
        .query_map([], |r| {
            let name: Vec<u8> = r.get(0)?;
            let state: i64 = r.get(1)?;
            Ok((String::from_utf8_lossy(&name).into_owned(), state))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `(files.id, files.state)` for the row with this name.
fn row_id_and_state(conn: &Connection, name: &str) -> rusqlite::Result<(i64, i64)> {
    conn.query_row(
        "SELECT id, state FROM files WHERE name = ?1",
        [name.as_bytes()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
}

/// `errors.kind` for every error row that points at this file row.
fn error_kinds(conn: &Connection, file_row_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT kind FROM errors WHERE file_row_id = ?1 ORDER BY id")?;
    let rows = stmt
        .query_map([file_row_id], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn error_count(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT COUNT(*) FROM errors", [], |r| r.get(0))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn verify_should_set_state_4_when_files_identical() -> TestResult {
    let fx = Fixture::new()?;
    fx.put("dup1.txt", b"hello world\n")?;
    fx.put("dup2.txt", b"hello world\n")?;

    scan_run(&fx.db, &fx.root, &byte_cfg(), &FsReader, false, None)?;

    let conn = open_ro(&fx.db)?;
    let states = state_list(&conn)?;
    assert_eq!(
        states,
        vec![4, 4],
        "both identical files should be verified (state 4), got {states:?}"
    );
    Ok(())
}

#[test]
fn verify_should_error_when_file_changed_after_hash() -> TestResult {
    let fx = Fixture::new()?;
    fx.put("dup1.txt", b"hello world\n")?;
    fx.put("dup2.txt", b"hello world\n")?;
    // Stop after full hashing, so the rows wait in state 3 for verification.
    scan_run(
        &fx.db,
        &fx.root,
        &byte_cfg(),
        &FsReader,
        false,
        Some(Phase::FullDone.as_i64()),
    )?;

    // Same length, different bytes, and a later mtime. The stored size and mtime
    // no longer match the file on disk.
    let changed = fx.root.join("dup2.txt");
    fs::write(&changed, b"HELLO WORLD\n")?;
    fs::OpenOptions::new()
        .write(true)
        .open(&changed)?
        .set_modified(SystemTime::now() + Duration::from_secs(3600))?;

    scan_run(&fx.db, &fx.root, &byte_cfg(), &FsReader, true, None)?;

    let conn = open_ro(&fx.db)?;
    let (id, state) = row_id_and_state(&conn, "dup2.txt")?;
    assert_eq!(
        state,
        State::Error.as_i64(),
        "changed file should end in state -2, got {state}"
    );
    // Exactly ChangedDuringRead. The re-stat that runs after the byte compare
    // reports the changed file first, so VerifyMismatch (5) is unreachable in
    // this scenario. Pinning the kind makes a change in that ordering visible.
    let kinds = error_kinds(&conn, id)?;
    assert_eq!(
        kinds,
        vec![ErrorKind::ChangedDuringRead.as_i64()],
        "changed file should record exactly one ChangedDuringRead (3) error row, got {kinds:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn verify_should_collapse_hardlinks() -> TestResult {
    let fx = Fixture::new()?;
    fx.put("orig.bin", b"same bytes here\n")?;
    fs::hard_link(fx.root.join("orig.bin"), fx.root.join("link.bin"))?;
    fx.put("copy.bin", b"same bytes here\n")?;

    scan_run(&fx.db, &fx.root, &byte_cfg(), &FsReader, false, None)?;

    let conn = open_ro(&fx.db)?;
    let observed = (state_list(&conn)?, error_count(&conn)?);
    assert_eq!(
        observed,
        (vec![4, 4, 4], 0),
        "hardlink pair plus copy should all be state 4 with no errors, got (states, errors) = {observed:?}"
    );
    Ok(())
}

#[test]
fn verify_off_should_promote_pending_to_state_5() -> TestResult {
    let fx = Fixture::new()?;
    fx.put("dup1.txt", b"hello world\n")?;
    fx.put("dup2.txt", b"hello world\n")?;
    fx.put("unique.txt", b"unique content here\n")?;
    // Run 1 leaves the duplicates in state 3 (full hashed, not yet verified).
    scan_run(
        &fx.db,
        &fx.root,
        &byte_cfg(),
        &FsReader,
        false,
        Some(Phase::FullDone.as_i64()),
    )?;

    // Run 2 resumes with verification off, which must promote the pending rows.
    scan_run(&fx.db, &fx.root, &Config::default(), &FsReader, true, None)?;

    let conn = open_ro(&fx.db)?;
    let rows = states_by_name(&conn)?;
    let stuck = rows
        .iter()
        .filter(|(_, s)| *s == State::FullHashed.as_i64())
        .count();
    let dup_states: Vec<i64> = rows
        .iter()
        .filter(|(n, _)| n.starts_with("dup"))
        .map(|(_, s)| *s)
        .collect();
    let observed = (stuck, dup_states);
    assert_eq!(
        observed,
        (0, vec![5, 5]),
        "no row should stay in state 3 and duplicates should be state 5, got (stuck, dup states) = {observed:?}"
    );
    Ok(())
}

#[test]
fn verify_should_not_mismatch_on_short_reads() -> TestResult {
    let fx = Fixture::new()?;
    let body: Vec<u8> = (0..100u8).collect();
    fx.put("dup1.bin", &body)?;
    fx.put("dup2.bin", &body)?;

    let reader = ShortReader { inner: FsReader };
    scan_run(&fx.db, &fx.root, &byte_cfg(), &reader, false, None)?;

    let conn = open_ro(&fx.db)?;
    let observed = (state_list(&conn)?, error_count(&conn)?);
    assert_eq!(
        observed,
        (vec![4, 4], 0),
        "identical files read in {SHORT_READ_MAX}-byte pieces should be state 4 with no errors, got (states, errors) = {observed:?}"
    );
    Ok(())
}
