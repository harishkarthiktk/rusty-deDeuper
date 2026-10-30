//! First physical extent of a file via `FS_IOC_FIEMAP`.
//!
//! The offset orders HDD scans so the head sweeps the platter once instead of
//! seeking per file. It is a scheduling hint, never a correctness input.

#![expect(
    unsafe_code,
    reason = "FIEMAP ioctl; the unsafe block states its invariants"
)]

use std::os::fd::AsRawFd;
use std::path::Path;

/// `_IOWR('f', 11, struct fiemap)` from `<linux/fs.h>`. Defined locally because the
/// `libc` crate does not expose it on every target.
const FS_IOC_FIEMAP: u32 = 0xC020_660B;

/// `fe_flags` bits from `<linux/fiemap.h>`. An extent with any of these set has no real
/// physical location to order by.
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;
const FIEMAP_EXTENT_DATA_TAIL: u32 = 0x0000_0400;
const NO_LOCATION: u32 = FIEMAP_EXTENT_UNKNOWN
    | FIEMAP_EXTENT_DELALLOC
    | FIEMAP_EXTENT_DATA_INLINE
    | FIEMAP_EXTENT_DATA_TAIL;

/// Mirrors `struct fiemap` from `<linux/fiemap.h>`. The kernel's flexible array is
/// modelled as one trailing element, which matches the layout for `fm_extent_count = 1`.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Fiemap {
    fm_start: u64,
    fm_length: u64,
    fm_flags: u32,
    fm_mapped_extents: u32,
    fm_extent_count: u32,
    fm_reserved: u32,
    fm_extents: [FiemapExtent; 1],
}

/// Mirrors `struct fiemap_extent` from `<linux/fiemap.h>` (56 bytes on all targets).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FiemapExtent {
    fe_logical: u64,
    fe_physical: u64,
    fe_length: u64,
    fe_reserved64: [u64; 2],
    fe_flags: u32,
    fe_reserved: [u32; 3],
}

/// Physical byte offset of the first mapped extent of `path`.
///
/// Returns `Ok(None)` when the filesystem does not implement FIEMAP (ENOTTY,
/// EOPNOTSUPP, and similar), when the file has no mapped extents, or when the
/// ioctl fails for any other reason. The ioctl failure is folded into `None`
/// on purpose: ordering falls back to the file id, so a scan should not abort.
/// Opening the file is still an error, because the caller just enumerated it.
pub fn first_extent_offset(path: &Path) -> std::io::Result<Option<u64>> {
    let file = crate::reader::open_regular(path)?;
    let mut req = Fiemap {
        fm_length: u64::MAX,
        fm_extent_count: 1,
        ..Fiemap::default()
    };

    // SAFETY: `file` owns an open descriptor until the end of this function, so the
    // fd is valid for the call. `req` is a live, initialized `#[repr(C)]` value of
    // 88 bytes (32-byte header plus one 56-byte extent), aligned to 8. That matches
    // the `fm_extent_count = 1` we declared, so the kernel's reads and writes stay
    // inside `req`. The pointer is not retained after the call returns.
    let rc = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            FS_IOC_FIEMAP as libc::Ioctl,
            std::ptr::addr_of_mut!(req),
        )
    };

    if rc != 0 || req.fm_mapped_extents == 0 {
        return Ok(None);
    }
    Ok(physical_if_located(&req.fm_extents[0]))
}

/// Physical offset of `ext`, or `None` when its flags mark it as having no real location
/// (unwritten, delayed-allocation, inline or tail-packed data). Ordering by such an
/// address would be meaningless.
fn physical_if_located(ext: &FiemapExtent) -> Option<u64> {
    (ext.fe_flags & NO_LOCATION == 0).then_some(ext.fe_physical)
}

#[cfg(all(test, target_os = "linux"))]
mod fiemap_tests {
    use super::{first_extent_offset, physical_if_located, FiemapExtent};
    use std::io;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn extent_with_flags(fe_flags: u32) -> FiemapExtent {
        FiemapExtent {
            fe_physical: 8192,
            fe_flags,
            ..FiemapExtent::default()
        }
    }

    /// A temp file removed when the test ends, including on panic. The crate has no
    /// `tempfile` dev-dependency, so the name is unique by process id, test name and time.
    struct TempFile(PathBuf);

    impl Drop for TempFile {
        fn drop(&mut self) {
            // Best effort: the file may already be gone.
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn write_temp(test_name: &str, bytes: &[u8]) -> io::Result<TempFile> {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let name = format!(
            "dedupe-fiemap-{}-{test_name}-{nanos}.bin",
            std::process::id()
        );
        let path = std::env::temp_dir().join(name);
        std::fs::write(&path, bytes)?;
        Ok(TempFile(path))
    }

    #[test]
    fn physical_if_located_should_return_none_when_unknown_flag_set() {
        let got = physical_if_located(&extent_with_flags(0x0000_0002));
        assert_eq!(
            got, None,
            "FIEMAP_EXTENT_UNKNOWN marks no real location, got {got:?}"
        );
    }

    #[test]
    fn physical_if_located_should_return_none_when_delalloc_flag_set() {
        let got = physical_if_located(&extent_with_flags(0x0000_0004));
        assert_eq!(
            got, None,
            "FIEMAP_EXTENT_DELALLOC has no allocated block yet, got {got:?}"
        );
    }

    #[test]
    fn physical_if_located_should_return_none_when_data_inline_flag_set() {
        let got = physical_if_located(&extent_with_flags(0x0000_0200));
        assert_eq!(
            got, None,
            "FIEMAP_EXTENT_DATA_INLINE stores data in the inode, got {got:?}"
        );
    }

    #[test]
    fn physical_if_located_should_return_none_when_data_tail_flag_set() {
        let got = physical_if_located(&extent_with_flags(0x0000_0400));
        assert_eq!(
            got, None,
            "FIEMAP_EXTENT_DATA_TAIL packs data with other files, got {got:?}"
        );
    }

    #[test]
    fn first_extent_offset_should_return_none_when_file_empty() {
        let file = write_temp("empty", &[]).expect("temp file should be writable");
        let got = first_extent_offset(&file.0).expect("opening an existing file should succeed");
        assert_eq!(
            got, None,
            "an empty file has no mapped extents, so the offset should be None, got {got:?}"
        );
    }

    #[test]
    fn first_extent_offset_should_not_error_when_file_tiny() {
        let file = write_temp("tiny", b"abc").expect("temp file should be writable");
        let got = first_extent_offset(&file.0);
        assert!(
            got.is_ok(),
            "a 3-byte file may be inline or on an unsupported filesystem, but must not error, got {got:?}"
        );
    }

    #[test]
    fn first_extent_offset_should_return_error_when_file_missing() {
        let missing = Path::new("/nonexistent/dedupe/fiemap/probe.bin");
        let got = first_extent_offset(missing);
        assert!(
            got.is_err(),
            "a file that cannot be opened is an error, not a None offset, got {got:?}"
        );
    }

    #[test]
    fn physical_if_located_should_return_offset_when_no_flags_set() {
        let got = physical_if_located(&extent_with_flags(0));
        assert_eq!(
            got,
            Some(8192),
            "an extent with no flags should keep its physical offset, got {got:?}"
        );
    }

    #[test]
    fn physical_if_located_should_ignore_flags_without_location_meaning() {
        // LAST (0x1) and ENCODED (0x8) do not change where the data sits.
        let got = physical_if_located(&extent_with_flags(0x0000_0001 | 0x0000_0008));
        assert_eq!(
            got,
            Some(8192),
            "LAST and ENCODED should not suppress the offset, got {got:?}"
        );
    }
}
