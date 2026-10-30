//! Linux storage probe: the block-queue `rotational` flag from sysfs, plus filesystem locality.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use dedupe_core::error::Result;

use crate::parse::sysfs::{
    combine, mount_for, overlay_dir, parent_queue_flag_path, parse_major_minor,
    parse_mountinfo_line, parse_rotational, parse_zpool_devices, queue_flag_path, MountEntry,
};
use crate::probe::FsLocality;

pub(super) fn is_rotational(path: &Path) -> Result<Option<bool>> {
    Ok(rotational(path))
}

pub(super) fn locality_linux(path: &Path) -> FsLocality {
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

/// `rotational` flag of the block device backing `path`, or `None` when unknown.
/// Anonymous devices (major 0: btrfs, ZFS, bcachefs, overlay) have no sysfs node, so
/// they resolve through the backing devices listed in `/proc/self/mountinfo`.
pub(super) fn rotational(path: &Path) -> Option<bool> {
    rotational_inner(path, 0)
}

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
            dev_of_node(Path::new(node)).and_then(|(major, minor)| rotational_of_dev(major, minor))
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
fn dev_of_node(node: &Path) -> Option<(u32, u32)> {
    let st = rustix::fs::stat(node).ok()?;
    if rustix::fs::FileType::from_raw_mode(st.st_mode) != rustix::fs::FileType::BlockDevice {
        return None;
    }
    Some((rustix::fs::major(st.st_rdev), rustix::fs::minor(st.st_rdev)))
}

/// Every device of the btrfs filesystem that contains `source`: the source itself plus the
/// members listed under the matching `/sys/fs/btrfs/<fsid>/devices/` directory.
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
fn sysfs_block_dev(name: &str) -> Option<(u32, u32)> {
    let raw = std::fs::read_to_string(format!("/sys/class/block/{name}/dev")).ok()?;
    parse_major_minor(&raw)
}

/// Device nodes of a ZFS pool from `zpool list -vHPL <pool>`, under the shared timeout helper.
fn zpool_devices(pool: &str) -> Option<Vec<PathBuf>> {
    use std::time::Duration;
    const ZPOOL_TIMEOUT: Duration = Duration::from_secs(5);
    let mut cmd = std::process::Command::new("zpool");
    cmd.args(["list", "-vHPL", pool]);
    let stdout = crate::sys::unix::output_with_timeout(cmd, ZPOOL_TIMEOUT)?;
    Some(parse_zpool_devices(std::str::from_utf8(&stdout).ok()?))
}
