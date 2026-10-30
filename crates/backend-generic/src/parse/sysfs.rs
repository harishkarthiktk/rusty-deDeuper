//! Sysfs and mountinfo parsing. Pure path building and flag parsing live here so they can be
//! unit-tested without touching the real `/sys` tree.

use std::path::{Path, PathBuf};

/// One `/proc/self/mountinfo` line: `id parent maj:min root mount_point opts [optional...] - fstype source super_opts`.
/// Uses the volume module's octal unescape, which exists only on linux.
#[cfg(target_os = "linux")]
pub(crate) fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    use crate::parse::mountinfo::unescape_octal;
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
pub(crate) fn queue_flag_path(major: u32, minor: u32) -> PathBuf {
    PathBuf::from(format!("/sys/dev/block/{major}:{minor}/queue/rotational"))
}

/// `..` is applied after the `/sys/dev/block/<maj>:<min>` symlink is resolved,
/// so it climbs from the partition directory to the whole disk.
pub(crate) fn parent_queue_flag_path(major: u32, minor: u32) -> PathBuf {
    PathBuf::from(format!(
        "/sys/dev/block/{major}:{minor}/../queue/rotational"
    ))
}

/// Sysfs writes `"1\n"` or `"0\n"`. Anything else is treated as unknown.
pub(crate) fn parse_rotational(raw: &str) -> Option<bool> {
    match raw.trim() {
        "1" => Some(true),
        "0" => Some(false),
        _ => None,
    }
}

/// One parsed `/proc/self/mountinfo` entry. Paths and sources are already unescaped.
#[derive(Debug, PartialEq)]
pub(crate) struct MountEntry {
    pub mount_point: PathBuf,
    pub fstype: String,
    pub source: String,
    pub super_opts: String,
}

/// Parses `major:minor` as printed by sysfs `dev` files.
pub(crate) fn parse_major_minor(raw: &str) -> Option<(u32, u32)> {
    let (major, minor) = raw.trim().split_once(':')?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Longest mount point that is a component-wise prefix of `path` (already canonical).
/// Longest prefix, not a `major:minor` match, because nested btrfs subvolumes share the
/// anonymous dev of their parent mount but get no line of their own.
pub(crate) fn mount_for<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    entries
        .iter()
        .filter(|e| path.starts_with(&e.mount_point))
        .max_by_key(|e| e.mount_point.components().count())
}

/// Combines per-device answers: any rotational device gives `Some(true)` (the HDD policy is
/// the safe one for mixed arrays), all non-rotational give `Some(false)`, otherwise unknown.
pub(crate) fn combine(flags: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
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
pub(crate) fn overlay_dir(super_opts: &str) -> Option<PathBuf> {
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
pub(crate) fn parse_zpool_devices(text: &str) -> Vec<PathBuf> {
    text.split_whitespace()
        .filter(|tok| tok.starts_with("/dev/"))
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::parse_mountinfo_line;
    use super::{
        combine, mount_for, overlay_dir, parent_queue_flag_path, parse_major_minor,
        parse_rotational, parse_zpool_devices, queue_flag_path, MountEntry,
    };
    use std::path::Path;
    use std::path::PathBuf;

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
