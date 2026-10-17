//! First physical extent of a file: `FS_IOC_FIEMAP` on Linux and
//! `FSCTL_GET_RETRIEVAL_POINTERS` on Windows.
//!
//! The offset orders HDD scans so the head sweeps the platter once instead of
//! seeking per file. It is a scheduling hint, never a correctness input.

#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::path::Path;

/// `_IOWR('f', 11, struct fiemap)` from `<linux/fs.h>`. Defined locally because the
/// `libc` crate does not expose it on every target.
#[cfg(target_os = "linux")]
const FS_IOC_FIEMAP: u32 = 0xC020_660B;

/// Mirrors `struct fiemap` from `<linux/fiemap.h>`. The kernel's flexible array is
/// modelled as one trailing element, which matches the layout for `fm_extent_count = 1`.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[expect(
    dead_code,
    reason = "the kernel fills and we read only fm_mapped_extents and fm_extents; the other fields must exist for the layout"
)]
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
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy, Default)]
#[expect(
    dead_code,
    reason = "only fe_physical is read; the remaining fields must exist for the layout"
)]
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
#[cfg(target_os = "linux")]
pub fn first_extent_offset(path: &Path) -> std::io::Result<Option<u64>> {
    let file = File::open(path)?;
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
    Ok(Some(req.fm_extents[0].fe_physical))
}

/// Windows lookup. Only the first extent is read through `FSCTL_GET_RETRIEVAL_POINTERS`,
/// so the physical offset is the first extent's LCN times the volume's cluster size.
#[cfg(windows)]
pub use win::first_extent_offset;

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::io;
    use std::mem::size_of;
    use std::path::Path;
    use std::ptr::{from_ref, null_mut};
    use std::sync::{Mutex, OnceLock, PoisonError};

    use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_GET_RETRIEVAL_POINTERS, STARTING_VCN_INPUT_BUFFER,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{first_lcn, lcn_to_byte_offset};
    use crate::win_sys::{open_handle, to_wide_nul, volume_root, OwnedHandle};

    /// Output length for the retrieval call: the 16-byte header plus room for several
    /// extents. Only the first extent is read, so this just has to hold one.
    const RETRIEVAL_BUF_LEN: u32 = 256;

    /// Input length for the retrieval call (`STARTING_VCN_INPUT_BUFFER`, 8 bytes).
    const INPUT_LEN: u32 = size_of::<STARTING_VCN_INPUT_BUFFER>() as u32;

    /// Physical byte offset of the first extent of `path`.
    ///
    /// Open failure is an error, as on Linux. Every failure after the open folds into
    /// `Ok(None)`, because the offset is only a read-order hint.
    pub fn first_extent_offset(path: &Path) -> io::Result<Option<u64>> {
        // Read-attributes access needs no read rights, so the lookup works on files the
        // scan can enumerate but not read. Share flags let the scan and this lookup coexist
        // with other handles on the same file.
        let handle = open_handle(
            &to_wide_nul(path.as_os_str()),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS,
        )?;
        let Some(lcn) = first_lcn_of(&handle) else {
            return Ok(None);
        };
        let Some(bytes_per_cluster) = bytes_per_cluster(path) else {
            return Ok(None);
        };
        Ok(lcn_to_byte_offset(lcn, bytes_per_cluster))
    }

    /// LCN of the extent that starts at VCN 0. `None` for resident, sparse, empty or
    /// unreadable files, which all mean "no physical offset to order by".
    fn first_lcn_of(handle: &OwnedHandle) -> Option<i64> {
        let input = STARTING_VCN_INPUT_BUFFER { StartingVcn: 0 };
        let mut out = [0u8; RETRIEVAL_BUF_LEN as usize];
        let mut returned: u32 = 0;
        // SAFETY: `handle` is a live handle that this frame owns. `input` is a live
        // `#[repr(C)]` value and `INPUT_LEN` is its exact size, so the kernel reads only
        // inside it. `out` is writable for `RETRIEVAL_BUF_LEN` bytes and that is the size
        // passed, so the kernel cannot write past it. `returned` is a live u32 that
        // receives the byte count. The overlapped pointer is null, so the call is
        // synchronous and no pointer outlives it.
        let ok = unsafe {
            DeviceIoControl(
                handle.raw(),
                FSCTL_GET_RETRIEVAL_POINTERS,
                from_ref(&input).cast(),
                INPUT_LEN,
                out.as_mut_ptr().cast(),
                RETRIEVAL_BUF_LEN,
                &mut returned,
                null_mut(),
            )
        };
        // ERROR_MORE_DATA means the buffer could not hold every extent. The first extent
        // is still written, and `first_lcn` only trusts bytes up to `returned`.
        if ok == 0 && io::Error::last_os_error().raw_os_error() != Some(ERROR_MORE_DATA as i32) {
            return None;
        }
        first_lcn(&out, usize::try_from(returned).ok()?)
    }

    /// Bytes per cluster of the volume holding `path`, cached per volume root.
    ///
    /// Every file on a volume asks for the same value, so the cache saves one
    /// `GetDiskFreeSpaceW` call per file. A poisoned lock still guards a valid map,
    /// because each insert is a single statement, so the guard is recovered, not propagated.
    fn bytes_per_cluster(path: &Path) -> Option<u64> {
        static CACHE: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();
        let root = volume_root(path)?;
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        let cached = cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&root)
            .copied();
        if let Some(size) = cached {
            return Some(size);
        }
        let size = cluster_size(&root)?;
        cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(root, size);
        Some(size)
    }

    /// Cluster size from `GetDiskFreeSpaceW`. Only the geometry is used. The free and
    /// total cluster counts from the same call are not needed and are ignored.
    fn cluster_size(root: &str) -> Option<u64> {
        let wide = to_wide_nul(root);
        let mut sectors_per_cluster: u32 = 0;
        let mut bytes_per_sector: u32 = 0;
        let mut free_clusters: u32 = 0;
        let mut total_clusters: u32 = 0;
        // SAFETY: `wide` is NUL-terminated and alive for the call. Each out-pointer is a
        // live, writable u32 local, so the call writes only into this frame.
        let ok = unsafe {
            GetDiskFreeSpaceW(
                wide.as_ptr(),
                &mut sectors_per_cluster,
                &mut bytes_per_sector,
                &mut free_clusters,
                &mut total_clusters,
            )
        };
        if ok == 0 {
            return None;
        }
        u64::from(sectors_per_cluster).checked_mul(u64::from(bytes_per_sector))
    }
}

/// LCN of the first extent in a `RETRIEVAL_POINTERS_BUFFER`, or `None`.
///
/// Layout (little-endian): `ExtentCount` u32 at 0, `StartingVcn` i64 at 8 (4 bytes of
/// padding before it), then extents of `{ NextVcn i64, Lcn i64 }`, the first at offset 16.
/// `returned` is the byte count the kernel reported. Only bytes up to it are trusted.
/// An LCN of -1 marks a sparse or virtual extent, so it is `None` like any other
/// negative value.
#[cfg(any(windows, test))]
fn first_lcn(buf: &[u8], returned: usize) -> Option<i64> {
    const HEADER_LEN: usize = 16;
    const EXTENT_LEN: usize = 16;
    const LCN_OFFSET_IN_EXTENT: usize = 8;

    let valid = buf.get(..returned)?;
    let extent_count = u32::from_le_bytes(valid.get(0..4)?.try_into().ok()?);
    if extent_count == 0 {
        return None;
    }
    let first = valid.get(HEADER_LEN..HEADER_LEN + EXTENT_LEN)?;
    let lcn_bytes = first.get(LCN_OFFSET_IN_EXTENT..LCN_OFFSET_IN_EXTENT + 8)?;
    let lcn = i64::from_le_bytes(lcn_bytes.try_into().ok()?);
    (lcn >= 0).then_some(lcn)
}

/// Byte offset of a cluster: `lcn * bytes_per_cluster`, with checked arithmetic.
///
/// `None` for a negative LCN, a zero cluster size (which would collapse every file to
/// offset 0 and destroy the ordering), or overflow.
#[cfg(any(windows, test))]
fn lcn_to_byte_offset(lcn: i64, bytes_per_cluster: u64) -> Option<u64> {
    let cluster = u64::try_from(lcn).ok()?;
    if bytes_per_cluster == 0 {
        return None;
    }
    cluster.checked_mul(bytes_per_cluster)
}

#[cfg(test)]
mod tests {
    use super::{first_lcn, lcn_to_byte_offset};

    /// A retrieval buffer holding one extent with the given LCN. `extent_count` is written
    /// as-is so a test can describe a zero-extent reply.
    fn retrieval_bytes(extent_count: u32, lcn: i64) -> Vec<u8> {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&extent_count.to_le_bytes());
        buf[8..16].copy_from_slice(&0i64.to_le_bytes());
        buf[16..24].copy_from_slice(&4i64.to_le_bytes());
        buf[24..32].copy_from_slice(&lcn.to_le_bytes());
        buf
    }

    #[test]
    fn first_lcn_should_return_lcn_when_one_extent_present() {
        let buf = retrieval_bytes(1, 4096);
        let parsed = first_lcn(&buf, buf.len());
        assert_eq!(
            parsed,
            Some(4096),
            "one extent with Lcn 4096 should parse to Some(4096), got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_zero_extents() {
        let buf = retrieval_bytes(0, 4096);
        let parsed = first_lcn(&buf, buf.len());
        assert_eq!(
            parsed, None,
            "ExtentCount 0 should give None even with an Lcn present, got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_buffer_truncated_before_extent() {
        let buf = retrieval_bytes(1, 4096);
        let parsed = first_lcn(&buf, 31);
        assert_eq!(
            parsed, None,
            "returned 31 bytes cuts the first extent short, so the Lcn is unreadable, got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_returned_exceeds_buffer() {
        let buf = retrieval_bytes(1, 4096);
        let parsed = first_lcn(&buf, buf.len() + 1);
        assert_eq!(
            parsed, None,
            "a returned count past the buffer end must not be trusted, got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_lcn_is_minus_one() {
        let buf = retrieval_bytes(1, -1);
        let parsed = first_lcn(&buf, buf.len());
        assert_eq!(
            parsed, None,
            "Lcn -1 marks a sparse extent and should give None, got {parsed:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_multiply_lcn_by_cluster_size() {
        let offset = lcn_to_byte_offset(3, 4096);
        assert_eq!(
            offset,
            Some(12_288),
            "Lcn 3 at 4096 bytes per cluster should be byte 12288, got {offset:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_return_none_when_cluster_size_zero() {
        let offset = lcn_to_byte_offset(3, 0);
        assert_eq!(
            offset, None,
            "a zero cluster size is invalid and should give None, got {offset:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_return_none_when_multiplication_overflows() {
        let offset = lcn_to_byte_offset(i64::MAX, u64::MAX);
        assert_eq!(
            offset, None,
            "an offset past u64::MAX should give None, got {offset:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_return_none_when_lcn_negative() {
        let offset = lcn_to_byte_offset(-1, 4096);
        assert_eq!(
            offset, None,
            "a negative Lcn has no physical offset and should give None, got {offset:?}"
        );
    }
}
