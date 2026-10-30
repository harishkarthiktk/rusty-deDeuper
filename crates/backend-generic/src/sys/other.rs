//! Fallback `Platform` for targets that are not Linux, macOS or Windows. Unix targets of this
//! kind keep the unix behaviour for metadata, opening, reading and path bytes.

use std::ffi::{OsStr, OsString};
use std::fs::{File, FileType, Metadata, OpenOptions};
use std::io;
use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::path::RawPath;
use dedupe_core::traits::{FileMeta, VolumeInfo};

use super::Platform;
use crate::probe::FsLocality;

pub(crate) struct Other;

impl Platform for Other {
    fn identify(path: &Path) -> Result<VolumeInfo> {
        let mount = path.to_path_buf();
        let display = mount.to_string_lossy().into_owned();
        Ok(VolumeInfo {
            uuid: format!("none:other:{display}"),
            uuid_kind: dedupe_core::model::UuidKind::None,
            mount: RawPath::new(mount.to_string_lossy().as_bytes().to_vec()),
            fs_type: dedupe_core::model::FsType::Other,
            label: None,
        })
    }

    fn is_rotational(path: &Path) -> Result<Option<bool>> {
        // No probe on this target, so the answer is unknown.
        let _ = path;
        Ok(None)
    }

    fn fs_locality(path: &Path) -> FsLocality {
        let _ = path;
        FsLocality::Unknown
    }

    fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, Option<u64>, i64, i64) {
        #[cfg(unix)]
        {
            super::unix::identity_bits(md)
        }
        #[cfg(not(unix))]
        {
            let _ = md;
            (None, None, 1, 0)
        }
    }

    fn platform_attr_bits(_md: &Metadata) -> i64 {
        0
    }

    fn ctime_ns(md: &Metadata) -> Option<i64> {
        #[cfg(unix)]
        {
            super::unix::ctime_ns(md)
        }
        #[cfg(not(unix))]
        {
            use dedupe_core::time::system_time_to_unix_ns;
            md.created().ok().and_then(system_time_to_unix_ns)
        }
    }

    fn file_meta_at(_path: &Path, md: &Metadata, ft: FileType) -> FileMeta {
        crate::meta::file_meta_from(md, ft)
    }

    fn walk_file_meta(entry: &walkdir::DirEntry, ft: FileType) -> io::Result<FileMeta> {
        let md = entry.path().symlink_metadata()?;
        Ok(crate::meta::file_meta_from(&md, ft))
    }

    fn configure_open(opts: &mut OpenOptions) {
        #[cfg(unix)]
        {
            super::unix::configure_open(opts);
        }
        #[cfg(not(unix))]
        {
            let _ = opts;
        }
    }

    fn after_open(file: &File) -> io::Result<()> {
        #[cfg(unix)]
        {
            super::unix::after_open(file)
        }
        #[cfg(not(unix))]
        {
            let _ = file;
            Ok(())
        }
    }

    fn read_once(file: &File, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(unix)]
        {
            super::unix::read_once(file, offset, buf)
        }
        #[cfg(not(unix))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = file;
            file.seek(SeekFrom::Start(offset))?;
            file.read(buf)
        }
    }

    fn first_extent_hint(path: &Path) -> Result<Option<u64>> {
        let _ = path;
        Ok(None)
    }

    fn os_str_to_raw(path: &OsStr) -> RawPath {
        #[cfg(unix)]
        {
            super::unix::os_str_to_raw(path)
        }
        #[cfg(not(unix))]
        {
            RawPath::new(path.to_string_lossy().as_bytes().to_vec())
        }
    }

    fn raw_to_os_string(path: &RawPath) -> OsString {
        #[cfg(unix)]
        {
            super::unix::raw_to_os_string(path)
        }
        #[cfg(not(unix))]
        {
            OsString::from(String::from_utf8_lossy(&path.0).into_owned())
        }
    }
}
