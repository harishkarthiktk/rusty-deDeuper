//! macOS volume identity: `ATTR_VOL_UUID` via `getattrlist`, else fsid.

#![expect(
    unsafe_code,
    reason = "libc statfs and getattrlist; each unsafe block states its invariants"
)]

use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::path::RawPath;
use dedupe_core::traits::VolumeInfo;

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;

// From <sys/attr.h>.
const ATTR_VOL_INFO: u32 = 0x8000_0000;
const FSOPT_NOFOLLOW: libc::c_ulong = 0x0000_0001;

pub fn identify(path: &Path) -> Result<VolumeInfo> {
    let mount = crate::sys::unix::mount_root(path);
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| dedupe_core::CoreError::Encoding("nul byte in path".into()))?;

    // SAFETY: `libc::statfs` is plain old data; all-zero is a valid value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `st` is writable.
    if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
        return Err(dedupe_core::CoreError::Io(std::io::Error::last_os_error()));
    }
    let fs_type = dedupe_core::model::FsType::parse(&cstr_array(&st.f_fstypename));

    // Volume attributes need the volume root itself (getattrlist(2) returns EINVAL
    // otherwise), so use the mount point statfs reports, not the dev-climbed `mount`.
    let vol_root = CString::new(cstr_bytes(&st.f_mntonname))
        .map_err(|_| dedupe_core::CoreError::Encoding("nul byte in mount".into()))?;
    let (uuid, uuid_kind) = match get_volume_uuid(&vol_root) {
        Some(u) => (format_uuid(&u), dedupe_core::model::UuidKind::FsUuid),
        None => {
            tracing::warn!(mount = %mount.display(), "ATTR_VOL_UUID unavailable; fsid fallback");
            // SAFETY: `libc::fsid_t` is 8 bytes of two `i32` words (`repr(C)`, integers
            // only, private `val` on Apple), the same size and layout as `[i32; 2]`, and
            // every 8-byte pattern is a valid `[i32; 2]`.
            let fsid: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
            (
                format!("fsid:{:08x}{:08x}", fsid[0] as u32, fsid[1] as u32),
                dedupe_core::model::UuidKind::FsidFallback,
            )
        }
    };
    // `mount` stays the dev-climbed path in the root's own spelling: the walker keys
    // folders by those paths. Firmlinked roots (GEN-14) are a known follow-up.
    Ok(VolumeInfo {
        uuid,
        uuid_kind,
        mount: RawPath::new(mount.as_os_str().as_bytes().to_vec()),
        fs_type,
        label: None,
    })
}

/// `ATTR_VOL_UUID` of the volume mounted at `vol_root`. `None` on failure or all-zero.
fn get_volume_uuid(vol_root: &CString) -> Option<[u8; 16]> {
    let mut attr = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // Reply: u32 total length (includes itself), then uuid_t. No attribute_set_t,
    // because ATTR_CMN_RETURNED_ATTRS is not requested.
    let mut buf = [0u8; 4 + 16];
    // SAFETY: `vol_root` is NUL-terminated; `attr` and `buf` are live and writable for
    // the sizes passed.
    let rc = unsafe {
        libc::getattrlist(
            vol_root.as_ptr(),
            (&mut attr as *mut libc::attrlist).cast(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            FSOPT_NOFOLLOW,
        )
    };
    if rc != 0 {
        tracing::debug!(err = %std::io::Error::last_os_error(), "getattrlist ATTR_VOL_UUID failed");
        return None;
    }
    let len = u32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
    if len < buf.len() {
        return None;
    }
    let uuid: [u8; 16] = buf[4..20].try_into().ok()?;
    (uuid != [0u8; 16]).then_some(uuid)
}

/// Lower-case 8-4-4-4-12, no panics.
fn format_uuid(b: &[u8; 16]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        s.push(char::from(HEX[usize::from(byte >> 4)]));
        s.push(char::from(HEX[usize::from(byte & 0xf)]));
    }
    s
}

fn cstr_bytes(arr: &[libc::c_char]) -> Vec<u8> {
    arr.iter()
        .map(|&c| c as u8)
        .take_while(|&c| c != 0)
        .collect()
}

fn cstr_array(arr: &[libc::c_char]) -> String {
    String::from_utf8_lossy(&cstr_bytes(arr)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::format_uuid;

    #[test]
    fn format_uuid_should_group_bytes_as_8_4_4_4_12_lowercase_hex() {
        let b: [u8; 16] = [
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66,
            0x77, 0x88,
        ];
        assert_eq!(format_uuid(&b), "12345678-9abc-def0-1122-334455667788");
    }

    #[test]
    fn format_uuid_should_return_36_chars_when_input_is_all_zero() {
        let s = format_uuid(&[0u8; 16]);
        assert_eq!(s, "00000000-0000-0000-0000-000000000000");
        assert_eq!(s.len(), 36);
    }

    #[test]
    fn format_uuid_should_emit_lowercase_when_input_is_all_ff() {
        assert_eq!(
            format_uuid(&[0xFF; 16]),
            "ffffffff-ffff-ffff-ffff-ffffffffffff"
        );
    }
}
