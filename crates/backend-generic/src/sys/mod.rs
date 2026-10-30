//! Per-OS implementations. Each OS module provides one type implementing `Platform`.
//! Static dispatch: the OS is fixed at compile time, so a missing function fails that
//! target's build. Facade modules call `Os::<fn>`; they hold no target `cfg`.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::path::RawPath;
use dedupe_core::traits::{FileMeta, VolumeInfo};

use crate::probe::FsLocality;

pub(crate) trait Platform {
    /// `VolumeIdentity::identify` body.
    fn identify(path: &Path) -> Result<VolumeInfo>;
    /// `StorageProbe::is_rotational` body.
    fn is_rotational(path: &Path) -> Result<Option<bool>>;
    /// `fs_locality` body.
    fn fs_locality(path: &Path) -> FsLocality;
    /// `(file_id, dev, nlink, HARDLINK bit)`.
    fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64);
    /// Platform attribute bits that `Metadata` exposes beyond identity.
    fn platform_attr_bits(md: &Metadata) -> i64;
    fn ctime_ns(md: &Metadata) -> Option<i64>;
    /// Metadata for a regular file (`meta::file_meta_at`).
    fn file_meta_at(path: &Path, md: &Metadata, ft: FileType) -> FileMeta;
    /// Metadata for a non-directory walker entry without following links.
    fn walk_file_meta(entry: &walkdir::DirEntry, ft: FileType) -> io::Result<FileMeta>;
    /// OS flags applied before `open_regular` opens the file.
    fn configure_open(opts: &mut OpenOptions);
    /// OS fix-up applied after `open_regular` opened the file.
    fn after_open(file: &File) -> io::Result<()>;
    /// One positional read.
    fn read_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// `ContentReader::first_extent_offset` body; `Ok(None)` where unsupported.
    fn first_extent_hint(path: &Path) -> Result<Option<u64>>;
    fn os_str_to_raw(path: &OsStr) -> RawPath;
    fn raw_to_os_string(path: &RawPath) -> OsString;
}

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub(crate) type Os = linux::Linux;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) type Os = macos::Macos;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) type Os = windows::Windows;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod other;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(crate) type Os = other::Other;

/// Public items that exist on some targets only. Each facade module glob re-exports its
/// own submodule, so the facade itself carries no target `cfg`.
pub(crate) mod exports {
    pub mod codec {
        #[cfg(windows)]
        pub use crate::parse::wtf8;
    }

    pub mod extents {
        #[cfg(target_os = "linux")]
        pub use crate::sys::linux::extents::first_extent_offset;
        #[cfg(windows)]
        pub use crate::sys::windows::extents::first_extent_offset;
    }

    pub mod meta {
        #[cfg(windows)]
        pub use crate::sys::windows::meta::file_meta_from_info;
    }
}

#[cfg(test)]
mod tests;
