//! Storage-kind probe. Linux reads the block-queue `rotational` flag from sysfs.
//! macOS asks `diskutil info -plist` for the `SolidState` flag of the device behind
//! the mount. Windows asks the volume's storage device for its seek-penalty flag.
//! Other targets report unknown. An unknown answer makes the core fall back to SSD
//! policy, so a missing probe never fails a scan.

use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::traits::StorageProbe;

/// Reports whether the device holding a path is rotational (HDD) or not (SSD).
#[derive(Clone, Copy, Debug, Default)]
pub struct GenericStorageProbe;

impl StorageProbe for GenericStorageProbe {
    fn is_rotational(&self, path: &Path) -> Result<Option<bool>> {
        #[cfg(target_os = "linux")]
        {
            Ok(sysfs::rotational(path))
        }
        #[cfg(target_os = "macos")]
        {
            // diskutil reports the opposite polarity (SolidState), so the inversion
            // happens once, here, at the trait boundary.
            Ok(macos::solid_state(path).map(|solid_state| !solid_state))
        }
        #[cfg(windows)]
        {
            // Any failure (network path, access denied, no volume device) is unknown.
            Ok(win::query_seek_penalty(path))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            // No probe on this target, so the answer is unknown.
            let _ = path;
            Ok(None)
        }
    }
}

/// Sysfs lookup. Pure path building and flag parsing live here so they can be
/// unit-tested without touching the real `/sys` tree.
#[cfg(any(target_os = "linux", test))]
mod sysfs {
    use std::path::PathBuf;
    // Only the linux probe uses these. The test-only build on other targets must not warn.
    #[cfg(target_os = "linux")]
    use std::{io::ErrorKind, path::Path};

    /// `rotational` flag of the block device backing `path`, or `None` when unknown.
    #[cfg(target_os = "linux")]
    pub(super) fn rotational(path: &Path) -> Option<bool> {
        let st = rustix::fs::stat(path).ok()?;
        let major = rustix::fs::major(st.st_dev);
        let minor = rustix::fs::minor(st.st_dev);

        match std::fs::read_to_string(queue_flag_path(major, minor)) {
            Ok(raw) => parse_rotational(&raw),
            // Partitions have no `queue/` directory of their own; the parent disk does.
            Err(err) if err.kind() == ErrorKind::NotFound => {
                std::fs::read_to_string(parent_queue_flag_path(major, minor))
                    .ok()
                    .and_then(|raw| parse_rotational(&raw))
            }
            Err(_) => None,
        }
    }

    /// `/sys/dev/block/<major>:<minor>/queue/rotational`.
    pub(super) fn queue_flag_path(major: u32, minor: u32) -> PathBuf {
        PathBuf::from(format!("/sys/dev/block/{major}:{minor}/queue/rotational"))
    }

    /// `..` is applied after the `/sys/dev/block/<maj>:<min>` symlink is resolved,
    /// so it climbs from the partition directory to the whole disk.
    pub(super) fn parent_queue_flag_path(major: u32, minor: u32) -> PathBuf {
        PathBuf::from(format!(
            "/sys/dev/block/{major}:{minor}/../queue/rotational"
        ))
    }

    /// Sysfs writes `"1\n"` or `"0\n"`. Anything else is treated as unknown.
    pub(super) fn parse_rotational(raw: &str) -> Option<bool> {
        match raw.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }
}

/// macOS probe: `statfs` finds the device node behind the path, and `diskutil`
/// reports whether that device is solid state.
#[cfg(target_os = "macos")]
mod macos {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::plist::parse_solid_state;

    /// Absolute path so a hostile `PATH` cannot substitute another `diskutil`.
    const DISKUTIL: &str = "/usr/sbin/diskutil";

    /// `SolidState` flag of the device backing `path`, or `None` when unknown.
    pub(super) fn solid_state(path: &Path) -> Option<bool> {
        let device = mounted_device(path)?;
        let output = Command::new(DISKUTIL)
            .args(["info", "-plist", device.as_str()])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let xml = std::str::from_utf8(&output.stdout).ok()?;
        parse_solid_state(xml)
    }

    /// Device node the filesystem holding `path` is mounted from, e.g. `/dev/disk3s1s1`.
    /// Network mounts report `//user@host/share`, so only `/dev/` sources are accepted.
    fn mounted_device(path: &Path) -> Option<String> {
        let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
        // SAFETY: `c_path` is NUL-terminated and outlives the call. `libc::statfs` writes
        // one struct into `st`, and the all-zero bit pattern is a valid value for this
        // plain C struct. `st` is only returned when the call reports success.
        let st = unsafe {
            let mut st: libc::statfs = std::mem::zeroed();
            (libc::statfs(c_path.as_ptr(), &mut st) == 0).then_some(st)
        }?;
        let name: Vec<u8> = st
            .f_mntfromname
            .iter()
            .take_while(|&&c| c != 0)
            .map(|&c| c as u8)
            .collect();
        let device = String::from_utf8(name).ok()?;
        device.starts_with("/dev/").then_some(device)
    }
}

/// Windows probe. The storage stack reports whether a device incurs a seek penalty,
/// the closest storage-level proxy for rotational media (HDDs do, SSDs do not).
#[cfg(windows)]
mod win {
    use std::mem::size_of;
    use std::path::Path;
    use std::ptr::{from_ref, null_mut};

    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
        IOCTL_STORAGE_QUERY_PROPERTY, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::seek_penalty::{interpret_seek_penalty, DESCRIPTOR_LEN};
    use super::volume_device_path;
    use crate::win_sys::{open_handle, to_wide_nul, volume_root};

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
}

/// Pure parsing of the seek-penalty reply. Compiled on every OS so it is unit-tested everywhere.
#[cfg(any(windows, test))]
mod seek_penalty {
    /// `sizeof(DEVICE_SEEK_PENALTY_DESCRIPTOR)`: `Version` and `Size` (u32 each),
    /// `IncursSeekPenalty` (u8), then 3 bytes of padding.
    pub(super) const DESCRIPTOR_LEN: usize = 12;
    const INCURS_SEEK_PENALTY_OFFSET: usize = 8;

    /// `Some(true)` when the device incurs a seek penalty (rotational), `Some(false)` when
    /// it does not. `None` when the kernel reported fewer bytes than the descriptor, or the
    /// buffer cannot hold them, so a truncated reply is never read as a valid answer.
    pub(super) fn interpret_seek_penalty(buf: &[u8], returned: usize) -> Option<bool> {
        if returned < DESCRIPTOR_LEN {
            return None;
        }
        let descriptor = buf.get(..DESCRIPTOR_LEN)?;
        let incurs = descriptor.get(INCURS_SEEK_PENALTY_OFFSET)?;
        Some(*incurs != 0)
    }
}

/// Device path that opens the whole volume, from a volume root as `GetVolumePathNameW`
/// reports it. `X:\` maps to `\\.\X:` and `\\?\Volume{GUID}\` maps to `\\?\Volume{GUID}`.
/// Anything else (UNC shares, folder mounts) is `None`, because opening it would address
/// a directory on a volume rather than the volume itself.
#[cfg(any(windows, test))]
fn volume_device_path(root: &str) -> Option<String> {
    const VOLUME_GUID_PREFIX: &str = r"\\?\Volume{";
    match root.as_bytes() {
        [letter, b':', b'\\'] if letter.is_ascii_alphabetic() => {
            Some(format!(r"\\.\{}:", char::from(*letter)))
        }
        _ if root.starts_with(VOLUME_GUID_PREFIX) && root.ends_with('\\') => {
            Some(root.trim_end_matches('\\').to_owned())
        }
        _ => None,
    }
}

/// Parser for `diskutil info -plist` output. Pure string handling, so it compiles and
/// is unit-tested on every target, while the subprocess and `statfs` stay macOS-only.
#[cfg(any(target_os = "macos", test))]
mod plist {
    /// `SolidState` of a `diskutil info -plist` document: `Some(true)` for solid state,
    /// `Some(false)` for rotational media. `None` when the key is missing, is not a
    /// boolean, or the device is a disk image (virtual media says nothing about hardware).
    pub(super) fn parse_solid_state(plist_xml: &str) -> Option<bool> {
        if plist_string(plist_xml, "BusProtocol") == Some("Disk Image") {
            return None;
        }
        let value = value_after_key(plist_xml, "SolidState")?.trim_start();
        if value.starts_with("<true/>") {
            Some(true)
        } else if value.starts_with("<false/>") {
            Some(false)
        } else {
            None
        }
    }

    /// Text that follows `<key>{key}</key>`, or `None` when the key is absent.
    fn value_after_key<'a>(plist_xml: &'a str, key: &str) -> Option<&'a str> {
        let (_, rest) = plist_xml.split_once(format!("<key>{key}</key>").as_str())?;
        Some(rest)
    }

    /// Contents of `<key>{key}</key><string>VALUE</string>`, or `None` when the value is not a string.
    fn plist_string<'a>(plist_xml: &'a str, key: &str) -> Option<&'a str> {
        let rest = value_after_key(plist_xml, key)?.trim_start();
        let (value, _) = rest.strip_prefix("<string>")?.split_once("</string>")?;
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::plist::parse_solid_state;
    use super::seek_penalty::interpret_seek_penalty;
    use super::sysfs::{parent_queue_flag_path, parse_rotational, queue_flag_path};
    use super::volume_device_path;
    use std::path::PathBuf;

    /// A `DEVICE_SEEK_PENALTY_DESCRIPTOR` reply: Version 1, Size 12, then the flag byte.
    fn seek_penalty_bytes(incurs_seek_penalty: u8) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf[4..8].copy_from_slice(&12u32.to_le_bytes());
        buf[8] = incurs_seek_penalty;
        buf
    }

    #[test]
    fn parse_rotational_should_return_true_for_one_with_newline() {
        let parsed = parse_rotational("1\n");
        assert_eq!(
            parsed,
            Some(true),
            "sysfs content \"1\\n\" should parse to Some(true), got {parsed:?}"
        );
    }

    #[test]
    fn parse_rotational_should_return_false_for_zero() {
        let parsed = parse_rotational("0\n");
        assert_eq!(
            parsed,
            Some(false),
            "sysfs content \"0\\n\" should parse to Some(false), got {parsed:?}"
        );
    }

    #[test]
    fn parse_rotational_should_return_none_for_garbage() {
        let parsed = parse_rotational("maybe");
        assert_eq!(
            parsed, None,
            "unparseable sysfs content should give None, got {parsed:?}"
        );
    }

    #[test]
    fn queue_flag_path_should_format_major_minor() {
        let path = queue_flag_path(8, 0);
        assert_eq!(
            path,
            PathBuf::from("/sys/dev/block/8:0/queue/rotational"),
            "major 8 minor 0 should map to the block queue flag, got {path:?}"
        );
    }

    #[test]
    fn parent_queue_flag_path_should_climb_one_level() {
        let path = parent_queue_flag_path(8, 1);
        assert_eq!(
            path,
            PathBuf::from("/sys/dev/block/8:1/../queue/rotational"),
            "partition 8:1 should climb to the parent disk's queue flag, got {path:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_some_true_for_true_value() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>PCI-Express</string>
    <key>SolidState</key>
    <true/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed,
            Some(true),
            "SolidState <true/> should parse to Some(true), got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_some_false_for_false_value() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>USB</string>
    <key>SolidState</key>
    <false/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed,
            Some(false),
            "SolidState <false/> should parse to Some(false), got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_none_when_key_missing() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>PCI-Express</string>
    <key>Internal</key>
    <true/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed, None,
            "a document without SolidState should give None, got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_none_when_value_is_not_boolean() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>SolidState</key>
    <string>yes</string>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed, None,
            "a non-boolean SolidState value should give None, got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_tolerate_whitespace_between_key_and_value() {
        let xml = "<dict>\n\t<key>SolidState</key>\n\t\n\t\t<false/>\n</dict>";
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed,
            Some(false),
            "whitespace between SolidState key and value should be skipped, got {parsed:?}"
        );
    }

    #[test]
    fn parse_solid_state_should_return_none_for_disk_image_bus_protocol() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>BusProtocol</key>
    <string>Disk Image</string>
    <key>SolidState</key>
    <true/>
</dict>
</plist>
"#;
        let parsed = parse_solid_state(xml);
        assert_eq!(
            parsed, None,
            "a Disk Image bus protocol should give None even with SolidState set, got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_some_true_when_flag_set() {
        let buf = seek_penalty_bytes(1);
        let parsed = interpret_seek_penalty(&buf, buf.len());
        assert_eq!(
            parsed,
            Some(true),
            "IncursSeekPenalty 1 should parse to Some(true), got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_some_false_when_flag_clear() {
        let buf = seek_penalty_bytes(0);
        let parsed = interpret_seek_penalty(&buf, buf.len());
        assert_eq!(
            parsed,
            Some(false),
            "IncursSeekPenalty 0 should parse to Some(false), got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_none_when_buffer_too_short() {
        let buf = [0u8; 4];
        let parsed = interpret_seek_penalty(&buf, buf.len());
        assert_eq!(
            parsed, None,
            "a 4-byte buffer cannot hold the descriptor and should give None, got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_none_when_returned_below_descriptor_length() {
        let buf = seek_penalty_bytes(1);
        let parsed = interpret_seek_penalty(&buf, 8);
        assert_eq!(
            parsed, None,
            "returned 8 bytes is a short reply and should give None even with the flag set, got {parsed:?}"
        );
    }

    #[test]
    fn volume_device_path_should_map_drive_root_to_device_when_drive_letter() {
        let device = volume_device_path("C:\\");
        assert_eq!(
            device.as_deref(),
            Some(r"\\.\C:"),
            "drive root C:\\ should map to the volume device \\\\.\\C:, got {device:?}"
        );
    }

    #[test]
    fn volume_device_path_should_strip_trailing_backslash_when_volume_guid() {
        let device = volume_device_path(r"\\?\Volume{1234-abcd}\");
        assert_eq!(
            device.as_deref(),
            Some(r"\\?\Volume{1234-abcd}"),
            "a volume GUID root should open without its trailing backslash, got {device:?}"
        );
    }

    #[test]
    fn volume_device_path_should_return_none_when_unc_share() {
        let device = volume_device_path(r"\\server\share\");
        assert_eq!(
            device, None,
            "a network share has no local volume device and should give None, got {device:?}"
        );
    }

    #[test]
    fn volume_device_path_should_return_none_when_folder_mount() {
        let device = volume_device_path(r"C:\mnt\data\");
        assert_eq!(
            device, None,
            "a folder mount addresses a directory, not the volume, and should give None, got {device:?}"
        );
    }
}
