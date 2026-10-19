//! Per-OS volume identity. See `.overall_plan.md` 4.10.
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

/// Spell `root` (from `GetVolumePathNameW`) the way `input` is spelled: verbatim (`\\?\`,
/// `\\?\UNC\`) or not, so `relative_components` can strip it as a byte prefix.
#[cfg(any(windows, test))]
fn match_verbatim(input: &[u16], root: &[u16]) -> Vec<u16> {
    let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let (vb, unc_vb, unc) = (w(r"\\?\"), w(r"\\?\UNC\"), w(r"\\"));
    let in_vb = input.starts_with(&vb);
    let root_vb = root.starts_with(&vb);
    match (in_vb, root_vb) {
        (true, false) if root.starts_with(&unc) => [&unc_vb[..], &root[2..]].concat(),
        (true, false) => [&vb[..], root].concat(),
        (false, true) if root.starts_with(&unc_vb) => [&unc[..], &root[unc_vb.len()..]].concat(),
        (false, true) if !root[vb.len()..].starts_with(&w("Volume{")) => root[vb.len()..].to_vec(),
        _ => root.to_vec(),
    }
}

/// File-system name from a `GetVolumeInformationW` buffer, up to the first NUL.
#[cfg(any(windows, test))]
fn fs_type_from_wide(name: &[u16]) -> dedupe_core::model::FsType {
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    dedupe_core::model::FsType::parse(&String::from_utf16_lossy(&name[..len]))
}

#[cfg(test)]
mod match_verbatim_tests {
    use super::match_verbatim;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn match_verbatim_should_spell_root_like_input_when_forms_differ() {
        let cases = [
            // Plain drive input, plain root: unchanged.
            (r"C:\dir\f", r"C:\", r"C:\"),
            // Verbatim input, plain root: add the verbatim prefix.
            (r"\\?\C:\dir\f", r"C:\", r"\\?\C:\"),
            // Verbatim input, plain UNC root: verbatim UNC form.
            (
                r"\\?\UNC\srv\share\f",
                r"\\srv\share\",
                r"\\?\UNC\srv\share\",
            ),
            // Plain UNC input, verbatim UNC root: strip to plain UNC.
            (r"\\srv\share\f", r"\\?\UNC\srv\share\", r"\\srv\share\"),
            // Plain input, verbatim drive root: strip the prefix.
            (r"C:\dir\f", r"\\?\C:\", r"C:\"),
            // Volume GUID root is kept verbatim for a plain input.
            (r"C:\dir\f", r"\\?\Volume{1234}\", r"\\?\Volume{1234}\"),
            // Already matching verbatim input and root: unchanged.
            (r"\\?\C:\dir\f", r"\\?\C:\", r"\\?\C:\"),
        ];
        for (input, root, want) in cases {
            assert_eq!(
                match_verbatim(&wide(input), &wide(root)),
                wide(want),
                "input {input:?}, root {root:?}"
            );
        }
    }
}

#[cfg(test)]
mod fs_type_tests {
    use super::fs_type_from_wide;
    use dedupe_core::model::FsType;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn fs_type_from_wide_should_return_ntfs_when_name_is_ntfs() {
        assert_eq!(fs_type_from_wide(&wide("NTFS")), FsType::Ntfs, "NTFS name");
    }

    #[test]
    fn fs_type_from_wide_should_return_fat32_when_name_is_fat32() {
        assert_eq!(
            fs_type_from_wide(&wide("FAT32")),
            FsType::Fat32,
            "FAT32 name"
        );
    }

    #[test]
    fn fs_type_from_wide_should_return_exfat_when_name_is_exfat() {
        assert_eq!(
            fs_type_from_wide(&wide("exFAT")),
            FsType::Exfat,
            "exFAT name"
        );
    }

    #[test]
    fn fs_type_from_wide_should_return_other_when_name_is_refs_or_empty() {
        assert_eq!(
            fs_type_from_wide(&wide("ReFS")),
            FsType::Other,
            "ReFS has no variant"
        );
        assert_eq!(fs_type_from_wide(&[]), FsType::Other, "empty buffer");
    }

    #[test]
    fn fs_type_from_wide_should_match_case_insensitively_when_name_is_lowercase() {
        assert_eq!(fs_type_from_wide(&wide("ntfs")), FsType::Ntfs, "lowercase");
    }

    #[test]
    fn fs_type_from_wide_should_stop_at_first_nul_when_buffer_has_trailing_data() {
        let mut buf = wide("NTFS");
        buf.extend(wide("FAT32"));
        assert_eq!(fs_type_from_wide(&buf), FsType::Ntfs, "embedded NUL");
    }

    #[test]
    fn fs_type_from_wide_should_use_full_length_when_buffer_has_no_nul() {
        let buf: Vec<u16> = "exFAT".encode_utf16().collect();
        assert_eq!(fs_type_from_wide(&buf), FsType::Exfat, "no terminator");
    }

    #[test]
    fn fs_type_from_wide_should_return_other_when_name_has_unpaired_surrogate() {
        let buf = [0xD800_u16, 0x0041, 0];
        assert_eq!(fs_type_from_wide(&buf), FsType::Other, "lossy UTF-16");
    }
}

#[cfg(target_os = "linux")]
pub(crate) mod imp {
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
        // Truncate, never sign-extend: on 32-bit targets a negative `f_type` would otherwise
        // never match a magic such as BTRFS (0x9123_683E).
        let fs_type = fs_type_from_magic(st.f_type as u32);

        let (uuid, uuid_kind) = match fs_uuid {
            Some(u) => (u, dedupe_core::model::UuidKind::FsUuid),
            None if st.f_fsid.val != [0, 0] => {
                tracing::warn!(mount = %mount.display(), "no fs UUID; using fsid fallback");
                (
                    format!(
                        "fsid:{:08x}{:08x}",
                        st.f_fsid.val[0] as u32, st.f_fsid.val[1] as u32
                    ),
                    dedupe_core::model::UuidKind::FsidFallback,
                )
            }
            // FUSE and some network filesystems leave f_fsid zero; every such mount
            // would otherwise share one identity.
            None => {
                let (fstype, source) = mountinfo_entry(dev)
                    .unwrap_or_else(|| (fs_type.as_str().to_owned(), mount.display().to_string()));
                tracing::warn!(
                    mount = %mount.display(), %fstype,
                    "no fs UUID and zero fsid; weak identity"
                );
                (
                    format!("none:{fstype}:{source}"),
                    dedupe_core::model::UuidKind::None,
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

    /// `(fstype, source)` of the mount whose `major:minor` equals `dev`.
    /// mountinfo: `id parent maj:min root mountpoint opts [optional...] - fstype source superopts`.
    fn mountinfo_entry(dev: u64) -> Option<(String, String)> {
        let want = format!("{}:{}", rustix::fs::major(dev), rustix::fs::minor(dev));
        let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        parse_mountinfo(&text, &want)
    }

    /// Fields 1-3 never contain spaces (the mount point is escaped), so splitting on
    /// spaces is safe up to the `-` separator.
    pub(crate) fn parse_mountinfo(text: &str, want: &str) -> Option<(String, String)> {
        text.lines().find_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            if pre.split(' ').nth(2)? != want {
                return None;
            }
            let mut f = post.split(' ');
            let fstype = f.next()?.to_owned();
            let source = unescape_octal(f.next()?);
            Some((fstype, source))
        })
    }

    /// Decodes the kernel's `\040`-style octal escapes (space, tab, newline, backslash).
    pub(crate) fn unescape_octal(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            let oct = b
                .get(i + 1..i + 4)
                .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
            match (b[i], oct) {
                (b'\\', Some(d)) => {
                    out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
                    i += 4;
                }
                (c, _) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// `statfs.f_type` magic numbers from `linux/magic.h`. `NTFS3` is the in-kernel
    /// driver's magic (the legacy `NTFS` one is distinct). Values from memory; confirm
    /// against the kernel headers before relying on them.
    fn fs_type_from_magic(magic: u32) -> dedupe_core::model::FsType {
        use dedupe_core::model::FsType;
        const EXT: u32 = 0xEF53;
        const XFS: u32 = 0x5846_5342;
        const BTRFS: u32 = 0x9123_683E;
        const NTFS: u32 = 0x5346_544E;
        const NTFS3: u32 = 0x7366_746E;
        const MSDOS: u32 = 0x4D44;
        const EXFAT: u32 = 0x2011_BAB0;
        match magic {
            EXT => FsType::Ext4,
            XFS => FsType::Xfs,
            BTRFS => FsType::Btrfs,
            NTFS | NTFS3 => FsType::Ntfs,
            MSDOS => FsType::Fat32,
            EXFAT => FsType::Exfat,
            _ => FsType::Other,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{fs_type_from_magic, parse_mountinfo, unescape_octal};
        use dedupe_core::model::FsType;

        #[test]
        fn fs_type_from_magic_should_map_known_magics_when_given_linux_statfs_values() {
            let cases = [
                (0xEF53, FsType::Ext4),
                (0x5846_5342, FsType::Xfs),
                (0x9123_683E, FsType::Btrfs),
                (0x5346_544E, FsType::Ntfs),
                (0x7366_746E, FsType::Ntfs),
                (0x4D44, FsType::Fat32),
                (0x2011_BAB0, FsType::Exfat),
                (0x6573_5546, FsType::Other),
            ];
            for (magic, want) in cases {
                assert_eq!(fs_type_from_magic(magic), want, "magic {magic:#x}");
            }
        }

        #[test]
        fn parse_mountinfo_should_return_fstype_and_source_when_mount_matches_dev() {
            let text = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
36 22 0:45 / /mnt/my\\040share rw,nosuid - fuse.sshfs user@host:/srv rw
40 22 0:50 / /mnt/plain rw - nfs4 server:/export rw
";
            assert_eq!(
                parse_mountinfo(text, "0:45"),
                Some(("fuse.sshfs".to_owned(), "user@host:/srv".to_owned()))
            );
            assert_eq!(
                parse_mountinfo(text, "8:1"),
                Some(("ext4".to_owned(), "/dev/sda1".to_owned()))
            );
            assert_eq!(parse_mountinfo(text, "9:9"), None, "unknown dev");
        }

        #[test]
        fn parse_mountinfo_should_skip_optional_fields_when_present() {
            let text = "51 22 0:60 / /x rw shared:3 master:2 - cifs //srv/share rw\n";
            assert_eq!(
                parse_mountinfo(text, "0:60"),
                Some(("cifs".to_owned(), "//srv/share".to_owned()))
            );
        }

        #[test]
        fn unescape_octal_should_decode_space_tab_newline_and_backslash() {
            assert_eq!(unescape_octal(r"a\040b"), "a b");
            assert_eq!(unescape_octal(r"a\011b"), "a\tb");
            assert_eq!(unescape_octal(r"a\012b"), "a\nb");
            assert_eq!(unescape_octal(r"a\134b"), "a\\b");
            assert_eq!(
                unescape_octal(r"a\99b"),
                r"a\99b",
                "non-octal digits stay literal"
            );
        }

        #[test]
        fn parse_mountinfo_should_return_none_when_line_is_malformed() {
            let no_separator = "1 2 0:70 / /x rw ext4 /dev/x rw\n";
            assert_eq!(parse_mountinfo(no_separator, "0:70"), None, "no ' - '");
            let no_source = "5 1 0:71 / /y rw - ext4\n";
            assert_eq!(parse_mountinfo(no_source, "0:71"), None, "missing source");
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // From <sys/attr.h>.
    const ATTR_VOL_INFO: u32 = 0x8000_0000;
    const FSOPT_NOFOLLOW: libc::c_ulong = 0x0000_0001;

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let mount = super::mount_root(path);
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
}

#[cfg(windows)]
mod imp {
    use super::*;
    use dedupe_core::traits::PathCodec;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Foundation::MAX_PATH;
    use windows_sys::Win32::Storage::FileSystem::{
        GetVolumeInformationW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };

    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        // `GetVolumePathNameW` docs advise a buffer at least as long as the input path.
        let cap = wide.len().max(MAX_PATH as usize + 1);
        let mut vol_path = vec![0u16; cap];
        let len =
            u32::try_from(cap).map_err(|e| dedupe_core::CoreError::InvalidArg(e.to_string()))?;
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
        let mount_wide = super::match_verbatim(&input, root);
        let mount =
            crate::codec::GenericPathCodec.to_raw(&std::ffi::OsString::from_wide(&mount_wide));

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
            GetVolumeNameForVolumeMountPointW(
                vol_path.as_ptr(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
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
            (serial, crate::volume::fs_type_from_wide(&fs_name))
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

#[cfg(all(test, unix))]
mod mount_root_tests {
    use super::mount_root;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dedupe-mount-root-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn dev(p: &Path) -> u64 {
        p.metadata().expect("metadata").dev()
    }

    #[test]
    fn mount_root_should_return_ancestor_on_same_device_when_path_is_nested() {
        let base = scratch_dir("nested");
        let nested = base.join("a").join("b").join("c");
        fs::create_dir_all(&nested).expect("create nested dirs");

        let root = mount_root(&nested);

        assert!(
            nested.starts_with(&root),
            "{root:?} is not an ancestor of {nested:?}"
        );
        assert_eq!(dev(&root), dev(&nested), "root must share the device");
        if let Some(parent) = root.parent() {
            if let Ok(pm) = parent.metadata() {
                assert_ne!(pm.dev(), dev(&root), "climb must stop at a device change");
            }
        }
        fs::remove_dir_all(&base).expect("cleanup");
    }

    #[test]
    fn mount_root_should_return_root_unchanged_when_path_is_filesystem_root() {
        assert_eq!(mount_root(Path::new("/")), PathBuf::from("/"));
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod identify_tests {
    use super::GenericVolumeIdentity;
    use dedupe_core::traits::VolumeIdentity;
    use std::fs;
    use std::path::PathBuf;

    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("dedupe-identify-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    #[test]
    fn identify_should_return_non_empty_uuid_when_path_is_temp_dir() {
        let dir = scratch_dir("nonempty");
        let info = GenericVolumeIdentity
            .identify(&dir)
            .expect("identify temp dir");
        assert!(!info.uuid.is_empty(), "uuid must not be empty");
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn identify_should_return_same_uuid_and_mount_when_called_twice() {
        let dir = scratch_dir("stable");
        let a = GenericVolumeIdentity
            .identify(&dir)
            .expect("first identify");
        let b = GenericVolumeIdentity
            .identify(&dir)
            .expect("second identify");
        assert_eq!(a.uuid, b.uuid, "uuid must be stable across calls");
        assert_eq!(a.mount, b.mount, "mount must be stable across calls");
        fs::remove_dir_all(&dir).expect("cleanup");
    }
}
