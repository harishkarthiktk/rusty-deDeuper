//! Normalize native `Metadata` into `core`'s `FileMeta` (no `cfg` escapes to core).

use std::fs::{FileType, Metadata};
use std::path::Path;

use dedupe_core::model::attrs;
use dedupe_core::time::system_time_to_unix_ns;
use dedupe_core::traits::{EntryKind, FileMeta};

use crate::sys::{Os, Platform};

// Items that exist on some targets only (`file_meta_from_info` on Windows).
#[allow(
    unused_imports,
    reason = "the glob is empty on targets that have no such item"
)]
pub use crate::sys::exports::meta::*;

/// Metadata for a regular file, read from one Win32 handle on Windows.
///
/// Windows: when `ft` is a regular file and the handle open succeeds, identity and info come
/// from that one open (`file_meta_from_info`). Otherwise the cached `md` is used, with
/// `file_id` `None` and `nlink` 1. Unix: `md` is the whole answer.
pub fn file_meta_at(path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
    Os::file_meta_at(path, md, ft)
}

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

    let (file_id, dev, nlink, hardlink_bits) = Os::identity_bits(md);
    let mut attr_bits = hardlink_bits | Os::platform_attr_bits(md);
    if matches!(kind, EntryKind::Symlink) {
        attr_bits |= attrs::SYMLINK;
    }

    FileMeta {
        file_id,
        dev,
        nlink,
        size: md.len() as i64,
        mtime_ns: modified_ns(md),
        ctime_ns: ctime_ns(md),
        attrs: attr_bits,
        kind,
    }
}

/// Signed Unix ns (pre-1970 is negative); an out-of-range or unavailable time is `0`, the
/// convention for a missing mtime.
fn modified_ns(md: &Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(system_time_to_unix_ns)
        .unwrap_or(0)
}

fn ctime_ns(md: &Metadata) -> Option<i64> {
    Os::ctime_ns(md)
}
