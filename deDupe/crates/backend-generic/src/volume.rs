//! Per-OS volume identity. See `.plan.md` 4.10.
//!
//! Prefer the filesystem UUID / volume GUID; fall back to an fsid. `UuidKind::None`
//! synthesizes `"none:<fs_type>:<mount>"` and warns (weak resume guard).

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;

use dedupe_core::error::Result;
// Windows builds construct `RawPath` only through the `PathCodec` in `imp`, so the
// top-level import would be unused there.
#[cfg(not(windows))]
use dedupe_core::path::RawPath;
use dedupe_core::traits::{VolumeIdentity, VolumeInfo};

#[derive(Clone, Copy, Debug, Default)]
pub struct GenericVolumeIdentity;

impl VolumeIdentity for GenericVolumeIdentity {
    fn identify(&self, path: &Path) -> Result<VolumeInfo> {
        imp::identify(path)
    }
}

/// Walk up until the device id changes; that is the mount root.
#[cfg(unix)]
fn mount_root(path: &Path) -> PathBuf {
    use std::os::unix::fs::MetadataExt;
    let mut cur = path.to_path_buf();
    while let Some(parent) = cur.parent() {
        match (cur.metadata(), parent.metadata()) {
            (Ok(a), Ok(b)) if a.dev() == b.dev() => cur = parent.to_path_buf(),
            _ => break,
        }
    }
    cur
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let md = std::fs::metadata(path)?;
        let dev = md.dev();

        let mut fs_uuid: Option<String> = None;
        if let Ok(rd) = std::fs::read_dir("/dev/disk/by-uuid") {
            for entry in rd.flatten() {
                let link = entry.path();
                if let Ok(target) = std::fs::metadata(&link) {
                    if target.rdev() == dev {
                        fs_uuid = Some(entry.file_name().to_string_lossy().into_owned());
                        break;
                    }
                }
            }
        }

        let mount = super::mount_root(path);
        let st = rustix::fs::statfs(&mount).map_err(|e| dedupe_core::CoreError::Io(e.into()))?;
        let fs_type = fs_type_from_magic(st.f_type as i64);

        let (uuid, uuid_kind) = match fs_uuid {
            Some(u) => (u, dedupe_core::model::UuidKind::FsUuid),
            None => {
                tracing::warn!(mount = %mount.display(), "no fs UUID; using fsid fallback");
                (
                    format!("fsid:{:016x}", st.f_fsid),
                    dedupe_core::model::UuidKind::FsidFallback,
                )
            }
        };

        Ok(VolumeInfo {
            uuid,
            uuid_kind,
            mount: RawPath::new(mount.as_os_str().as_bytes().to_vec()),
            fs_type,
            label: None,
        })
    }

    fn fs_type_from_magic(magic: i64) -> dedupe_core::model::FsType {
        use dedupe_core::model::FsType;
        const EXT: i64 = 0xEF53;
        const XFS: i64 = 0x5846_5342;
        const BTRFS: i64 = 0x9123_683E;
        match magic {
            EXT => FsType::Ext4,
            XFS => FsType::Xfs,
            BTRFS => FsType::Btrfs,
            _ => FsType::Other,
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let mount = super::mount_root(path);
        let c = CString::new(mount.as_os_str().as_bytes())
            .map_err(|_| dedupe_core::CoreError::Encoding("nul byte in mount path".into()))?;

        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::statfs(c.as_ptr(), &mut st) };
        if rc != 0 {
            return Err(dedupe_core::CoreError::Io(std::io::Error::last_os_error()));
        }
        let fs_type = dedupe_core::model::FsType::parse(&cstr_array(&st.f_fstypename));

        let (uuid, uuid_kind) = match get_volume_uuid(&c) {
            Some(u) if u.chars().any(|ch| ch != '0' && ch != '-') => {
                (u, dedupe_core::model::UuidKind::FsUuid)
            }
            _ => {
                tracing::warn!(mount = %mount.display(), "ATTR_VOL_UUID unavailable; fsid fallback");
                let fsid: [i32; 2] = unsafe { std::mem::transmute(st.f_fsid) };
                (
                    format!("fsid:{:08x}{:08x}", fsid[0] as u32, fsid[1] as u32),
                    dedupe_core::model::UuidKind::FsidFallback,
                )
            }
        };
        let _ = &st;
        Ok(VolumeInfo {
            uuid,
            uuid_kind,
            mount: RawPath::new(mount.as_os_str().as_bytes().to_vec()),
            fs_type,
            label: None,
        })
    }

    fn get_volume_uuid(c: &CString) -> Option<String> {
        let mut attr = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: 0,
            volattr: libc::ATTR_VOL_UUID,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        let mut buf = [0u8; 1024];
        let rc = unsafe {
            libc::getattrlist(
                c.as_ptr(),
                &mut attr as *mut libc::attrlist as *mut libc::c_void,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
            )
        };
        if rc != 0 || buf.len() < 36 {
            return None;
        }
        // attribute_set_t is 5 * u32 (20 bytes); ATTR_VOL_UUID is a raw uuid_t.
        let uuid = &buf[20..36];
        Some(format_uuid(uuid))
    }

    fn format_uuid(b: &[u8]) -> String {
        let hex = |s: &[u8]| -> String {
            let mut out = String::new();
            for byte in s {
                out.push(char::from_digit((byte >> 4) as u32, 16).unwrap());
                out.push(char::from_digit((byte & 0xf) as u32, 16).unwrap());
            }
            out
        };
        format!(
            "{}-{}-{}-{}-{}",
            hex(&b[0..4]),
            hex(&b[4..6]),
            hex(&b[6..8]),
            hex(&b[8..10]),
            hex(&b[10..16])
        )
    }

    fn cstr_array(arr: &[libc::c_char]) -> String {
        let bytes: Vec<u8> = arr
            .iter()
            .map(|&c| c as u8)
            .take_while(|&c| c != 0)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

#[cfg(windows)]
mod imp {
    use super::*;
    use dedupe_core::traits::PathCodec;
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let mut vol_path = vec![0u16; 261];
        let ok = unsafe {
            GetVolumePathNameW(wide.as_ptr(), vol_path.as_mut_ptr(), vol_path.len() as u32)
        };
        if ok == 0 {
            return Err(dedupe_core::CoreError::Io(std::io::Error::last_os_error()));
        }

        let mut name = vec![0u16; 261];
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(
                vol_path.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        };
        if ok == 0 {
            return Err(dedupe_core::CoreError::Io(std::io::Error::last_os_error()));
        }
        let nlen = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let guid = String::from_utf16_lossy(&name[..nlen]);

        // GUID is the primary identity; fall back to the NTFS serial only when the
        // GUID is empty (4.10). No schema change: the serial lives in `volumes.uuid`.
        let (uuid, uuid_kind) = if guid.trim_matches('\\').is_empty() {
            let serial = volume_serial(&vol_path);
            (
                format!("serial:{serial:08x}"),
                dedupe_core::model::UuidKind::VolumeSerial,
            )
        } else {
            (guid, dedupe_core::model::UuidKind::FsUuid)
        };

        let mount =
            crate::codec::GenericPathCodec.to_raw(std::ffi::OsStr::new(&String::from_utf16_lossy(
                &vol_path[..vol_path
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(vol_path.len())],
            )));

        Ok(VolumeInfo {
            uuid,
            uuid_kind,
            mount,
            fs_type: dedupe_core::model::FsType::Ntfs,
            label: None,
        })
    }

    fn volume_serial(vol_path: &[u16]) -> u32 {
        let mut name_buf = vec![0u16; 261];
        let mut serial = 0u32;
        let mut max_len = 0u32;
        let mut flags = 0u32;
        let mut fs_name = vec![0u16; 261];
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
            0
        } else {
            serial
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod imp {
    use super::*;

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let mount = path.to_path_buf();
        let display = mount.to_string_lossy().into_owned();
        Ok(VolumeInfo {
            uuid: format!("none:other:{display}"),
            uuid_kind: dedupe_core::model::UuidKind::None,
            mount: RawPath::new(mount.to_string_lossy().as_bytes().to_vec()),
            fs_type: dedupe_core::model::FsType::Other,
            label: None,
        })
    }
}
