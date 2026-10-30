//! Linux volume identity: filesystem UUID from `/dev/disk/by-uuid`, else fsid.

use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::path::RawPath;
use dedupe_core::traits::VolumeInfo;

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

    let mount = crate::sys::unix::mount_root(path);
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
    crate::parse::mountinfo::parse_mountinfo(&text, &want)
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
    use super::fs_type_from_magic;
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
}
