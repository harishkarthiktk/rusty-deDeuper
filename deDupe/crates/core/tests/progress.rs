//! Progress-reporting tests: the sink receives balanced per-pass events.
//!
//! Uses in-test backends (real-FS walker + reader, fake volume identity), the
//! same shape as `phase1.rs`, copied down to what these tests need.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::error::Result;
use dedupe_core::hash::Blake3Hasher;
use dedupe_core::model::{attrs, FsType, Phase, UuidKind};
use dedupe_core::path::RawPath;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::Services;
use dedupe_core::progress::{
    emit_advance, emit_finished, emit_started, null_sink, PhaseProgress, SharedSink, Sink,
};
use dedupe_core::traits::*;

// ---------------------------------------------------------------------------
// Counting sink
// ---------------------------------------------------------------------------

#[derive(Default, Debug)]
struct PhaseStats {
    total: Option<u64>,
    advanced: u64,
    finished: bool,
}

#[derive(Default)]
struct Stats {
    phases: BTreeMap<(i64, i64, Phase), PhaseStats>,
    /// The phase currently running for each (run, volume). `advance` carries no phase.
    active: BTreeMap<(i64, i64), Phase>,
}

struct CountingSink {
    stats: Arc<Mutex<Stats>>,
}

impl Sink for CountingSink {
    fn phase_started(&mut self, p: &PhaseProgress) {
        let mut s = self.stats.lock().unwrap();
        s.active.insert((p.run_id, p.volume_id), p.phase);
        s.phases.insert(
            (p.run_id, p.volume_id, p.phase),
            PhaseStats {
                total: p.total,
                ..Default::default()
            },
        );
    }

    fn advance(&mut self, run_id: i64, volume_id: i64, n: u64) {
        let mut s = self.stats.lock().unwrap();
        let phase = s.active[&(run_id, volume_id)];
        s.phases
            .entry((run_id, volume_id, phase))
            .or_default()
            .advanced += n;
    }

    fn phase_finished(&mut self, p: &PhaseProgress) {
        let mut s = self.stats.lock().unwrap();
        s.phases
            .get_mut(&(p.run_id, p.volume_id, p.phase))
            .expect("phase_finished without phase_started")
            .finished = true;
    }
}

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
        _cfg: &Config,
        visit: &mut dyn FnMut(WalkItem) -> Result<()>,
    ) -> Result<()> {
        visit(WalkItem::Dir {
            path: Codec.path_to_raw(root),
        })?;
        recurse(root, visit)
    }
}

fn recurse(dir: &Path, visit: &mut dyn FnMut(WalkItem) -> Result<()>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        let p = entry?.path();
        let md = fs::symlink_metadata(&p)?;
        if md.is_dir() {
            visit(WalkItem::Dir {
                path: Codec.path_to_raw(&p),
            })?;
            recurse(&p, visit)?;
        } else {
            visit(WalkItem::File {
                path: Codec.path_to_raw(&p),
                meta: to_meta(&md),
            })?;
        }
    }
    Ok(())
}

/// Inode and link count. Windows has no stable inode here, so no hardlink collapse.
#[cfg(unix)]
fn inode_and_links(md: &fs::Metadata) -> (Option<Vec<u8>>, i64) {
    use std::os::unix::fs::MetadataExt;
    (Some(md.ino().to_le_bytes().to_vec()), md.nlink() as i64)
}

#[cfg(not(unix))]
fn inode_and_links(_md: &fs::Metadata) -> (Option<Vec<u8>>, i64) {
    (None, 1)
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
    let (file_id, nlink) = inode_and_links(md);
    FileMeta {
        file_id,
        nlink,
        size: md.len() as i64,
        mtime_ns: md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0),
        ctime_ns: None,
        attrs: if nlink > 1 { attrs::HARDLINK } else { 0 },
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

struct Reader;

impl ContentReader for Reader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let f = fs::File::open(path)?;
        Ok(pread(&f, buf, offset)?)
    }
    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        Ok(to_meta(&fs::symlink_metadata(path)?))
    }
}

struct FakeVol {
    uuid: String,
}

impl VolumeIdentity for FakeVol {
    fn identify(&self, path: &Path) -> Result<VolumeInfo> {
        Ok(VolumeInfo {
            uuid: self.uuid.clone(),
            uuid_kind: UuidKind::FsUuid,
            // Mount at the scan root so the root folder is the volume root.
            mount: Codec.path_to_raw(path),
            fs_type: FsType::Ext4,
            label: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn fixture_tree() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tree")
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn advance_sums_to_total_per_pass() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    copy_dir(&fixture_tree(), &root);
    let db = dir.path().join("d.db");

    let stats = Arc::new(Mutex::new(Stats::default()));
    let sink: SharedSink = Arc::new(Mutex::new(CountingSink {
        stats: Arc::clone(&stats),
    }));

    let walker = Walker;
    let codec = Codec;
    let reader = Reader;
    let vi = FakeVol {
        uuid: "vol-progress".to_string(),
    };
    let hasher = Blake3Hasher;
    let cancel = Token::new();
    let svc = Services {
        enumr: &walker,
        reader: &reader,
        codec: &codec,
        volume: &vi,
        hasher: &hasher,
        cancel: &cancel,
        sink: &sink,
    };
    let opts = ScanOptions {
        cli_args: "test".to_string(),
        resume: false,
        stop_after: None,
    };
    run_scan(&db, &[root], &Config::default(), &svc, &opts).unwrap();

    let s = stats.lock().unwrap();

    let enumerated = s
        .phases
        .iter()
        .find(|((_, _, p), _)| *p == Phase::Enumerated)
        .map(|(_, ps)| ps)
        .expect("Enumerated phase not reported");
    assert_eq!(
        enumerated.total, None,
        "Enumerated must start with unknown total"
    );

    let mut checked = 0;
    for ((_, _, phase), ps) in &s.phases {
        if let Some(n) = ps.total {
            assert_eq!(ps.advanced, n, "{phase:?}: advance sum != total");
            assert!(ps.finished, "{phase:?}: phase_finished not called");
            checked += 1;
        }
    }
    // Verified only runs with `cfg.verify == Byte`, which the default config leaves off.
    assert!(
        checked >= 2,
        "expected PrefixDone and FullDone to report totals, got {checked}"
    );
}

#[test]
fn null_sink_is_inert() {
    let sink = null_sink();
    let p = PhaseProgress {
        run_id: 1,
        volume_id: 1,
        phase: Phase::FullDone,
        done: 0,
        total: Some(2),
    };
    emit_started(&sink, &p);
    emit_advance(&sink, 1, 1, 2);
    emit_finished(&sink, &PhaseProgress { done: 2, ..p });
}
