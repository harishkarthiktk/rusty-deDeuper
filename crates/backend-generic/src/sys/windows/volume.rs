//! Windows volume identity: volume GUID, else volume serial.

#![expect(
    unsafe_code,
    reason = "Win32 volume queries; each unsafe block states its invariants"
)]

use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::traits::{PathCodec, VolumeInfo};
use windows_sys::Win32::Foundation::MAX_PATH;
use windows_sys::Win32::Storage::FileSystem::{
    GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
};

use crate::parse::wide::{fs_type_from_wide, match_verbatim};

pub fn identify(path: &Path) -> Result<VolumeInfo> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // `GetVolumePathNameW` docs advise a buffer at least as long as the input path.
    let cap = wide.len().max(MAX_PATH as usize + 1);
    let mut vol_path = vec![0u16; cap];
    let len = u32::try_from(cap).map_err(|e| dedupe_core::CoreError::InvalidArg(e.to_string()))?;
    // SAFETY: `wide` is NUL-terminated; `vol_path` is writable for `len` units.
    let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), vol_path.as_mut_ptr(), len) };
    if ok == 0 {
        return Err(dedupe_core::CoreError::Io(std::io::Error::last_os_error()));
    }

    let root = until_nul(&vol_path);
    let (serial, fs_type) = volume_info(&vol_path);
    // Identity text only, never used as a path.
    let root_key = String::from_utf16_lossy(root).to_lowercase();
    let (uuid, uuid_kind) = match volume_guid(&vol_path) {
        Some(guid) => (guid, dedupe_core::model::UuidKind::FsUuid),
        // Mapped, UNC and subst roots have no volume GUID. The root is part of the id
        // because several shares of one NAS volume report the same serial.
        None if serial != 0 => (
            format!("serial:{serial:08x}:{root_key}"),
            dedupe_core::model::UuidKind::VolumeSerial,
        ),
        None => {
            tracing::warn!(root = %root_key, "no volume GUID or serial; weak identity");
            (
                format!("none:{}:{root_key}", fs_type.as_str()),
                dedupe_core::model::UuidKind::None,
            )
        }
    };

    // Spell the mount the way the caller spelled the path (verbatim or not), so
    // `relative_components` can strip it as a byte prefix.
    let input: Vec<u16> = path.as_os_str().encode_wide().collect();
    let mount_wide = match_verbatim(&input, root);
    let mount = crate::codec::GenericPathCodec.to_raw(&std::ffi::OsString::from_wide(&mount_wide));

    Ok(VolumeInfo {
        uuid,
        uuid_kind,
        mount,
        fs_type,
        label: None,
    })
}

/// `\\?\Volume{GUID}\` for a local mount point, `None` for UNC, mapped or subst roots.
fn volume_guid(vol_path: &[u16]) -> Option<String> {
    // MAX_PATH + 1 units: the volume GUID path is about 50 units, and this matches the
    // name buffer size documented for the volume APIs.
    let mut name = vec![0u16; MAX_PATH as usize + 1];
    // SAFETY: `vol_path` is NUL-terminated; `name` is writable for its length.
    let ok = unsafe {
        GetVolumeNameForVolumeMountPointW(vol_path.as_ptr(), name.as_mut_ptr(), name.len() as u32)
    };
    if ok == 0 {
        return None;
    }
    let guid = String::from_utf16_lossy(until_nul(&name));
    (!guid.trim_matches('\\').is_empty()).then_some(guid)
}

fn until_nul(buf: &[u16]) -> &[u16] {
    &buf[..buf.iter().position(|&c| c == 0).unwrap_or(buf.len())]
}

/// Volume serial and file-system type. `(0, Other)` when the query fails.
fn volume_info(vol_path: &[u16]) -> (u32, dedupe_core::model::FsType) {
    let mut name_buf = vec![0u16; MAX_PATH as usize + 1];
    let mut serial = 0u32;
    let mut max_len = 0u32;
    let mut flags = 0u32;
    let mut fs_name = vec![0u16; MAX_PATH as usize + 1];
    // SAFETY: `vol_path` is NUL-terminated; `name_buf` and `fs_name` are writable for
    // the lengths passed; `serial`, `max_len` and `flags` are live out-parameters.
    let ok = unsafe {
        GetVolumeInformationW(
            vol_path.as_ptr(),
            name_buf.as_mut_ptr(),
            name_buf.len() as u32,
            &mut serial,
            &mut max_len,
            &mut flags,
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };
    if ok == 0 {
        (0, dedupe_core::model::FsType::Other)
    } else {
        (serial, fs_type_from_wide(&fs_name))
    }
}
