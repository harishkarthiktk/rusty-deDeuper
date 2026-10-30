use crate::meta::file_meta_from;
use dedupe_core::model::attrs;
use dedupe_core::time::system_time_to_unix_ns;
use dedupe_core::traits::EntryKind;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Temp directory removed on drop. Metadata tests need real files, since `Metadata`
/// cannot be built by hand.
struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(name: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after the epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("dedupe-meta-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&path).expect("create temp dir");
        TempDirGuard(path)
    }

    fn join(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write fixture file");
}

#[test]
fn file_meta_from_should_map_regular_file_kind_and_size_when_file_written() {
    let dir = TempDirGuard::new("regular");
    let path = dir.join("a.txt");
    write_file(&path, b"12345");
    let md = fs::metadata(&path).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.kind, EntryKind::RegularFile);
    assert_eq!(meta.size, 5);
    assert!(!attrs::has(meta.attrs, attrs::SYMLINK));
    assert!(!attrs::has(meta.attrs, attrs::HARDLINK));
}

#[test]
fn file_meta_from_should_map_directory_kind_when_path_is_dir() {
    let dir = TempDirGuard::new("dir");
    let sub = dir.join("sub");
    fs::create_dir(&sub).unwrap();
    let md = fs::metadata(&sub).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.kind, EntryKind::Directory);
    assert!(!attrs::has(meta.attrs, attrs::SYMLINK));
}

#[cfg(unix)]
#[test]
fn file_meta_from_should_map_symlink_kind_and_set_symlink_bit_when_lstat_used() {
    let dir = TempDirGuard::new("symlink");
    let target = dir.join("target.txt");
    let link = dir.join("link");
    write_file(&target, b"t");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let md = fs::symlink_metadata(&link).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.kind, EntryKind::Symlink);
    assert!(attrs::has(meta.attrs, attrs::SYMLINK));
}

#[cfg(unix)]
#[test]
fn file_meta_from_should_use_inode_and_dev_as_identity_when_unix() {
    use std::os::unix::fs::MetadataExt;
    let dir = TempDirGuard::new("identity");
    let path = dir.join("a.txt");
    write_file(&path, b"id");
    let md = fs::metadata(&path).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.file_id, Some(md.ino().to_le_bytes().to_vec()));
    assert_eq!(meta.dev, Some(md.dev()));
    assert_eq!(meta.nlink, md.nlink() as i64);
}

#[cfg(unix)]
#[test]
fn file_meta_from_should_set_hardlink_bit_when_nlink_above_one() {
    let dir = TempDirGuard::new("hardlink");
    let path = dir.join("a.txt");
    write_file(&path, b"h");
    fs::hard_link(&path, dir.join("b.txt")).unwrap();
    let md = fs::metadata(&path).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.nlink, 2);
    assert!(attrs::has(meta.attrs, attrs::HARDLINK));
}

#[test]
fn file_meta_from_should_store_mtime_as_unix_ns_when_time_is_post_epoch() {
    let dir = TempDirGuard::new("mtime");
    let path = dir.join("a.txt");
    write_file(&path, b"m");
    // Whole seconds, so the expected value is exact on any timestamp granularity.
    let when = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(when)
        .unwrap();
    let md = fs::metadata(&path).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.mtime_ns, 1_700_000_000_000_000_000);
}

#[test]
fn file_meta_from_should_store_pre_1970_mtime_as_negative_ns_when_time_before_epoch() {
    let dir = TempDirGuard::new("pre-epoch");
    let path = dir.join("a.txt");
    write_file(&path, b"p");
    let when = UNIX_EPOCH - Duration::from_secs(1_000);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(when)
        .unwrap();
    let md = fs::metadata(&path).unwrap();
    let expected = system_time_to_unix_ns(when);

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(expected, Some(-1_000_000_000_000));
    assert_eq!(meta.mtime_ns, -1_000_000_000_000);
}

#[test]
fn file_meta_from_should_store_zero_mtime_when_time_overflows_i64_ns() {
    let dir = TempDirGuard::new("overflow");
    let path = dir.join("a.txt");
    write_file(&path, b"o");
    // About year 2401: seconds fit in i64, but nanoseconds do not.
    let when = UNIX_EPOCH + Duration::from_secs(13_600_000_000);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(when)
        .unwrap();
    let md = fs::metadata(&path).unwrap();
    assert_eq!(
        system_time_to_unix_ns(when),
        None,
        "precondition: overflows i64 ns"
    );

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.mtime_ns, 0);
}

#[cfg(unix)]
#[test]
fn file_meta_from_should_store_ctime_from_unix_ctime_when_unix() {
    use dedupe_core::time::unix_parts_to_ns;
    use std::os::unix::fs::MetadataExt;
    let dir = TempDirGuard::new("ctime");
    let path = dir.join("a.txt");
    write_file(&path, b"c");
    let md = fs::metadata(&path).unwrap();

    let meta = file_meta_from(&md, md.file_type());

    assert_eq!(meta.ctime_ns, unix_parts_to_ns(md.ctime(), md.ctime_nsec()));
    assert!(meta.ctime_ns.is_some());
}
