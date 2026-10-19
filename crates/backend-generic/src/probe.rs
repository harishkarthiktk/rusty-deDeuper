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

/// Remote-ness of the filesystem holding a path. SQLite WAL needs shared memory and
/// working file locks, which network filesystems do not provide reliably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsLocality {
    Local,
    /// NFS, SMB/CIFS, FUSE, Ceph, or a Windows network drive.
    Remote,
    /// WSL's 9p/drvfs mounts: local disk behind a network-style protocol. Usually works.
    Virtualized,
    Unknown,
}

/// Classifies the filesystem holding `path` (an existing file or directory). Any failure
/// is `Unknown`, so a missing probe never blocks a scan.
pub fn fs_locality(path: &Path) -> FsLocality {
    #[cfg(target_os = "linux")]
    {
        locality_linux(path)
    }
    #[cfg(target_os = "macos")]
    {
        locality_macos(path)
    }
    #[cfg(windows)]
    {
        locality_windows(path)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = path;
        FsLocality::Unknown
    }
}

#[cfg(target_os = "linux")]
fn locality_linux(path: &Path) -> FsLocality {
    // Magic numbers from <linux/magic.h>. `f_type` is signed on some targets, so mask it.
    const NFS: u64 = 0x6969;
    const SMB: u64 = 0x517B;
    const SMB2: u64 = 0xFE53_4D42;
    const CIFS: u64 = 0xFF53_4D42;
    const FUSE: u64 = 0x6573_5546;
    const CEPH: u64 = 0x00C3_6400;
    const V9FS: u64 = 0x0102_1997;
    let Ok(st) = rustix::fs::statfs(path) else {
        return FsLocality::Unknown;
    };
    match (st.f_type as u64) & 0xFFFF_FFFF {
        NFS | SMB | SMB2 | CIFS | FUSE | CEPH => FsLocality::Remote,
        V9FS => FsLocality::Virtualized,
        _ => FsLocality::Local,
    }
}

#[cfg(target_os = "macos")]
fn locality_macos(path: &Path) -> FsLocality {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return FsLocality::Unknown;
    };
    // SAFETY: `libc::statfs` is plain old data; all-zero is a valid value.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is NUL-terminated and outlives the call; `st` is writable.
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return FsLocality::Unknown;
    }
    if st.f_flags & (libc::MNT_LOCAL as u32) == 0 {
        FsLocality::Remote
    } else {
        FsLocality::Local
    }
}

#[cfg(windows)]
fn locality_windows(path: &Path) -> FsLocality {
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

/// Sysfs lookup. Pure path building and flag parsing live here so they can be
/// unit-tested without touching the real `/sys` tree.
#[cfg(any(target_os = "linux", test))]
mod sysfs {
    use std::path::{Path, PathBuf};
    // Only the linux probe uses this. The test-only build on other targets must not warn.
    #[cfg(target_os = "linux")]
    use std::io::ErrorKind;

    /// `rotational` flag of the block device backing `path`, or `None` when unknown.
    /// Anonymous devices (major 0: btrfs, ZFS, bcachefs, overlay) have no sysfs node, so
    /// they resolve through the backing devices listed in `/proc/self/mountinfo`.
    #[cfg(target_os = "linux")]
    pub(super) fn rotational(path: &Path) -> Option<bool> {
        rotational_inner(path, 0)
    }

    #[cfg(target_os = "linux")]
    fn rotational_inner(path: &Path, depth: u8) -> Option<bool> {
        let st = rustix::fs::stat(path).ok()?;
        let major = rustix::fs::major(st.st_dev);
        let minor = rustix::fs::minor(st.st_dev);
        if major != 0 {
            return rotational_of_dev(major, minor);
        }
        // Overlay recursion goes one level deep; anything deeper is unknown.
        if depth > 1 {
            return None;
        }
        let canon = std::fs::canonicalize(path).ok()?;
        let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        let entries: Vec<MountEntry> = info.lines().filter_map(parse_mountinfo_line).collect();
        let mount = mount_for(&entries, &canon)?;
        match mount.fstype.as_str() {
            "btrfs" => combine(
                btrfs_devices(&mount.source)?
                    .into_iter()
                    .map(|(major, minor)| rotational_of_dev(major, minor)),
            ),
            "bcachefs" => combine(mount.source.split(':').map(|node| {
                dev_of_node(Path::new(node))
                    .and_then(|(major, minor)| rotational_of_dev(major, minor))
            })),
            "zfs" => combine(
                zpool_devices(mount.source.split('/').next()?)?
                    .iter()
                    .map(|node| {
                        dev_of_node(node).and_then(|(major, minor)| rotational_of_dev(major, minor))
                    }),
            ),
            "overlay" => {
                overlay_dir(&mount.super_opts).and_then(|dir| rotational_inner(&dir, depth + 1))
            }
            _ => None,
        }
    }

    /// Queue flag of the block device `major:minor`, with the parent disk as fallback for partitions.
    #[cfg(target_os = "linux")]
    fn rotational_of_dev(major: u32, minor: u32) -> Option<bool> {
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

    /// `(major, minor)` of the block device node at `node`, or `None` if it is not a block device.
    #[cfg(target_os = "linux")]
    fn dev_of_node(node: &Path) -> Option<(u32, u32)> {
        let st = rustix::fs::stat(node).ok()?;
        if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::BlockDevice {
            return None;
        }
        Some((rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev)))
    }

    /// Every device of the btrfs filesystem that contains `source`: the source itself plus the
    /// members listed under the matching `/sys/fs/btrfs/<fsid>/devices/` directory.
    #[cfg(target_os = "linux")]
    fn btrfs_devices(source: &str) -> Option<Vec<(u32, u32)>> {
        let source_path = Path::new(source);
        let mut devs = vec![dev_of_node(source_path)?];
        let name = source_path.file_name()?.to_str()?;
        for fsid in std::fs::read_dir("/sys/fs/btrfs")
            .into_iter()
            .flatten()
            .flatten()
        {
            let Ok(members) = std::fs::read_dir(fsid.path().join("devices")) else {
                continue;
            };
            let names: Vec<String> = members
                .flatten()
                .map(|m| m.file_name().to_string_lossy().into_owned())
                .collect();
            if names.iter().any(|n| n == name) {
                for member in names.iter().filter_map(|n| sysfs_block_dev(n)) {
                    if !devs.contains(&member) {
                        devs.push(member);
                    }
                }
                break;
            }
        }
        Some(devs)
    }

    /// `major:minor` of the named block device, read from `/sys/class/block/<name>/dev`.
    #[cfg(target_os = "linux")]
    fn sysfs_block_dev(name: &str) -> Option<(u32, u32)> {
        let raw = std::fs::read_to_string(format!("/sys/class/block/{name}/dev")).ok()?;
        parse_major_minor(&raw)
    }

    /// Device nodes of a ZFS pool from `zpool list -vHPL <pool>`, under the shared timeout helper.
    #[cfg(target_os = "linux")]
    fn zpool_devices(pool: &str) -> Option<Vec<PathBuf>> {
        use std::time::Duration;
        const ZPOOL_TIMEOUT: Duration = Duration::from_secs(5);
        let mut cmd = std::process::Command::new("zpool");
        cmd.args(["list", "-vHPL", pool]);
        let stdout = super::output_with_timeout(cmd, ZPOOL_TIMEOUT)?;
        Some(parse_zpool_devices(std::str::from_utf8(&stdout).ok()?))
    }

    /// One `/proc/self/mountinfo` line: `id parent maj:min root mount_point opts [optional...] - fstype source super_opts`.
    /// Uses the volume module's octal unescape, which exists only on linux.
    #[cfg(target_os = "linux")]
    pub(super) fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
        use crate::volume::imp::unescape_octal;
        let (left, right) = line.split_once(" - ")?;
        let mount_point = left.split(' ').nth(4)?;
        let mut fields = right.split(' ');
        Some(MountEntry {
            mount_point: PathBuf::from(unescape_octal(mount_point)),
            fstype: fields.next()?.to_owned(),
            source: unescape_octal(fields.next()?),
            super_opts: fields.next().unwrap_or("").to_owned(),
        })
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

    /// One parsed `/proc/self/mountinfo` entry. Paths and sources are already unescaped.
    #[derive(Debug, PartialEq)]
    pub(super) struct MountEntry {
        pub mount_point: PathBuf,
        pub fstype: String,
        pub source: String,
        pub super_opts: String,
    }

    /// Parses `major:minor` as printed by sysfs `dev` files.
    pub(super) fn parse_major_minor(raw: &str) -> Option<(u32, u32)> {
        let (major, minor) = raw.trim().split_once(':')?;
        Some((major.parse().ok()?, minor.parse().ok()?))
    }

    /// Longest mount point that is a component-wise prefix of `path` (already canonical).
    /// Longest prefix, not a `major:minor` match, because nested btrfs subvolumes share the
    /// anonymous dev of their parent mount but get no line of their own.
    pub(super) fn mount_for<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
        entries
            .iter()
            .filter(|e| path.starts_with(&e.mount_point))
            .max_by_key(|e| e.mount_point.components().count())
    }

    /// Combines per-device answers: any rotational device gives `Some(true)` (the HDD policy is
    /// the safe one for mixed arrays), all non-rotational give `Some(false)`, otherwise unknown.
    pub(super) fn combine(flags: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
        let mut seen = false;
        let mut all_known = true;
        for flag in flags {
            seen = true;
            match flag {
                Some(true) => return Some(true),
                Some(false) => {}
                None => all_known = false,
            }
        }
        (seen && all_known).then_some(false)
    }

    /// Overlay directory to probe: `upperdir=` when present, else the first `lowerdir=` entry.
    pub(super) fn overlay_dir(super_opts: &str) -> Option<PathBuf> {
        fn opt_value<'a>(opts: &'a str, key: &str) -> Option<&'a str> {
            opts.split(',').find_map(|opt| opt.strip_prefix(key))
        }
        if let Some(upper) = opt_value(super_opts, "upperdir=") {
            return Some(PathBuf::from(upper));
        }
        let lower = opt_value(super_opts, "lowerdir=")?.split(':').next()?;
        (!lower.is_empty()).then(|| PathBuf::from(lower))
    }

    /// Device nodes from `zpool list -vHPL` output: the `/dev/...` tokens, one per vdev leaf.
    pub(super) fn parse_zpool_devices(text: &str) -> Vec<PathBuf> {
        text.split_whitespace()
            .filter(|tok| tok.starts_with("/dev/"))
            .map(PathBuf::from)
            .collect()
    }
}

/// Runs `cmd` and returns its stdout on a zero exit status, or `None` on spawn
/// failure, non-zero exit, or when `timeout` elapses (the child is then killed and reaped).
/// stdin and stderr are discarded. stdout is drained on a thread so a full pipe cannot
/// deadlock. On timeout that thread is detached rather than joined, in case a grandchild
/// still holds the pipe open.
// Unused only on unix targets other than macOS and Linux (both callers are cfg-gated to those).
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
#[cfg(unix)]
pub(crate) fn output_with_timeout(
    mut cmd: std::process::Command,
    timeout: std::time::Duration,
) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        out.read_to_end(&mut buf).map(|_| buf)
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return reader.join().ok()?.ok(),
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                // Timed out or try_wait failed: kill and reap; reader thread detached.
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
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
    use std::process::Command;
    use std::time::Duration;

    use super::output_with_timeout;
    use super::plist::parse_solid_state;

    /// Absolute path so a hostile `PATH` cannot substitute another `diskutil`.
    const DISKUTIL: &str = "/usr/sbin/diskutil";

    /// Upper bound on the `diskutil` call so a wedged disk cannot hang the scan.
    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

    /// `SolidState` flag of the device backing `path`, or `None` when unknown.
    pub(super) fn solid_state(path: &Path) -> Option<bool> {
        let device = mounted_device(path)?;
        let mut cmd = Command::new(DISKUTIL);
        cmd.args(["info", "-plist", device.as_str()]);
        let stdout = output_with_timeout(cmd, PROBE_TIMEOUT)?;
        let xml = std::str::from_utf8(&stdout).ok()?;
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
    #[cfg(target_os = "linux")]
    use super::sysfs::parse_mountinfo_line;
    use super::sysfs::{
        combine, mount_for, overlay_dir, parent_queue_flag_path, parse_major_minor,
        parse_rotational, parse_zpool_devices, queue_flag_path, MountEntry,
    };
    use super::volume_device_path;
    use std::path::Path;
    use std::path::PathBuf;

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_should_return_stdout_for_fast_command() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo hi"]);
        let out = super::output_with_timeout(cmd, std::time::Duration::from_secs(5));
        assert_eq!(
            out,
            Some(b"hi\n".to_vec()),
            "a fast successful command should return its stdout, got {out:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn output_with_timeout_should_return_none_when_deadline_passes() {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 10"]);
        let started = std::time::Instant::now();
        let out = super::output_with_timeout(cmd, std::time::Duration::from_millis(100));
        let elapsed = started.elapsed();
        assert_eq!(
            out, None,
            "a command past its deadline should give None, got {out:?}"
        );
        // The spec expects about 1 s; the bound is loose so a loaded CI host does not flake.
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "timeout should return well before the 10 s sleep ends, took {elapsed:?}"
        );
    }

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

    fn mount(mount_point: &str, fstype: &str) -> MountEntry {
        MountEntry {
            mount_point: PathBuf::from(mount_point),
            fstype: fstype.to_owned(),
            source: String::new(),
            super_opts: String::new(),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_return_btrfs_subvolume_entry() {
        let line = "36 35 0:32 /@home /home rw,relatime shared:1 - btrfs /dev/sda2 rw,subvolid=257,subvol=/@home";
        assert_eq!(
            parse_mountinfo_line(line),
            Some(MountEntry {
                mount_point: PathBuf::from("/home"),
                fstype: "btrfs".to_owned(),
                source: "/dev/sda2".to_owned(),
                super_opts: "rw,subvolid=257,subvol=/@home".to_owned(),
            }),
            "btrfs subvolume line should keep the device and the subvol option"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_return_zfs_dataset_entry() {
        let line = "40 30 0:45 / /tank/data rw,relatime shared:20 - zfs tank/data rw,xattr,noacl";
        assert_eq!(
            parse_mountinfo_line(line),
            Some(MountEntry {
                mount_point: PathBuf::from("/tank/data"),
                fstype: "zfs".to_owned(),
                source: "tank/data".to_owned(),
                super_opts: "rw,xattr,noacl".to_owned(),
            }),
            "ZFS dataset line should split the pool/dataset source"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_return_overlay_upperdir_entry() {
        let line = "100 30 0:60 / / rw,relatime shared:50 - overlay overlay rw,lowerdir=/var/lib/docker/overlay2/l/ABC:/var/lib/docker/overlay2/l/DEF,upperdir=/var/lib/docker/overlay2/xyz/diff,workdir=/var/lib/docker/overlay2/xyz/work";
        let entry = parse_mountinfo_line(line);
        assert_eq!(
            entry.as_ref().map(|e| e.fstype.as_str()),
            Some("overlay"),
            "overlay line should report fstype overlay, got {entry:?}"
        );
        assert_eq!(
            entry.as_ref().and_then(|e| overlay_dir(&e.super_opts)),
            Some(PathBuf::from("/var/lib/docker/overlay2/xyz/diff")),
            "overlay super options should yield the upperdir, got {entry:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_unescape_space_in_mount_point() {
        let line = "55 30 8:3 / /mnt/my\\040disk rw,relatime shared:9 - ext4 /dev/sdb1 rw";
        assert_eq!(
            parse_mountinfo_line(line).map(|e| e.mount_point),
            Some(PathBuf::from("/mnt/my disk")),
            "\\040 in the mount point should decode to a space"
        );
    }

    #[test]
    fn mount_for_should_pick_longest_component_prefix() {
        let entries = vec![mount("/", "ext4"), mount("/home", "btrfs")];
        let fstype = |p: &str| mount_for(&entries, Path::new(p)).map(|e| e.fstype.clone());
        assert_eq!(
            fstype("/home/user/a"),
            Some("btrfs".to_owned()),
            "the deeper /home mount should win over /"
        );
        assert_eq!(
            fstype("/homework/a"),
            Some("ext4".to_owned()),
            "/homework must not match /home, the check is component-wise"
        );
    }

    #[test]
    fn mount_for_should_return_none_when_no_mount_contains_path() {
        let entries = vec![mount("/home", "btrfs")];
        assert!(
            mount_for(&entries, Path::new("/srv/data")).is_none(),
            "a path outside every mount point should give None"
        );
    }

    #[test]
    fn combine_should_follow_truth_table() {
        assert_eq!(
            combine(Vec::<Option<bool>>::new()),
            None,
            "no devices is unknown"
        );
        assert_eq!(
            combine([Some(false), Some(false)]),
            Some(false),
            "all non-rotational devices give Some(false)"
        );
        assert_eq!(
            combine([Some(false), Some(true)]),
            Some(true),
            "one rotational device makes the whole set rotational"
        );
        assert_eq!(
            combine([None, Some(true)]),
            Some(true),
            "a rotational device wins even when another is unknown"
        );
        assert_eq!(
            combine([Some(false), None]),
            None,
            "an unknown device with no rotational one leaves the set unknown"
        );
        assert_eq!(combine([None]), None, "a single unknown device is unknown");
    }

    #[test]
    fn overlay_dir_should_prefer_upperdir_then_first_lowerdir() {
        assert_eq!(
            overlay_dir("rw,lowerdir=/l1:/l2,upperdir=/u,workdir=/w"),
            Some(PathBuf::from("/u")),
            "upperdir should win over lowerdir"
        );
        assert_eq!(
            overlay_dir("ro,lowerdir=/l1:/l2"),
            Some(PathBuf::from("/l1")),
            "without upperdir the first lowerdir entry should be used"
        );
        assert_eq!(
            overlay_dir("rw,relatime"),
            None,
            "no dir option should give None"
        );
    }

    #[test]
    fn parse_zpool_devices_should_keep_only_dev_tokens() {
        let text =
            "tank\t1.81T\t1.20T\n\tmirror-0\t1.81T\n\t\t/dev/sda\t1.81T\n\t\t/dev/sdb\t1.81T\n";
        assert_eq!(
            parse_zpool_devices(text),
            vec![PathBuf::from("/dev/sda"), PathBuf::from("/dev/sdb")],
            "only the /dev/ leaf tokens should be kept"
        );
    }

    #[test]
    fn parse_major_minor_should_read_sysfs_dev_file() {
        assert_eq!(
            parse_major_minor("8:16\n"),
            Some((8, 16)),
            "sysfs dev file should parse"
        );
        assert_eq!(
            parse_major_minor("garbage"),
            None,
            "non-numeric content should give None"
        );
    }

    #[test]
    fn mount_for_should_return_none_when_entries_empty() {
        assert!(
            mount_for(&[], Path::new("/home/user")).is_none(),
            "an empty mount table should give None for any path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn fs_locality_should_return_unknown_when_path_missing() {
        let missing = Path::new("/definitely/not/a/real/dedupe/path");
        let locality = super::fs_locality(missing);
        assert_eq!(
            locality,
            super::FsLocality::Unknown,
            "a path that cannot be stat-ed should be Unknown, got {locality:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn fs_locality_should_not_report_remote_when_path_is_temp_dir() {
        let locality = super::fs_locality(&std::env::temp_dir());
        assert_ne!(
            locality,
            super::FsLocality::Remote,
            "the temp dir is normally local disk or tmpfs, got {locality:?}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_return_none_when_line_empty() {
        assert!(
            parse_mountinfo_line("").is_none(),
            "an empty mountinfo line should give None"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_return_none_when_lines_malformed() {
        let lines = [
            "garbage",
            "36 35 98:0 /mnt/a rw,noatime shared:1 - ext4",
            "36 35 98:0 /mnt/b",
            "36 35 98:0 - ext4 /dev/sda1 rw",
        ];
        for line in lines {
            let parsed = parse_mountinfo_line(line);
            assert!(
                parsed.is_none(),
                "malformed line {line:?} should be skipped, got {parsed:?}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_mountinfo_line_should_keep_valid_entry_when_malformed_lines_surround_it() {
        let lines = [
            "garbage",
            "36 35 98:0 / /mnt/ok rw - ext4 /dev/sdb rw",
            "36 35 98:0 /mnt/bad",
        ];
        let parsed: Vec<MountEntry> = lines
            .iter()
            .filter_map(|line| parse_mountinfo_line(line))
            .collect();
        assert_eq!(
            parsed.len(),
            1,
            "only the well-formed line should survive, got {parsed:?}"
        );
        assert_eq!(
            parsed[0].mount_point,
            PathBuf::from("/mnt/ok"),
            "the surviving entry should keep its mount point"
        );
    }

    #[test]
    fn parse_zpool_devices_should_collect_leaves_when_mirrors_and_raidz_present() {
        let text = [
            "tank 7.27T 3.10T 4.17T -",
            "\tmirror-0 3.63T 1.50T 2.13T -",
            "\t\t/dev/sda 3.63T - - -",
            "\t\t/dev/sdb 3.63T - - -",
            "\traidz1-1 3.63T 1.60T 2.03T -",
            "\t\t/dev/nvme0n1p3 1.00T - - -",
            "\t\t/dev/nvme1n1p3 1.00T - - -",
            "\t\t/dev/nvme2n1p3 1.00T - - -",
            "\tlogs",
            "\t\t/dev/sdc 1G - - -",
        ]
        .join("\n");
        assert_eq!(
            parse_zpool_devices(&text),
            vec![
                PathBuf::from("/dev/sda"),
                PathBuf::from("/dev/sdb"),
                PathBuf::from("/dev/nvme0n1p3"),
                PathBuf::from("/dev/nvme1n1p3"),
                PathBuf::from("/dev/nvme2n1p3"),
                PathBuf::from("/dev/sdc"),
            ],
            "every /dev leaf across mirrors, raidz and logs should be kept in order"
        );
    }

    #[test]
    fn parse_zpool_devices_should_return_empty_when_vdevs_are_file_backed() {
        let text = "tank 1G 0 1G -\n\t/var/tmp/vdev0 1G 0 1G -\n";
        assert!(
            parse_zpool_devices(text).is_empty(),
            "file-backed vdevs have no /dev node and should give no devices"
        );
        assert!(
            parse_zpool_devices("").is_empty(),
            "empty zpool output should give no devices"
        );
    }
}
