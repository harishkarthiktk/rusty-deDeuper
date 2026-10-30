//! macOS implementation of `Platform`.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::model::attrs;
use dedupe_core::path::RawPath;
use dedupe_core::traits::{FileMeta, VolumeInfo};

use super::unix;
use super::Platform;
use crate::probe::FsLocality;

mod probe;
mod volume;

pub(crate) struct Macos;

impl Platform for Macos {
    fn identify(path: &Path) -> Result<VolumeInfo> {
        volume::identify(path)
    }

    fn is_rotational(path: &Path) -> Result<Option<bool>> {
        probe::is_rotational(path)
    }

    fn fs_locality(path: &Path) -> FsLocality {
        probe::locality_macos(path)
    }

    fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
        unix::identity_bits(md)
    }

    fn platform_attr_bits(md: &Metadata) -> i64 {
        use std::os::macos::fs::MetadataExt;
        // SF_DATALESS: iCloud/APFS dataless file; reading triggers a download.
        const SF_DATALESS: i64 = 0x4000_0000;
        if md.st_flags() as i64 & SF_DATALESS != 0 {
            attrs::PLACEHOLDER
        } else {
            0
        }
    }

    fn ctime_ns(md: &Metadata) -> Option<i64> {
        unix::ctime_ns(md)
    }

    fn file_meta_at(path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
        unix::file_meta_at(path, md, ft)
    }

    fn walk_file_meta(entry: &walkdir::DirEntry, ft: FileType) -> io::Result<FileMeta> {
        unix::walk_file_meta(entry, ft)
    }

    fn configure_open(opts: &mut OpenOptions) {
        unix::configure_open(opts);
    }

    fn after_open(file: &File) -> io::Result<()> {
        unix::after_open(file)
    }

    fn read_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        unix::read_once(file, offset, buf)
    }

    fn first_extent_hint(path: &Path) -> Result<Option<u64>> {
        let _ = path;
        Ok(None)
    }

    fn os_str_to_raw(path: &OsStr) -> RawPath {
        unix::os_str_to_raw(path)
    }

    fn raw_to_os_string(path: &RawPath) -> OsString {
        unix::raw_to_os_string(path)
    }
}
