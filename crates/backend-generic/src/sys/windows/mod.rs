//! Windows implementation of `Platform`.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::path::RawPath;
use dedupe_core::traits::{FileMeta, VolumeInfo};

use super::Platform;
use crate::parse::wtf8;
use crate::probe::FsLocality;

pub(super) mod extents;
pub(super) mod meta;
mod probe;
mod volume;

pub(crate) struct Windows;

impl Platform for Windows {
    fn identify(path: &Path) -> Result<VolumeInfo> {
        volume::identify(path)
    }

    fn is_rotational(path: &Path) -> Result<Option<bool>> {
        // Any failure (network path, access denied, no volume device) is unknown.
        Ok(probe::query_seek_penalty(path))
    }

    fn fs_locality(path: &Path) -> FsLocality {
        probe::locality_windows(path)
    }

    fn identity_bits(_md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
        (None, None, 1, 0)
    }

    fn platform_attr_bits(md: &Metadata) -> i64 {
        meta::platform_attr_bits(md)
    }

    /// `std::fs::Metadata` has no change time on Windows. `None` means unknown, which
    /// `ctime_matches` in core treats as a match, so a fallback read never flips a file to
    /// changed. The handle path (`file_meta_from_info`) reports the real ChangeTime.
    fn ctime_ns(_md: &Metadata) -> Option<i64> {
        None
    }

    fn file_meta_at(path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
        meta::file_meta_at(path, md, ft)
    }

    fn walk_file_meta(entry: &walkdir::DirEntry, ft: FileType) -> io::Result<FileMeta> {
        if ft.is_file() {
            if let Some(info) = dedupe_win_sys::file_info(entry.path()) {
                return Ok(crate::meta::file_meta_from_info(info));
            }
        }
        let md = entry.metadata().map_err(|e| {
            e.into_io_error()
                .unwrap_or_else(|| std::io::Error::other("walk metadata"))
        })?;
        Ok(crate::meta::file_meta_from(&md, ft))
    }

    /// Windows keeps the plain read-only open. Scan and lookup reads on Windows go through
    /// backend-windows `HandleReader`, which checks the NTFS file id of a path open against the
    /// enumerated id; this reader keeps the plain open.
    fn configure_open(_opts: &mut OpenOptions) {}

    fn after_open(_file: &File) -> io::Result<()> {
        Ok(())
    }

    fn read_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        use std::os::windows::fs::FileExt;
        file.seek_read(buf, offset)
    }

    fn first_extent_hint(path: &Path) -> Result<Option<u64>> {
        // Same policy as Linux: the offset only orders reads, so a failed lookup must
        // not abort the scan.
        Ok(crate::extents::first_extent_offset(path).unwrap_or(None))
    }

    fn os_str_to_raw(path: &OsStr) -> RawPath {
        use std::os::windows::ffi::OsStrExt;
        RawPath::new(wtf8::encode(path.encode_wide()))
    }

    fn raw_to_os_string(path: &RawPath) -> OsString {
        use std::os::windows::ffi::OsStringExt;
        let units = match wtf8::decode(&path.0) {
            Ok(units) => units,
            Err(offset) => {
                tracing::warn!(
                    offset,
                    len = path.0.len(),
                    "malformed WTF-8 in stored path; substituting U+FFFD"
                );
                wtf8::decode_lossy(&path.0)
            }
        };
        OsString::from_wide(&units)
    }
}
