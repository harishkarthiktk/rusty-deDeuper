//! Windows lookup. Only the first extent is read through `FSCTL_GET_RETRIEVAL_POINTERS`,
//! so the physical offset is the first extent's LCN times the volume's cluster size.

#![expect(
    unsafe_code,
    reason = "Win32 DeviceIoControl and GetDiskFreeSpaceW; each unsafe block states its invariants"
)]

use std::collections::HashMap;
use std::io;
use std::mem::size_of;
use std::path::Path;
use std::ptr::{from_ref, null_mut};
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

use windows_sys::Win32::Foundation::ERROR_MORE_DATA;
use windows_sys::Win32::Storage::FileSystem::{
    GetDiskFreeSpaceW, FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::Ioctl::{FSCTL_GET_RETRIEVAL_POINTERS, STARTING_VCN_INPUT_BUFFER};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::parse::extents::{first_lcn, lcn_to_byte_offset};
use dedupe_win_sys::{open_handle, to_wide_nul, volume_root_cached, OwnedHandle};

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
/// `GetDiskFreeSpaceW` call per file. The volume root comes from the per-directory memo
/// in `volume_root_cached`, so a warm lookup costs no `GetVolumePathNameW` call. Warm
/// hits take a shared lock. A poisoned lock still guards a valid map, because each insert
/// is a single statement, so the guard is recovered, not propagated.
fn bytes_per_cluster(path: &Path) -> Option<u64> {
    static CACHE: OnceLock<RwLock<HashMap<Arc<str>, u64>>> = OnceLock::new();
    let root = volume_root_cached(path)?;
    let cache = CACHE.get_or_init(Default::default);
    if let Some(&size) = cache
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&*root)
    {
        return Some(size);
    }
    let size = cluster_size(&root)?;
    cache
        .write()
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
