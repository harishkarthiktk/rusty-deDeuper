//! Windows metadata: one handle per regular file, and attribute-bit mapping.

use std::fs::{FileType, Metadata};
use std::path::Path;

use dedupe_core::model::attrs;
use dedupe_core::time::filetime_to_unix_ns;
use dedupe_core::traits::{EntryKind, FileMeta};

use crate::meta::file_meta_from;

/// Metadata for a regular file, read from one Win32 handle on Windows.
///
/// Windows: when `ft` is a regular file and the handle open succeeds, identity and info come
/// from that one open (`file_meta_from_info`). Otherwise the cached `md` is used, with
/// `file_id` `None` and `nlink` 1. Unix: `md` is the whole answer.
pub(super) fn file_meta_at(path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
    if ft.is_file() {
        if let Some(info) = dedupe_win_sys::file_info(path) {
            return file_meta_from_info(info);
        }
    }
    file_meta_from(md, ft)
}

/// Builds a regular-file `FileMeta` from one Win32 info read. Attributes map through the same
/// `win_attr_bits` as `file_meta_from`.
pub fn file_meta_from_info(info: dedupe_win_sys::WinFileInfo) -> FileMeta {
    let mut attr_bits = win_attr_bits(info.attrs);
    if info.nlink > 1 {
        attr_bits |= attrs::HARDLINK;
    }
    FileMeta {
        file_id: Some(info.file_id),
        dev: Some(u64::from(info.volume_serial)),
        nlink: i64::from(info.nlink),
        size: i64::try_from(info.size).unwrap_or(i64::MAX),
        mtime_ns: filetime_to_unix_ns(info.written).unwrap_or(0),
        // ChangeTime is the metadata-change time (matches Unix ctime). Creation time is only a
        // fallback when the query failed.
        ctime_ns: info
            .change_time
            .and_then(filetime_to_unix_ns)
            .or_else(|| filetime_to_unix_ns(info.created)),
        attrs: attr_bits,
        kind: EntryKind::RegularFile,
    }
}

pub(super) fn platform_attr_bits(md: &Metadata) -> i64 {
    use std::os::windows::fs::MetadataExt;
    win_attr_bits(md.file_attributes())
}

/// Windows attribute bits to `attrs` flags. Shared by the std path and the handle path.
fn win_attr_bits(fa: u32) -> i64 {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
    const FILE_ATTRIBUTE_OFFLINE: u32 = 0x0000_1000;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    let mut bits: i64 = 0;
    if fa & FILE_ATTRIBUTE_HIDDEN != 0 {
        bits |= attrs::HIDDEN;
    }
    let placeholder = FILE_ATTRIBUTE_OFFLINE
        | FILE_ATTRIBUTE_RECALL_ON_OPEN
        | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
    if fa & placeholder != 0 {
        bits |= attrs::PLACEHOLDER;
    }
    bits
}
