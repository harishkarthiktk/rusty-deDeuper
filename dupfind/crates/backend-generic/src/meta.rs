//! Normalize native `Metadata` into `core`'s `FileMeta` (no `cfg` escapes to core).

use std::fs::{FileType, Metadata};
use std::time::{SystemTime, UNIX_EPOCH};

use dupfind_core::model::attrs;
use dupfind_core::traits::{EntryKind, FileMeta};

#[allow(unused_mut, unused_assignments)]
pub fn file_meta_from(md: &Metadata, ft: FileType) -> FileMeta {
    let kind = if ft.is_dir() {
        EntryKind::Directory
    } else if ft.is_symlink() {
        EntryKind::Symlink
    } else if ft.is_file() {
        EntryKind::RegularFile
    } else {
        EntryKind::Other
    };

    let mut file_id: Option<Vec<u8>> = None;
    let mut nlink: i64 = 1;
    let mut attr_bits: i64 = 0;

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        file_id = Some(md.ino().to_le_bytes().to_vec());
        nlink = md.nlink() as i64;
        if md.nlink() > 1 {
            attr_bits |= attrs::HARDLINK;
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        // SF_DATALESS: iCloud/APFS dataless file; reading triggers a download.
        const SF_DATALESS: i64 = 0x4000_0000;
        if md.st_flags() as i64 & SF_DATALESS != 0 {
            attr_bits |= attrs::PLACEHOLDER;
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let fa = md.file_attributes();
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
        const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
        const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
        const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
        if fa & FILE_ATTRIBUTE_HIDDEN != 0 {
            attr_bits |= attrs::HIDDEN;
        }
        if fa
            & (FILE_ATTRIBUTE_OFFLINE
                | FILE_ATTRIBUTE_RECALL_ON_OPEN
                | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS)
            != 0
        {
            attr_bits |= attrs::PLACEHOLDER;
        }
    }

    if matches!(kind, EntryKind::Symlink) {
        attr_bits |= attrs::SYMLINK;
    }

    FileMeta {
        file_id,
        nlink,
        size: md.len() as i64,
        mtime_ns: modified_ns(md),
        ctime_ns: ctime_ns(md),
        attrs: attr_bits,
        kind,
    }
}

fn to_ns(t: Option<SystemTime>) -> Option<i64> {
    t.and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
}

fn modified_ns(md: &Metadata) -> i64 {
    to_ns(md.modified().ok()).unwrap_or(0)
}

#[cfg(unix)]
fn ctime_ns(md: &Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    Some(md.ctime().saturating_mul(1_000_000_000) + md.ctime_nsec())
}

#[cfg(not(unix))]
fn ctime_ns(md: &Metadata) -> Option<i64> {
    to_ns(md.created().ok())
}
