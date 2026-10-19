//! Normalize native `Metadata` into `core`'s `FileMeta` (no `cfg` escapes to core).

use std::fs::{FileType, Metadata};
use std::path::Path;

use dedupe_core::model::attrs;
use dedupe_core::time::system_time_to_unix_ns;
use dedupe_core::traits::{EntryKind, FileMeta};

#[cfg(windows)]
use dedupe_core::time::filetime_to_unix_ns;

/// Metadata for a regular file, read from one Win32 handle on Windows.
///
/// Windows: when `ft` is a regular file and the handle open succeeds, identity and info come
/// from that one open (`file_meta_from_info`). Otherwise the cached `md` is used, with
/// `file_id` `None` and `nlink` 1. Unix: `md` is the whole answer.
#[cfg(windows)]
pub fn file_meta_at(path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
    if ft.is_file() {
        if let Some(info) = crate::win_sys::file_info(path) {
            return file_meta_from_info(info);
        }
    }
    file_meta_from(md, ft)
}

#[cfg(not(windows))]
pub fn file_meta_at(_path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
    file_meta_from(md, ft)
}

/// Builds a regular-file `FileMeta` from one Win32 info read. Attributes map through the same
/// `win_attr_bits` as `file_meta_from`.
#[cfg(windows)]
pub fn file_meta_from_info(info: crate::win_sys::WinFileInfo) -> FileMeta {
    let mut attr_bits = win_attr_bits(info.attrs);
    if info.nlink > 1 {
        attr_bits |= attrs::HARDLINK;
    }
    FileMeta {
        file_id: Some(info.file_id),
        dev: Some(u64::from(info.volume_serial)),
        nlink: i64::from(info.nlink),
        size: i64::try_from(info.size).unwrap_or(i64::MAX),
        mtime_ns: filetime_to_unix_ns(info.written).unwrap_or(0),
        // ChangeTime is the metadata-change time (matches Unix ctime). Creation time is only a
        // fallback when the query failed.
        ctime_ns: info
            .change_time
            .and_then(filetime_to_unix_ns)
            .or_else(|| filetime_to_unix_ns(info.created)),
        attrs: attr_bits,
        kind: EntryKind::RegularFile,
    }
}

pub fn file_meta_from(md: &Metadata, ft: FileType) -> FileMeta {
    let kind = if ft.is_dir() {
        EntryKind::Directory
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_file() {
        EntryKind::RegularFile
    } else {
        EntryKind::Other
    };

    let (file_id, dev, nlink, hardlink_bits) = identity_bits(md);
    let mut attr_bits = hardlink_bits | platform_attr_bits(md);
    if matches!(kind, EntryKind::Symlink) {
        attr_bits |= attrs::SYMLINK;
    }

    FileMeta {
        file_id,
        dev,
        nlink,
        size: md.len() as i64,
        mtime_ns: modified_ns(md),
        ctime_ns: ctime_ns(md),
        attrs: attr_bits,
        kind,
    }
}

/// `(file_id, dev, nlink, HARDLINK bit)`. Unix reads the inode; elsewhere there is no
/// identity and the link count is 1.
#[cfg(unix)]
fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    let hard = if md.nlink() > 1 { attrs::HARDLINK } else { 0 };
    (
        Some(md.ino().to_le_bytes().to_vec()),
        Some(md.dev()),
        md.nlink() as i64,
        hard,
    )
}

#[cfg(not(unix))]
fn identity_bits(_md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
    (None, None, 1, 0)
}

/// Platform attribute bits that `Metadata` exposes beyond identity.
#[cfg(target_os = "macos")]
fn platform_attr_bits(md: &Metadata) -> i64 {
    use std::os::macos::fs::MetadataExt;
    // SF_DATALESS: iCloud/APFS dataless file; reading triggers a download.
    const SF_DATALESS: i64 = 0x4000_0000;
    if md.st_flags() as i64 & SF_DATALESS != 0 {
        attrs::PLACEHOLDER
    } else {
        0
    }
}

#[cfg(windows)]
fn platform_attr_bits(md: &Metadata) -> i64 {
    use std::os::windows::fs::MetadataExt;
    win_attr_bits(md.file_attributes())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn platform_attr_bits(_md: &Metadata) -> i64 {
    0
}

/// Windows attribute bits to `attrs` flags. Shared by the std path and the handle path.
#[cfg(windows)]
fn win_attr_bits(fa: u32) -> i64 {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    let mut bits: i64 = 0;
    if fa & FILE_ATTRIBUTE_HIDDEN != 0 {
        bits |= attrs::HIDDEN;
    }
    let placeholder = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    if fa & placeholder != 0 {
        bits |= attrs::PLACEHOLDER;
    }
    bits
}

/// Signed Unix ns (pre-1970 is negative); an out-of-range or unavailable time is `0`, the
/// convention for a missing mtime.
fn modified_ns(md: &Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(system_time_to_unix_ns)
        .unwrap_or(0)
}

#[cfg(unix)]
fn ctime_ns(md: &Metadata) -> Option<i64> {
    use dedupe_core::time::unix_parts_to_ns;
    use std::os::unix::fs::MetadataExt;
    unix_parts_to_ns(md.ctime(), md.ctime_nsec())
}

/// Windows `file_meta_from` has no change time in `Metadata`, so it uses creation time.
/// The handle path (`file_meta_from_info`) reports the real ChangeTime.
#[cfg(not(unix))]
fn ctime_ns(md: &Metadata) -> Option<i64> {
    md.created().ok().and_then(system_time_to_unix_ns)
}

#[cfg(test)]
mod tests {
    use super::{file_meta_from, system_time_to_unix_ns};
    use dedupe_core::model::attrs;
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
            let path = std::env::temp_dir()
                .join(format!("dedupe-meta-{name}-{}-{nanos}", std::process::id()));
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
}
