//! Phase 1 integration tests: resume, lookup, rescan, move detection and guards.
//!
//! These use in-test backends (a real-FS walker + reader and a fake volume
//! identity) so `dedupe-core` stays free of `cfg`/`unsafe`.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::db::{queries, Db};
use dedupe_core::error::{CoreError, Result};
use dedupe_core::hash::Blake3Hasher;
#[cfg(unix)]
use dedupe_core::model::attrs;
use dedupe_core::model::{FsType, State, UuidKind};
use dedupe_core::path::RawPath;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::Services;
use dedupe_core::report::Report;
use dedupe_core::traits::*;

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

/// Reports a larger size after a read, to exercise the change-during-read guard.
struct TamperReader {
    inner: FsReader,
}

impl ContentReader for TamperReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.inner.read_at(path, offset, buf)
    }
    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        let mut m = self.inner.metadata(path)?;
        m.size += 1;
        Ok(m)
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
            // Mount the fake volume at the canonicalized scan root, so the volume
            // root folder == scan root and files/folders are direct children.
            mount: Codec.path_to_raw(path),
            fs_type: FsType::Ext4,
            label: None,
        })
    }
}

/// Like [`FakeVol`] but with a fixed mount, so sibling scan roots share one volume.
struct MountVol {
    uuid: String,
    mount: PathBuf,
}

impl VolumeIdentity for MountVol {
    fn identify(&self, _path: &Path) -> Result<VolumeInfo> {
        Ok(VolumeInfo {
            uuid: self.uuid.clone(),
            uuid_kind: UuidKind::FsUuid,
            mount: Codec.path_to_raw(&self.mount),
            fs_type: FsType::Ext4,
            label: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

struct Env {
    _dir: tempfile::TempDir,
    db: PathBuf,
    root: PathBuf,
}

fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(&root).unwrap();
    let db = dir.path().join("d.db");
    Env {
        _dir: dir,
        db,
        root,
    }
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, bytes).unwrap();
}

/// A tree with small dups, a large dup pair, uniques and a zero-length file.
fn populate(root: &Path) {
    write(root, "a.txt", b"hello world\n");
    write(root, "sub/a_copy.txt", b"hello world\n");
    write(root, "nested/deep/dup.txt", b"hello world\n");
    write(root, "b.txt", b"unique content\n");
    write(root, "only.txt", b"only here\n");
    write(root, "empty.txt", b"");
    write(root, "f1.bin", &[b'A'; 100]);
    write(root, "f2.bin", &[b'A'; 100]);
    let big: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    write(root, "big1.bin", &big);
    write(root, "big2.bin", &big);
    let mut other = big.clone();
    other[0] = other[0].wrapping_add(1);
    write(root, "big_unique.bin", &other);
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
fn resume_at_every_boundary_converges() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let reader = FsReader::new();

    let full_db = e._dir.path().join("full.db");
    scan(&full_db, &e.root, &cfg, &vi, &reader, false, None).unwrap();
    let full = report_json(&full_db);

    for n in 0..=6i64 {
        let db = e._dir.path().join(format!("r{n}.db"));
        scan(&db, &e.root, &cfg, &vi, &reader, false, Some(n)).unwrap();
        scan(&db, &e.root, &cfg, &vi, &reader, true, None).unwrap();
        assert_eq!(report_json(&db), full, "resume at pass {n} diverged");
    }
}

#[test]
fn lookup_basic_and_mutation() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let reader = FsReader::new();
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();

    let codec = Codec;
    let conn = Db::open_readonly(&e.db).unwrap().conn;

    // Copy of a scanned file matches.
    let copy = e._dir.path().join("copy_a.txt");
    fs::copy(e.root.join("a.txt"), &copy).unwrap();
    let out = dedupe_core::lookup::lookup(&conn, &reader, &codec, &vi, &copy, &cfg, false).unwrap();
    assert!(matches!(out, dedupe_core::lookup::LookupOutcome::Matches(m) if m.len() == 3));

    // One byte different: no match.
    write(e._dir.path(), "mut.txt", b"hello worlx\n");
    let mut_path = e._dir.path().join("mut.txt");
    let out =
        dedupe_core::lookup::lookup(&conn, &reader, &codec, &vi, &mut_path, &cfg, false).unwrap();
    assert!(matches!(out, dedupe_core::lookup::LookupOutcome::NoMatch));

    // Zero-length is skipped.
    write(e._dir.path(), "zero.txt", b"");
    let zero = e._dir.path().join("zero.txt");
    let out = dedupe_core::lookup::lookup(&conn, &reader, &codec, &vi, &zero, &cfg, false).unwrap();
    assert!(matches!(
        out,
        dedupe_core::lookup::LookupOutcome::Skipped(_)
    ));
}

#[test]
fn lookup_singleton_regression() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let reader = FsReader::new();
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();

    // b.txt had a unique size at scan time (state -3); its copy must still match.
    let copy = e._dir.path().join("copy_b.txt");
    fs::copy(e.root.join("b.txt"), &copy).unwrap();
    let codec = Codec;
    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let out = dedupe_core::lookup::lookup(&conn, &reader, &codec, &vi, &copy, &cfg, false).unwrap();
    assert!(matches!(out, dedupe_core::lookup::LookupOutcome::Matches(m) if m.len() == 1));
}

#[test]
fn lookup_freshness_missing() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let reader = FsReader::new();
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();

    let copy = e._dir.path().join("copy_a.txt");
    fs::copy(e.root.join("a.txt"), &copy).unwrap();
    fs::remove_file(e.root.join("a.txt")).unwrap();

    let codec = Codec;
    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let out = dedupe_core::lookup::lookup(&conn, &reader, &codec, &vi, &copy, &cfg, false).unwrap();
    let dedupe_core::lookup::LookupOutcome::Matches(rows) = out else {
        panic!("expected matches")
    };
    assert!(rows
        .iter()
        .any(|m| m.freshness == dedupe_core::lookup::Freshness::Missing));
}

#[test]
fn rescan_reuses_hashes_and_removes_deleted() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");

    {
        let reader = FsReader::new();
        scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();
    }

    // Unchanged rescan performs zero content reads.
    let reader = FsReader::new();
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();
    assert_eq!(
        reader.opens(),
        0,
        "unchanged rescan must not read file content"
    );

    // Modified file is re-hashed. Keep the same length so a.txt still has a size
    // partner; a unique-size change is correctly skipped by the size pass.
    write(&e.root, "a.txt", b"HELLO WORLD\n");
    let reader = FsReader::new();
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();
    assert!(reader.opens() > 0, "modified file must be read again");

    // Deleted file's row is removed.
    fs::remove_file(e.root.join("f2.bin")).unwrap();
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();
    let conn = Db::open_readonly(&e.db).unwrap().conn;
    // Volume root == scan root (FakeVol mounts at the scan root), so f2.bin is a
    // direct child of the root folder.
    let root_id = queries::find_folder(&conn, 1, 0, &RawPath::empty())
        .unwrap()
        .unwrap();
    let gone = queries::find_by_path(&conn, 1, root_id, &RawPath::new(b"f2.bin".to_vec())).unwrap();
    assert!(gone.is_none(), "deleted file row should be removed");
}

// The fixture's `to_meta` supplies `file_id` (the inode) only on Unix, and move detection
// needs a file id.
#[cfg(unix)]
#[test]
fn move_detection_keeps_id_and_reads_nothing() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let old = queries::find_by_path(
        &conn,
        1,
        folder_of(&conn, "nested/deep"),
        &RawPath::new(b"dup.txt".to_vec()),
    )
    .unwrap()
    .expect("dup.txt indexed");
    drop(conn);

    // Rename the containing directory: same inode/size/mtime, new path.
    fs::rename(e.root.join("nested"), e.root.join("renamed")).unwrap();

    let reader = FsReader::new();
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();
    assert_eq!(reader.opens(), 0, "move should reuse hashes");

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let new = queries::find_by_path(
        &conn,
        1,
        folder_of(&conn, "renamed/deep"),
        &RawPath::new(b"dup.txt".to_vec()),
    )
    .unwrap()
    .expect("moved file indexed");
    assert_eq!(new.id, old.id, "move must preserve files.id");
    assert!(new.full_hash.is_some());
}

fn folder_of(conn: &rusqlite::Connection, rel: &str) -> i64 {
    let mut cur = queries::find_folder(conn, 1, 0, &RawPath::empty())
        .unwrap()
        .unwrap();
    for comp in rel.split('/') {
        cur = queries::find_folder(conn, 1, cur, &RawPath::new(comp.as_bytes().to_vec()))
            .unwrap()
            .unwrap_or_else(|| panic!("missing folder {rel}"));
    }
    cur
}

#[cfg(unix)]
#[test]
fn hardlink_only_pair_is_not_a_group() {
    let e = env();
    fs::write(e.root.join("orig"), b"data-here\n").unwrap();
    fs::hard_link(e.root.join("orig"), e.root.join("link")).unwrap();

    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let report = Report::from_db(&conn, 0).unwrap();
    assert_eq!(
        report.groups.len(),
        0,
        "hardlink-only pair is not a duplicate"
    );

    // Both rows are UNIQUE_SIZE with attrs.hardlink set.
    let rows = queries::all_rows_in_state(&conn, 1, State::UniqueSize).unwrap();
    assert!(rows.iter().any(|r| attrs::has(r.attrs, attrs::HARDLINK)));

    // An independent copy creates a real group with two distinct inodes.
    fs::write(e.root.join("copy"), b"data-here\n").unwrap();
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();
    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let report = Report::from_db(&conn, 0).unwrap();
    assert_eq!(report.groups.len(), 1);
    assert_eq!(report.groups[0].distinct_inodes, 2);
    // Two inodes hold one copy of the bytes, so exactly one file's size is wasted.
    let size = b"data-here\n".len() as i64;
    assert_eq!(report.groups[0].wasted_logical, size);
}

#[cfg(unix)]
#[test]
fn non_regular_files_are_skipped() {
    use std::os::unix::net::UnixListener;

    let e = env();
    write(&e.root, "a.txt", b"hello world\n");
    write(&e.root, "sub/a_copy.txt", b"hello world\n");
    // A FIFO and a socket must never be opened.
    let fifo = e.root.join("pipe");
    let status = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(status.map(|s| s.success()).unwrap_or(false), "mkfifo");
    let _listener = UnixListener::bind(e.root.join("sock")).unwrap();

    // Scan on a worker thread: if a FIFO is opened, the open blocks forever, so
    // the timeout turns that regression into a failure instead of a hang.
    let (db, root) = (e.db.clone(), e.root.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let cfg = Config::default();
        let vi = FakeVol::new("vol-A");
        let res = scan(&db, &root, &cfg, &vi, &FsReader::new(), false, None);
        let _ = tx.send(res);
    });
    let scanned = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("scan did not finish within 30 s; a FIFO or socket was likely opened");
    scanned.unwrap();

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let skipped = queries::all_rows_in_state(&conn, 1, State::Skipped).unwrap();
    assert!(skipped.len() >= 2, "fifo and socket should be skipped");
}

#[test]
fn change_during_read_records_error_and_stores_no_hash() {
    let e = env();
    write(&e.root, "a.txt", b"hello world\n");
    write(&e.root, "sub/a_copy.txt", b"hello world\n");
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    let reader = TamperReader {
        inner: FsReader::new(),
    };
    scan(&e.db, &e.root, &cfg, &vi, &reader, false, None).unwrap();

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let errs = queries::all_rows_in_state(&conn, 1, State::Error).unwrap();
    assert!(!errs.is_empty(), "expected a changed_during_read error");
    assert!(errs.iter().all(|r| r.full_hash.is_none()));
}

#[test]
fn volume_uuid_guard_refuses_then_force_proceeds() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();

    // Leave a running run.
    scan(
        &e.db,
        &e.root,
        &cfg,
        &FakeVol::new("vol-A"),
        &FsReader::new(),
        false,
        Some(1),
    )
    .unwrap();

    // Resume with a different UUID: refused.
    let err = scan(
        &e.db,
        &e.root,
        &cfg,
        &FakeVol::new("vol-B"),
        &FsReader::new(),
        true,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::VolumeMismatch { .. }));

    // --force proceeds.
    let mut forced = cfg.clone();
    forced.force = true;
    scan(
        &e.db,
        &e.root,
        &forced,
        &FakeVol::new("vol-B"),
        &FsReader::new(),
        true,
        None,
    )
    .unwrap();
}

#[test]
fn hash_config_mismatch_refuses_then_force_wipes() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    scan(
        &e.db,
        &e.root,
        &cfg,
        &FakeVol::new("vol-A"),
        &FsReader::new(),
        false,
        None,
    )
    .unwrap();

    let mut changed = cfg.clone();
    changed.prefix_first_bytes = 8 * 1024;
    let err = scan(
        &e.db,
        &e.root,
        &changed,
        &FakeVol::new("vol-A"),
        &FsReader::new(),
        false,
        None,
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::HashConfigMismatch(_)));

    let mut forced = changed.clone();
    forced.force = true;
    scan(
        &e.db,
        &e.root,
        &forced,
        &FakeVol::new("vol-A"),
        &FsReader::new(),
        false,
        None,
    )
    .unwrap();
}

#[test]
fn abandoned_run_then_resume_reports_nothing() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");

    // Interrupted run stays `running`.
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, Some(1)).unwrap();
    {
        let conn = Db::open_readonly(&e.db).unwrap().conn;
        assert!(queries::latest_running_run(&conn).unwrap().is_some());
    }

    // A fresh scan marks it abandoned and completes.
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();
    let conn = Db::open_readonly(&e.db).unwrap().conn;
    assert!(queries::latest_running_run(&conn).unwrap().is_none());

    let err = scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), true, None).unwrap_err();
    assert!(matches!(err, CoreError::NothingToResume));
}

#[test]
fn unicode_bytes_round_trip_through_db() {
    let e = env();
    // NFD "e" + combining acute, plus invalid UTF-8 bytes.
    let name: Vec<u8> = vec![b'e', 0xcc, 0x81, 0xff, 0x80];
    write(&e.root, "a.txt", b"hello world\n");
    write(&e.root, "sub/a_copy.txt", b"hello world\n");

    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();

    // upsert_file below writes, so this connection must be read-write.
    let conn = Db::open(&e.db, &cfg).unwrap().conn;
    let folder = folder_of(&conn, "sub");
    let entry = dedupe_core::model::FileEntry::enumerated(
        999_999,
        1,
        folder,
        RawPath::new(name.clone()),
        1,
    );
    queries::upsert_file(&conn, &entry).unwrap();
    let got = queries::find_by_path(&conn, 1, folder, &RawPath::new(name.clone()))
        .unwrap()
        .unwrap();
    assert_eq!(got.name.as_bytes(), name.as_slice());
}

#[test]
fn lookup_insert_persists_on_demand_hashes() {
    let e = env();
    populate(&e.root);
    let cfg = Config::default();
    let vi = FakeVol::new("vol-A");
    scan(&e.db, &e.root, &cfg, &vi, &FsReader::new(), false, None).unwrap();

    // b.txt ("unique content\n") has a unique size at scan time -> state -3, no hash.
    let copy = e._dir.path().join("copy_b.txt");
    fs::copy(e.root.join("b.txt"), &copy).unwrap();

    let codec = Codec;
    let reader = FsReader::new();
    let conn = Db::open(&e.db, &cfg).unwrap().conn; // read-write for --insert
    let out = dedupe_core::lookup::lookup(&conn, &reader, &codec, &vi, &copy, &cfg, true).unwrap();
    assert!(matches!(out, dedupe_core::lookup::LookupOutcome::Matches(m) if m.len() == 1));

    // FakeVol mounts at the scan root, so b.txt is a direct child of the root folder.
    let root_id = queries::find_folder(&conn, 1, 0, &RawPath::empty())
        .unwrap()
        .unwrap();
    let b = queries::find_by_path(&conn, 1, root_id, &RawPath::new(b"b.txt".to_vec()))
        .unwrap()
        .unwrap();
    assert!(b.full_hash.is_some());
    assert_eq!(b.state, State::Done);
}

#[test]
fn rescan_root_scoping_leaves_other_roots_untouched() {
    let e = env();
    let a = e.root.join("A");
    let b = e.root.join("B");
    fs::create_dir_all(&a).unwrap();
    fs::create_dir_all(&b).unwrap();
    fs::write(a.join("fa.txt"), b"same-content\n").unwrap();
    fs::write(b.join("fb.txt"), b"same-content\n").unwrap();

    let cfg = Config::default();
    let vi = MountVol {
        uuid: "vol-A".into(),
        mount: fs::canonicalize(&e.root).unwrap(),
    };
    let walker = Walker;
    let codec = Codec;
    let hasher = Blake3Hasher;
    let opts = ScanOptions {
        cli_args: "t".into(),
        resume: false,
        stop_after: None,
        no_lock: false,
    };

    let reader1 = FsReader::new();
    let cancel1 = Token::new();
    let sink1 = dedupe_core::progress::null_sink();
    let svc1 = Services {
        enumr: &walker,
        reader: &reader1,
        codec: &codec,
        volume: &vi,
        hasher: &hasher,
        cancel: &cancel1,
        sink: &sink1,
    };
    run_scan(&e.db, &[a.clone(), b.clone()], &cfg, &svc1, &opts).unwrap();

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let b_folder = folder_of(&conn, "B");
    let fid_b = queries::find_by_path(&conn, 1, b_folder, &RawPath::new(b"fb.txt".to_vec()))
        .unwrap()
        .unwrap()
        .id;
    drop(conn);

    // Rescan only root A.
    let reader2 = FsReader::new();
    let cancel2 = Token::new();
    let sink2 = dedupe_core::progress::null_sink();
    let svc2 = Services {
        enumr: &walker,
        reader: &reader2,
        codec: &codec,
        volume: &vi,
        hasher: &hasher,
        cancel: &cancel2,
        sink: &sink2,
    };
    run_scan(&e.db, std::slice::from_ref(&a), &cfg, &svc2, &opts).unwrap();

    let conn = Db::open_readonly(&e.db).unwrap().conn;
    let b_folder = folder_of(&conn, "B");
    let b_row = queries::find_by_path(&conn, 1, b_folder, &RawPath::new(b"fb.txt".to_vec()))
        .unwrap()
        .expect("root B rows must survive a root-A-only rescan");
    assert_eq!(b_row.id, fid_b);
}
