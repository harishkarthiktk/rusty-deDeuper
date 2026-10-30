//! Windows storage probe. The storage stack reports whether a device incurs a seek penalty,
//! the closest storage-level proxy for rotational media (HDDs do, SSDs do not).

#![expect(
    unsafe_code,
    reason = "Win32 DeviceIoControl and drive queries; each unsafe block states its invariants"
)]

use std::mem::size_of;
use std::path::Path;
use std::ptr::{from_ref, null_mut};

use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
use windows_sys::Win32::System::Ioctl::{
    PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
    IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

use crate::parse::device_path::volume_device_path;
use crate::parse::seek_penalty::{interpret_seek_penalty, DESCRIPTOR_LEN};
use crate::probe::FsLocality;
use dedupe_win_sys::{open_handle, to_wide_nul, volume_root};

// Fails the build if the byte layout in `seek_penalty` drifts from the real struct.
const _: () = assert!(size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() == DESCRIPTOR_LEN);

/// Seek-penalty flag of the volume holding `path`, or `None` on any failure.
pub(super) fn query_seek_penalty(path: &Path) -> Option<bool> {
    let root = volume_root(path)?;
    let device = volume_device_path(&root)?;
    // Access 0 asks for the query only, which needs no read or write rights on the volume.
    let handle = open_handle(
        &to_wide_nul(&device),
        0,
        FILE_SHARE_READ | FILE_SHARE_WRITE,
        0,
    )
    .ok()?;

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceSeekPenaltyProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let query_len = u32::try_from(size_of::<STORAGE_PROPERTY_QUERY>()).ok()?;
    let mut descriptor = [0u8; DESCRIPTOR_LEN];
    let out_len = u32::try_from(DESCRIPTOR_LEN).ok()?;
    let mut returned: u32 = 0;
    // SAFETY: `handle` is a live volume handle that this frame owns. `query` is a live
    // `#[repr(C)]` value and `query_len` is its exact size, so the kernel reads only
    // inside it. `descriptor` is writable for `out_len` bytes and that is the size
    // passed, so the kernel cannot write past it. `returned` is a live u32 that
    // receives the byte count. The overlapped pointer is null, so the call is
    // synchronous and no pointer outlives it.
    let ok = unsafe {
        DeviceIoControl(
            handle.raw(),
            IOCTL_STORAGE_QUERY_PROPERTY,
            from_ref(&query).cast(),
            query_len,
            descriptor.as_mut_ptr().cast(),
            out_len,
            &mut returned,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }
    interpret_seek_penalty(&descriptor, usize::try_from(returned).ok()?)
}

pub(super) fn locality_windows(path: &Path) -> FsLocality {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetVolumePathNameW};
    // From <winbase.h>.
    const DRIVE_REMOTE: u32 = 4;
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut root = vec![0u16; wide.len().max(260) + 1];
    let len = root.len() as u32;
    // SAFETY: `wide` is NUL-terminated; `root` is writable for `len` units.
    if unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), len) } == 0 {
        return FsLocality::Unknown;
    }
    // SAFETY: `root` holds a NUL-terminated volume root written by the call above.
    if unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE {
        FsLocality::Remote
    } else {
        FsLocality::Local
    }
}
