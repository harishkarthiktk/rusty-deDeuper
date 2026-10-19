//! Safe traits that isolate every OS-specific concern from `core`.

use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::model::{FsType, Hash, UuidKind};
use crate::path::RawPath;

/// What the walker found at one path.
#[derive(Clone, Debug)]
pub enum WalkItem {
    Dir {
        path: RawPath,
    },
    File {
        path: RawPath,
        meta: FileMeta,
    },
    /// An entry the walker could not read. `is_dir` is true when the failed path is (or
    /// may be) a directory: its subtree was not enumerated.
    Error {
        path: RawPath,
        is_dir: bool,
        kind: crate::model::ErrorKind,
        message: String,
    },
}

/// Filesystem metadata, normalized into plain integers/bytes (no `cfg` in core).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    /// Stable file identity: inode / NTFS FRN / APFS file id. `None` where absent.
    pub file_id: Option<Vec<u8>>,
    /// Device the entry was observed on this run: `st_dev` on Unix. Runtime only, never
    /// persisted as identity (not stable across reboots or re-plugs). `None` when unknown.
    pub dev: Option<u64>,
    pub nlink: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: Option<i64>,
    pub attrs: i64,
    pub kind: EntryKind,
}

impl FileMeta {
    /// Size as a byte count. A negative value is corrupt data: it is reported,
    /// never wrapped into a huge `u64`.
    pub fn size_bytes(&self) -> Result<u64> {
        u64::try_from(self.size)
            .map_err(|_| CoreError::InvalidArg(format!("negative file size {}", self.size)))
    }
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

/// One open file. Positional reads with no shared cursor, so a handle can move
/// to a worker thread.
pub trait FileHandle: Send {
    /// Read up to `buf.len()` bytes at `offset`. Returns the count read, `0` at EOF.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize>;
}

/// Hint for how a handle's reads will be issued. Backends may use it for
/// buffering or read-ahead, or ignore it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessPattern {
    /// Reads walk the file front to back (whole-file hashing, byte verify).
    Sequential,
    /// Reads jump between offsets (the prefix hash touches first and last bytes).
    Random,
}

/// What to open: the path always, plus the stored file id when the backend can open by id.
#[derive(Clone, Copy, Debug)]
pub struct ReadTarget<'a> {
    /// Native path, the only locator every backend understands.
    pub path: &'a Path,
    /// The backend's own identity bytes, e.g. an 8-byte NTFS file reference on
    /// Windows. Backends that cannot open by id ignore it.
    pub file_id: Option<&'a [u8]>,
    /// Hint for how the reads will be issued.
    pub access: AccessPattern,
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

    /// Open once for many positional reads. The default opens nothing: every
    /// read on the handle goes back through `read_at`, so existing doubles keep
    /// their per-call behavior. `Self` is not required to be `Sized`, so this
    /// stays callable on `&dyn ContentReader`.
    fn open<'a>(&'a self, path: &'a Path) -> Result<Box<dyn FileHandle + 'a>> {
        Ok(Box::new(PathHandle { reader: self, path }))
    }

    /// Open by target. The default ignores `file_id` and `access` and calls
    /// [`ContentReader::open`], so existing readers keep their behavior. A backend
    /// that can open by id overrides this.
    fn open_target<'a>(&'a self, t: ReadTarget<'a>) -> Result<Box<dyn FileHandle + 'a>> {
        self.open(t.path)
    }

    /// Physical offset of the first extent, used to order reads on rotational
    /// media. `None` when unsupported or unknown.
    fn first_extent_offset(&self, _path: &Path) -> Result<Option<u64>> {
        Ok(None)
    }
}

/// Default [`FileHandle`]: forwards each read to [`ContentReader::read_at`].
struct PathHandle<'a, R: ContentReader + ?Sized> {
    reader: &'a R,
    path: &'a Path,
}

impl<R: ContentReader + ?Sized> FileHandle for PathHandle<'_, R> {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        self.reader.read_at(self.path, offset, buf)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Overrides only `open` and counts calls, so the default `open_target` route
    /// to `open` is observable.
    struct OpenCounter {
        opens: AtomicU64,
    }

    impl ContentReader for OpenCounter {
        fn read_at(&self, _path: &Path, _offset: u64, _buf: &mut [u8]) -> Result<usize> {
            Ok(0)
        }

        fn metadata(&self, _path: &Path) -> Result<FileMeta> {
            Err(CoreError::Other(
                "metadata is not used by this double".into(),
            ))
        }

        fn open<'a>(&'a self, path: &'a Path) -> Result<Box<dyn FileHandle + 'a>> {
            self.opens.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(PathHandle { reader: self, path }))
        }

        fn opens(&self) -> u64 {
            self.opens.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn open_target_should_delegate_to_open_when_not_overridden() {
        let reader = OpenCounter {
            opens: AtomicU64::new(0),
        };
        let target = ReadTarget {
            path: Path::new("x"),
            file_id: Some(&b"abc"[..]),
            access: AccessPattern::Random,
        };
        reader
            .open_target(target)
            .expect("open_target on a default reader should succeed");
        assert_eq!(
            reader.opens(),
            1,
            "open_target should call open once, got {}",
            reader.opens()
        );
    }

    #[test]
    fn size_bytes_should_return_value_when_non_negative() {
        let meta = FileMeta {
            dev: None,
            file_id: None,
            nlink: 1,
            size: 4096,
            mtime_ns: 0,
            ctime_ns: None,
            attrs: 0,
            kind: EntryKind::RegularFile,
        };
        let got = meta.size_bytes();
        assert!(
            matches!(got, Ok(4096)),
            "size 4096 should map to Ok(4096), got {got:?}"
        );
    }

    #[test]
    fn size_bytes_should_reject_negative() {
        let meta = FileMeta {
            dev: None,
            file_id: None,
            nlink: 1,
            size: -5,
            mtime_ns: 0,
            ctime_ns: None,
            attrs: 0,
            kind: EntryKind::RegularFile,
        };
        let got = meta.size_bytes();
        assert!(
            matches!(got, Err(CoreError::InvalidArg(ref msg)) if msg.contains("-5")),
            "size -5 should be InvalidArg with the value in its message, got {got:?}"
        );
    }
}
