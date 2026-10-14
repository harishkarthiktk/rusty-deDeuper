//! Safe traits that isolate every OS-specific concern from `core`.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::Result;
use crate::model::{FsType, Hash, UuidKind};
use crate::path::RawPath;

/// What the walker found at one path.
#[derive(Clone, Debug)]
pub enum WalkItem {
    Dir { path: RawPath },
    File { path: RawPath, meta: FileMeta },
}

/// Filesystem metadata, normalized into plain integers/bytes (no `cfg` in core).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    /// Stable file identity: inode / NTFS FRN / APFS file id. `None` where absent.
    pub file_id: Option<Vec<u8>>,
    pub nlink: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: Option<i64>,
    pub attrs: i64,
    pub kind: EntryKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    RegularFile,
    Directory,
    Symlink,
    Other,
}

/// Streams the entries under a root. Directories are yielded before their contents.
pub trait Enumerator: Send + Sync {
    fn walk(
        &self,
        root: &Path,
        cfg: &Config,
        visit: &mut dyn FnMut(WalkItem) -> Result<()>,
    ) -> Result<()>;
}

/// Reads file content and metadata without `unsafe` leaking into `core`.
pub trait ContentReader: Send + Sync {
    /// Read up to `buf.len()` bytes at `offset`. Returns the count read.
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize>;

    /// Read exactly the requested bytes (may return fewer at EOF).
    fn read_range(&self, path: &Path, offset: u64, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        let n = self.read_at(path, offset, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// `lstat`-equivalent metadata.
    fn metadata(&self, path: &Path) -> Result<FileMeta>;

    /// Count of opens, for "no I/O on rescan" tests. Zero unless instrumented.
    fn opens(&self) -> u64 {
        0
    }
}

/// Hash primitive. Allows a test double in place of BLAKE3.
pub trait Hasher: Send + Sync {
    fn hash_bytes(&self, data: &[u8]) -> Hash;

    /// Hash the concatenation of `parts` without an explicit join step.
    fn hash_concat(&self, parts: &[&[u8]]) -> Hash {
        let mut joined = Vec::new();
        for p in parts {
            joined.extend_from_slice(p);
        }
        self.hash_bytes(&joined)
    }
}

/// Volume identity: `FsUuid` where possible, `FsidFallback` otherwise. See 4.10.
pub trait VolumeIdentity: Send + Sync {
    fn identify(&self, path: &Path) -> Result<VolumeInfo>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
    pub uuid: String,
    pub uuid_kind: UuidKind,
    pub mount: RawPath,
    pub fs_type: FsType,
    pub label: Option<String>,
}

/// Converts between native paths and byte-exact [`RawPath`]. One impl per backend.
pub trait PathCodec: Send + Sync {
    fn to_raw(&self, path: &OsStr) -> RawPath;

    fn to_native(&self, path: &RawPath) -> OsString;

    fn path_to_raw(&self, path: &Path) -> RawPath {
        self.to_raw(path.as_os_str())
    }

    fn raw_to_path(&self, path: &RawPath) -> PathBuf {
        PathBuf::from(self.to_native(path))
    }

    /// Split into `(parent, final component)` for folder/name storage.
    fn split_component(&self, path: &Path) -> (RawPath, RawPath);
}

/// Detects rotational media. Policy lives in `core::storage_profile` (Phase 2).
pub trait StorageProbe: Send + Sync {
    /// `None` when unknown; callers assume SSD and log.
    fn is_rotational(&self, path: &Path) -> Result<Option<bool>>;
}

/// Placeholder for the delete/hardlink/move actions that land in Phase 7.
pub trait Action {
    fn describe(&self) -> String;
}
