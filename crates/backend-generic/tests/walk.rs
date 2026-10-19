//! Walker contract tests (GEN-21). Unix only: they rely on symlinks, hard links and chmod.
//! Each test builds its own tree under `std::env::temp_dir()` and removes it on drop.
#![cfg(unix)]

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use dedupe_backend_generic::WalkdirEnumerator;
use dedupe_core::config::Config;
use dedupe_core::exclude::ExcludeSet;
use dedupe_core::model::{attrs, ErrorKind};
use dedupe_core::traits::{EntryKind, Enumerator, FileMeta, WalkItem};

/// Temp directory that is removed on drop, even when a test panics.
struct TempTree(PathBuf);

impl TempTree {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("dedupe-walk-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp tree");
        TempTree(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        restore_access(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Makes every directory readable and searchable again so cleanup can remove it.
fn restore_access(dir: &Path) {
    let Ok(meta) = fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.is_dir() {
        return;
    }
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            restore_access(&entry.path());
        }
    }
}

/// True when chmod 000 does not stop reads (running as root, or CAP_DAC_OVERRIDE).
/// Such a test cannot observe an unreadable subtree and is skipped.
fn permissions_are_bypassed(dir: &Path) -> bool {
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    let readable = fs::read_dir(dir).is_ok();
    let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
    readable
}

fn walk_all(root: &Path, cfg: &Config) -> Vec<WalkItem> {
    let mut out = Vec::new();
    WalkdirEnumerator
        .walk(root, cfg, &mut |item| {
            out.push(item);
            Ok(())
        })
        .expect("walk only fails when the visitor fails");
    out
}

fn cfg_with_exclude(patterns: &[&str]) -> Config {
    let owned: Vec<String> = patterns.iter().map(|s| s.to_string()).collect();
    Config {
        exclude: ExcludeSet::new(&owned).expect("valid exclude glob"),
        ..Config::default()
    }
}

fn raw(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

fn item_bytes(item: &WalkItem) -> &[u8] {
    match item {
        WalkItem::Dir { path } | WalkItem::File { path, .. } | WalkItem::Error { path, .. } => {
            path.as_bytes()
        }
    }
}

fn position_of(items: &[WalkItem], path: &Path) -> Option<usize> {
    let want = raw(path);
    items.iter().position(|i| item_bytes(i) == want.as_slice())
}

fn meta_of<'a>(items: &'a [WalkItem], path: &Path) -> &'a FileMeta {
    let want = raw(path);
    items
        .iter()
        .find_map(|i| match i {
            WalkItem::File { path: p, meta } if p.as_bytes() == want.as_slice() => Some(meta),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no File item for {}", path.display()))
}

#[test]
fn walker_should_prune_excluded_dir_below_root_when_pattern_matches() {
    let tree = TempTree::new("prune");
    let root = tree.path();
    fs::create_dir_all(root.join("keep")).unwrap();
    fs::create_dir_all(root.join("sub/node_modules/pkg")).unwrap();
    fs::write(root.join("keep/a.txt"), b"a").unwrap();
    fs::write(root.join("sub/node_modules/pkg/b.txt"), b"b").unwrap();

    let items = walk_all(root, &cfg_with_exclude(&["**/node_modules"]));

    assert!(
        position_of(&items, &root.join("keep/a.txt")).is_some(),
        "non-excluded file should be yielded"
    );
    assert!(
        position_of(&items, &root.join("sub/node_modules")).is_none(),
        "excluded directory should not be yielded"
    );
    assert!(
        items
            .iter()
            .all(|i| !String::from_utf8_lossy(item_bytes(i)).contains("node_modules")),
        "nothing under an excluded directory should be yielded"
    );
}

#[test]
fn walker_should_not_prune_root_when_root_path_matches_pattern() {
    let tree = TempTree::new("root-excluded");
    let root = tree.path().join("node_modules");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();

    let items = walk_all(&root, &cfg_with_exclude(&["**/node_modules"]));

    assert!(
        position_of(&items, &root.join("a.txt")).is_some(),
        "the root (depth 0) is never excluded, so its children are yielded"
    );
}

#[test]
fn walker_should_yield_dir_before_its_children_when_walking_tree() {
    let tree = TempTree::new("order");
    let root = tree.path();
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::write(root.join("a/b/c.txt"), b"c").unwrap();

    let items = walk_all(root, &Config::default());
    let dir_a = position_of(&items, &root.join("a")).expect("dir a yielded");
    let dir_b = position_of(&items, &root.join("a/b")).expect("dir a/b yielded");
    let file_c = position_of(&items, &root.join("a/b/c.txt")).expect("file yielded");

    assert!(matches!(items[dir_a], WalkItem::Dir { .. }));
    assert!(dir_a < dir_b, "parent dir must precede child dir");
    assert!(dir_b < file_c, "dir must precede its file");
}

#[test]
fn walker_should_not_descend_into_symlinked_dir_when_link_points_to_dir() {
    let tree = TempTree::new("symlink-dir");
    let root = tree.path();
    fs::create_dir(root.join("real")).unwrap();
    fs::write(root.join("real/inner.txt"), b"x").unwrap();
    symlink(root.join("real"), root.join("link")).unwrap();

    let items = walk_all(root, &Config::default());

    assert!(
        position_of(&items, &root.join("link/inner.txt")).is_none(),
        "symlinked directory must not be followed"
    );
    let link_pos = position_of(&items, &root.join("link")).expect("link entry yielded");
    assert!(
        !matches!(items[link_pos], WalkItem::Dir { .. }),
        "symlink must not be reported as a directory"
    );
    let link = meta_of(&items, &root.join("link"));
    assert_eq!(link.kind, EntryKind::Symlink);
    assert!(attrs::has(link.attrs, attrs::SYMLINK));
}

#[test]
fn walker_should_report_nlink_and_hardlink_bit_when_file_has_two_names() {
    let tree = TempTree::new("hardlink");
    let root = tree.path();
    fs::write(root.join("a.txt"), b"same").unwrap();
    fs::hard_link(root.join("a.txt"), root.join("b.txt")).unwrap();

    let items = walk_all(root, &Config::default());
    let a = meta_of(&items, &root.join("a.txt"));
    let b = meta_of(&items, &root.join("b.txt"));

    assert_eq!(a.kind, EntryKind::RegularFile);
    assert_eq!(a.nlink, 2);
    assert!(attrs::has(a.attrs, attrs::HARDLINK));
    assert_eq!(a.file_id, b.file_id, "both names share one inode");
    assert!(a.file_id.is_some(), "unix always reports an inode");
}

#[test]
fn walker_should_clear_hardlink_bit_when_file_has_one_name() {
    let tree = TempTree::new("single-link");
    let root = tree.path();
    fs::write(root.join("a.txt"), b"solo").unwrap();

    let items = walk_all(root, &Config::default());
    let a = meta_of(&items, &root.join("a.txt"));

    assert_eq!(a.nlink, 1);
    assert!(!attrs::has(a.attrs, attrs::HARDLINK));
}

#[test]
fn walker_should_emit_error_item_when_root_does_not_exist() {
    let tree = TempTree::new("missing-root");
    let missing = tree.path().join("does-not-exist");

    let items = walk_all(&missing, &Config::default());

    assert_eq!(items.len(), 1, "a missing root yields exactly one item");
    match &items[0] {
        WalkItem::Error {
            path, is_dir, kind, ..
        } => {
            assert_eq!(path.as_bytes(), raw(&missing).as_slice());
            assert!(*is_dir, "unknown type is treated as a directory");
            assert_eq!(*kind, ErrorKind::Io);
        }
        other => panic!("expected WalkItem::Error, got {other:?}"),
    }
}

#[test]
fn walker_should_emit_error_item_and_skip_children_when_subdir_is_unreadable() {
    let tree = TempTree::new("unreadable");
    let root = tree.path();
    let locked = root.join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("secret.txt"), b"hidden").unwrap();
    fs::write(root.join("open.txt"), b"visible").unwrap();

    if permissions_are_bypassed(&locked) {
        eprintln!("skipping: chmod 000 does not restrict this process (running as root)");
        return;
    }
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

    let items = walk_all(root, &Config::default());

    assert!(
        position_of(&items, &locked.join("secret.txt")).is_none(),
        "children of an unreadable directory must not be yielded"
    );
    assert!(
        position_of(&items, &root.join("open.txt")).is_some(),
        "a sibling of the unreadable directory must still be yielded"
    );
    let error = items
        .iter()
        .find_map(|i| match i {
            WalkItem::Error {
                path, is_dir, kind, ..
            } if path.as_bytes() == raw(&locked).as_slice() => Some((*is_dir, *kind)),
            _ => None,
        })
        .expect("an Error item for the unreadable directory");
    assert!(error.0, "failed read_dir must be reported as a directory");
    assert_eq!(error.1, ErrorKind::Permission);
}
