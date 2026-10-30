//! Linux implementation of `Platform`.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::path::RawPath;
use dedupe_core::traits::{FileMeta, VolumeInfo};

use super::unix;
use super::Platform;
use crate::probe::FsLocality;

pub(super) mod extents;
mod probe;
mod volume;

pub(crate) struct Linux;

impl Platform for Linux {
    fn identify(path: &Path) -> Result<VolumeInfo> {
        volume::identify(path)
    }

    fn is_rotational(path: &Path) -> Result<Option<bool>> {
        probe::is_rotational(path)
    }

    fn fs_locality(path: &Path) -> FsLocality {
        probe::locality_linux(path)
    }

    fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
        unix::identity_bits(md)
    }

    fn platform_attr_bits(_md: &Metadata) -> i64 {
        0
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
        // The offset only orders reads. A file that vanished since enumeration
        // must not abort the scan: the hash pass reports it as an io error.
        Ok(crate::extents::first_extent_offset(path).unwrap_or(None))
    }

    fn os_str_to_raw(path: &OsStr) -> RawPath {
        unix::os_str_to_raw(path)
    }

    fn raw_to_os_string(path: &RawPath) -> OsString {
        unix::raw_to_os_string(path)
    }
}
