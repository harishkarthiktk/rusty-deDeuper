//! Self-check: the NTFS `$MFT` path and the standard directory walker must describe the same tree.
//!
//! Run from an elevated PowerShell, with the scratch directory on an NTFS volume:
//!
//! ```text
//! cargo test -p dedupe-backend-windows --test mft_vs_walk -- --ignored
//! ```
//!
//! Known risk: reading the raw `$MFT` can report a size or mtime that differs from the walker for
//! files written moments ago, because the two read different on-disk copies of the metadata. The
//! test syncs every file and waits briefly, but a mismatch in size or mtime on very recently
//! written files is a known risk to investigate, not proof of a bug.

#![cfg(windows)]

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dedupe_backend_generic::WalkdirEnumerator;
use dedupe_backend_windows::{AutoEnumerator, EnumNotices};
use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::model::attrs;
use dedupe_core::traits::{EntryKind, Enumerator, WalkItem};

/// A unique directory under `temp_dir`, removed on drop so a failing assertion still cleans up.
struct ScratchTree {
    root: PathBuf,
}

impl ScratchTree {
    fn create() -> std::io::Result<Self> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let root =
            std::env::temp_dir().join(format!("dedupe-mft-vs-walk-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
}

impl Drop for ScratchTree {
    fn drop(&mut self) {
        // Best effort: a leftover temp directory must not turn a test failure into a panic
        // during unwinding.
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Deterministic, non-zero content, so a size or content mismatch is visible.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

/// Writes `bytes` and syncs the file, so the metadata is on disk before either enumerator reads it.
fn write_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn build_scratch_tree(root: &Path) -> std::io::Result<()> {
    let nested = root.join("nested").join("deeper");
    let spaced = root.join("spaces in name");
    let unicode_dir = root.join("unicode_ß_名前");
    fs::create_dir_all(&nested)?;
    fs::create_dir_all(&spaced)?;
    fs::create_dir_all(&unicode_dir)?;

    write_file(&root.join("empty.bin"), &[])?;
    write_file(&root.join("one.bin"), &[7])?;
    write_file(&root.join("four_k.bin"), &pattern(4096))?;
    write_file(&nested.join("hundred_k.bin"), &pattern(100_000))?;
    write_file(&spaced.join("two mib.bin"), &pattern(2 * 1024 * 1024))?;
    write_file(&unicode_dir.join("café.txt"), b"unicode name")?;

    // A second name for the same file record. Both enumerators must yield both names.
    fs::hard_link(root.join("four_k.bin"), root.join("four_k_link.bin"))?;
    // Creating a symlink needs privilege that not every shell has. Its absence is not a
    // failure of the comparison, so the result is ignored on purpose.
    let _ = std::os::windows::fs::symlink_file(root.join("one.bin"), root.join("one_link.bin"));
    Ok(())
}

/// One enumerated item, reduced to the fields both enumerators must agree on.
///
/// Excluded on purpose: `file_id` and `nlink`. Only the `$MFT` path provides them, and the
/// walker reports `None` and 1, so comparing them would fail by design. The `HARDLINK` bit is
/// excluded for the same reason: the `$MFT` path derives it from the link count.
/// Directories carry only path and kind, because the walker reports directories by path alone.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    path: Vec<u8>,
    kind: EntryKind,
    size: i64,
    mtime_ns: i64,
    ctime_ns: Option<i64>,
    attrs: i64,
}

impl Row {
    fn from_item(item: WalkItem) -> Self {
        match item {
            WalkItem::Dir { path } => Self {
                path: path.as_bytes().to_vec(),
                kind: EntryKind::Directory,
                size: 0,
                mtime_ns: 0,
                ctime_ns: None,
                attrs: 0,
            },
            WalkItem::File { path, meta } => Self {
                path: path.as_bytes().to_vec(),
                kind: meta.kind,
                size: meta.size,
                mtime_ns: meta.mtime_ns,
                ctime_ns: meta.ctime_ns,
                attrs: meta.attrs & !attrs::HARDLINK,
            },
        }
    }
}

/// Every item under `root`, sorted by path bytes so two runs compare directly.
fn collect(enumerator: &dyn Enumerator, root: &Path) -> Vec<Row> {
    let mut rows = Vec::new();
    enumerator
        .walk(root, &Config::default(), &mut |item: WalkItem| {
            rows.push(Row::from_item(item));
            Ok(())
        })
        .expect("enumerating the scratch tree should succeed");
    rows.sort_by(|a, b| a.path.cmp(&b.path));
    rows
}

/// Rows in `left` with no equal row in `right`, described for the failure message.
fn difference(left: &[Row], right: &[Row]) -> Vec<String> {
    left.iter()
        .filter(|row| !right.contains(row))
        .map(describe)
        .collect()
}

fn describe(row: &Row) -> String {
    format!(
        "{} kind={:?} size={} mtime_ns={} ctime_ns={:?} attrs={:#x}",
        String::from_utf8_lossy(&row.path),
        row.kind,
        row.size,
        row.mtime_ns,
        row.ctime_ns,
        row.attrs
    )
}

#[test]
#[ignore = "needs an elevated shell on an NTFS volume"]
fn mft_enumeration_should_match_directory_walk_for_scratch_tree() {
    let scratch = ScratchTree::create()
        .expect("creating the scratch directory under temp_dir should succeed");
    build_scratch_tree(&scratch.root).expect("building the scratch tree should succeed");
    // Lets cached directory metadata settle, so both enumerators read the same values.
    std::thread::sleep(Duration::from_millis(500));

    let notices = Arc::new(EnumNotices::default());
    let mft_rows = collect(
        &AutoEnumerator::new(false, Arc::clone(&notices), Token::new()),
        &scratch.root,
    );
    // Any queued notice means the enumerator fell back to the walker, so the comparison
    // below would prove nothing about the `$MFT` path.
    let queued = notices.drain();
    assert!(
        queued.is_empty(),
        "the MFT path should run without fallback, but the enumerator queued: {queued:?}"
    );

    let walk_rows = collect(&WalkdirEnumerator, &scratch.root);

    let only_mft = difference(&mft_rows, &walk_rows);
    let only_walk = difference(&walk_rows, &mft_rows);
    assert!(
        only_mft.is_empty() && only_walk.is_empty(),
        "MFT and walker disagree.\nonly in MFT:\n{}\nonly in walker:\n{}",
        only_mft.join("\n"),
        only_walk.join("\n")
    );
}
