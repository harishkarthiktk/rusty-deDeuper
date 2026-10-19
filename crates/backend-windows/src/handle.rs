//! Opens files for content reads: by the stored NTFS file id when one is given, by path otherwise,
//! and as a plain synchronous handle or an overlapped read-ahead handle.
//!
//! The planning and decoding helpers are pure. They build on every OS under `cfg(test)`, so
//! their tests run on the host. Everything that calls Win32 sits in `win32`, which is Windows-only.

use dedupe_core::traits::AccessPattern;

/// The kind of handle a read plan needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenKind {
    /// A synchronous handle, read with positional `seek_read`.
    Sync,
    /// A handle opened with `FILE_FLAG_OVERLAPPED` and read through `ReadAhead`.
    Overlapped,
}

/// Chooses the read path. Overlapped reads pay off only for a sequential scan of a file that
/// holds at least two chunks with at least two slots. Random reads, a single slot, or an unknown
/// length all take the sync path.
fn plan_open(
    access: AccessPattern,
    read_ahead: usize,
    file_len: Option<u64>,
    chunk: usize,
) -> OpenKind {
    // Saturating, so an absurd chunk size cannot wrap the threshold around.
    let min_len = u64::try_from(chunk).unwrap_or(u64::MAX).saturating_mul(2);
    let worth_overlapping = access == AccessPattern::Sequential
        && read_ahead >= 2
        && file_len.is_some_and(|len| len >= min_len);
    if worth_overlapping {
        OpenKind::Overlapped
    } else {
        OpenKind::Sync
    }
}

/// Decodes a stored file id as an NTFS file reference. Only the 8-byte form is accepted,
/// little-endian, which is what `frn.to_le_bytes()` writes. Other lengths are not NTFS ids
/// and must not be passed to `OpenFileById`. ReFS ids are 16 bytes (`FILE_ID_128`) and are
/// rejected here on purpose: they never reach `OpenFileById` and fall back to the path.
fn file_id_to_frn(bytes: &[u8]) -> Option<i64> {
    let array: [u8; 8] = bytes.try_into().ok()?;
    Some(i64::from_le_bytes(array))
}

#[cfg(windows)]
pub use win32::HandleReader;
#[cfg(windows)]
pub(crate) use win32::{file_len, SHARE_ALL};

#[cfg(windows)]
mod win32 {
    #![expect(
        unsafe_code,
        reason = "OpenFileById and GetFileSizeEx take raw handles; each unsafe block states its invariant, and the sync handle is handed to std::fs::File"
    )]

    use std::collections::HashMap;
    use std::fs::File;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::fs::FileExt;
    use std::os::windows::io::FromRawHandle;
    use std::path::Path;
    use std::ptr::{null, null_mut};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, PoisonError, RwLock};

    use dedupe_backend_generic::win_sys::{
        open_handle, to_wide_nul, volume_root_cached, OwnedHandle,
    };
    use dedupe_backend_generic::FsContentReader;
    use dedupe_core::error::Result;
    use dedupe_core::traits::{AccessPattern, ContentReader, FileHandle, FileMeta, ReadTarget};
    use windows_sys::Win32::Foundation::{FALSE, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        FileIdType, GetFileInformationByHandle, GetFileSizeEx, GetVolumeInformationByHandleW,
        OpenFileById, ReOpenFile, BY_HANDLE_FILE_INFORMATION, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_FLAG_RANDOM_ACCESS,
        FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_DESCRIPTOR, FILE_ID_DESCRIPTOR_0, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    use super::{file_id_to_frn, plan_open, OpenKind};
    use crate::overlapped::OverlappedFile;

    /// Read, write and delete sharing. Keeps the volume usable while it is scanned, and lets
    /// a concurrent delete or rename proceed.
    pub(crate) const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;

    /// Length of the file behind `handle`, from `GetFileSizeEx`.
    pub(crate) fn file_len(handle: &OwnedHandle) -> io::Result<u64> {
        let mut length: i64 = 0;
        // SAFETY: `handle` is live for the call. `length` is a live i64 that the kernel writes
        // only on success, and we read it only after success.
        let ok = unsafe { GetFileSizeEx(handle.raw(), &mut length) };
        if ok == FALSE {
            return Err(io::Error::last_os_error());
        }
        u64::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file length is negative"))
    }

    /// Volume-root handle used only as the hint for `OpenFileById`. Only NTFS hints are
    /// cached as usable: other file systems either lack stable 64-bit ids or were never
    /// validated.
    struct VolumeHint(OwnedHandle);

    // SAFETY: the hint is only read (its value is passed to `OpenFileById`). It is never closed
    // or replaced while shared, because the cache keeps it alive behind an `Arc`. Kernel handles
    // are not bound to the thread that opened them.
    unsafe impl Send for VolumeHint {}

    // SAFETY: a shared `&VolumeHint` only reads the handle value, as described for `Send`.
    unsafe impl Sync for VolumeHint {}

    /// A synchronous file handle. Positional reads use `seek_read`, as `FsContentReader` does.
    struct SyncFile {
        file: File,
    }

    impl SyncFile {
        fn new(handle: OwnedHandle) -> Self {
            let raw = handle.raw();
            // The guard would close the handle on drop. Forgetting it hands ownership to the File.
            std::mem::forget(handle);
            // SAFETY: `raw` is a valid synchronous handle from a successful open. The guard that
            // owned it was forgotten above, so the File is now the only owner and closes it once.
            Self {
                file: unsafe { File::from_raw_handle(raw) },
            }
        }
    }

    impl FileHandle for SyncFile {
        fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
            Ok(self.file.seek_read(buf, offset)?)
        }
    }

    /// Maps the access pattern to its cache hint flag.
    fn pattern_flags(access: AccessPattern) -> FILE_FLAGS_AND_ATTRIBUTES {
        match access {
            AccessPattern::Sequential => FILE_FLAG_SEQUENTIAL_SCAN,
            AccessPattern::Random => FILE_FLAG_RANDOM_ACCESS,
        }
    }

    /// True when `h` is a handle on an NTFS volume.
    fn is_ntfs(h: &OwnedHandle) -> bool {
        let mut name = [0u16; 32];
        // SAFETY: `h` is live. Only the file-system-name buffer is requested, and its
        // length in units is passed; every other out-parameter is null.
        let ok = unsafe {
            GetVolumeInformationByHandleW(
                h.raw(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                name.as_mut_ptr(),
                u32::try_from(name.len()).unwrap_or(0),
            )
        };
        let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        let ntfs: Vec<u16> = "NTFS".encode_utf16().collect();
        ok != FALSE && name[..len] == ntfs[..]
    }

    /// The handle really is file `frn`. Guards file systems that ignore the sequence
    /// number, and ids that resolve to a different file.
    fn handle_is(handle: &OwnedHandle, frn: i64) -> bool {
        // SAFETY: integers only; the all-zero pattern is valid.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `handle` is live and `info` is a writable out-parameter of the right size.
        if unsafe { GetFileInformationByHandle(handle.raw(), &mut info) } == FALSE {
            return false;
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        u64::from_ne_bytes(frn.to_ne_bytes()) == index
    }

    /// Same file, new handle with the overlapped flag. No second id or path resolution.
    fn reopen_overlapped(handle: &OwnedHandle, access: AccessPattern) -> io::Result<OwnedHandle> {
        // SAFETY: `handle` is live. `ReOpenFile` returns a new handle we own, or
        // `INVALID_HANDLE_VALUE`.
        let raw = unsafe {
            ReOpenFile(
                handle.raw(),
                GENERIC_READ,
                SHARE_ALL,
                pattern_flags(access) | FILE_FLAG_OVERLAPPED,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh handle from a successful `ReOpenFile`; the guard is its
        // only closer.
        Ok(unsafe { OwnedHandle::from_raw(raw) })
    }

    /// Opens file reference `frn` on the volume that `hint` belongs to.
    fn open_by_id(
        hint: &VolumeHint,
        frn: i64,
        flags: FILE_FLAGS_AND_ATTRIBUTES,
    ) -> io::Result<OwnedHandle> {
        let descriptor = FILE_ID_DESCRIPTOR {
            dwSize: u32::try_from(size_of::<FILE_ID_DESCRIPTOR>()).map_err(io::Error::other)?,
            Type: FileIdType,
            Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: frn },
        };
        // SAFETY: `hint.0` is a live volume handle that the cache keeps alive. `descriptor` is a
        // live local whose `dwSize` is its own size, and it is only read during the call. The
        // security attributes are optional and passed as null.
        let raw = unsafe {
            OpenFileById(
                hint.0.raw(),
                &descriptor,
                GENERIC_READ,
                SHARE_ALL,
                null(),
                flags,
            )
        };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a new handle that `OpenFileById` returned on success. Nothing else owns
        // it, so the guard is its only closer.
        Ok(unsafe { OwnedHandle::from_raw(raw) })
    }

    /// Opens file content by stored file id, with the path as the fallback.
    ///
    /// Reads are served from the file the id names. A file that moved still opens by id, which is
    /// the identity the database tracks. A stale or unknown id falls through to the path.
    pub struct HandleReader {
        inner: FsContentReader,
        read_ahead: usize,
        chunk: usize,
        /// Volume-root hints keyed by volume root (`GetVolumePathNameW` text). `None` records a
        /// volume whose hint could not be opened, so the failure is not retried for every file.
        /// Read-locked on hits; write-locked only on a miss.
        hints: RwLock<HashMap<Arc<str>, Option<Arc<VolumeHint>>>>,
        /// Opens made by this reader. Added to the inner reader's count in `opens`.
        opens: AtomicU64,
    }

    impl HandleReader {
        /// `read_ahead` is the number of overlapped slots, and `chunk` is the size of each.
        /// A `read_ahead` below 2 keeps every file on the sync path.
        pub fn new(read_ahead: usize, chunk: usize) -> Self {
            Self {
                inner: FsContentReader::new(),
                read_ahead,
                chunk,
                hints: RwLock::new(HashMap::new()),
                opens: AtomicU64::new(0),
            }
        }

        /// The hint handle for the volume that holds `path`, opened once per volume.
        ///
        /// The root comes from the per-directory memo in `volume_root_cached`, so a warm call
        /// costs no `GetVolumePathNameW`. A warm hit takes only the read lock.
        fn hint_for(&self, path: &Path) -> Option<Arc<VolumeHint>> {
            let root = volume_root_cached(path)?;
            // A poisoned lock still holds valid entries, so it is recovered, not propagated.
            let hit = self
                .hints
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&*root)
                .cloned();
            if let Some(cached) = hit {
                return cached;
            }
            let mut hints = self.hints.write().unwrap_or_else(PoisonError::into_inner);
            // Double-checked: another worker may have opened this volume while we waited.
            if let Some(cached) = hints.get(&*root) {
                return cached.clone();
            }
            // Opened under the write lock, so two workers never open the same hint at once.
            let opened = open_handle(
                &to_wide_nul(&*root),
                GENERIC_READ,
                SHARE_ALL,
                FILE_FLAG_BACKUP_SEMANTICS,
            )
            .ok()
            .filter(is_ntfs)
            .map(|handle| Arc::new(VolumeHint(handle)));
            hints.insert(root, opened.clone());
            opened
        }

        /// Opens the target with `flags`: by id first when it carries a valid id, then by path.
        fn open_handle_for(
            &self,
            t: ReadTarget<'_>,
            flags: FILE_FLAGS_AND_ATTRIBUTES,
        ) -> io::Result<OwnedHandle> {
            if let Some(frn) = t.file_id.and_then(file_id_to_frn) {
                if let Some(hint) = self.hint_for(t.path) {
                    match open_by_id(&hint, frn, flags) {
                        Ok(handle) if handle_is(&handle, frn) => return Ok(handle),
                        Ok(_) => {
                            tracing::debug!("open by id returned another file; opening by path");
                        }
                        Err(err) => {
                            tracing::debug!(error = %err, "open by file id failed; opening by path");
                        }
                    }
                }
            }
            open_handle(
                &to_wide_nul(t.path.as_os_str()),
                GENERIC_READ,
                SHARE_ALL,
                flags,
            )
        }
    }

    impl ContentReader for HandleReader {
        fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.inner.read_at(path, offset, buf)
        }

        fn metadata(&self, path: &Path) -> Result<FileMeta> {
            self.inner.metadata(path)
        }

        fn opens(&self) -> u64 {
            self.inner
                .opens()
                .saturating_add(self.opens.load(Ordering::Relaxed))
        }

        fn first_extent_offset(&self, path: &Path) -> Result<Option<u64>> {
            self.inner.first_extent_offset(path)
        }

        fn open<'a>(&'a self, path: &'a Path) -> Result<Box<dyn FileHandle + 'a>> {
            self.open_target(ReadTarget {
                path,
                file_id: None,
                access: AccessPattern::Random,
            })
        }

        fn open_target<'a>(&'a self, t: ReadTarget<'a>) -> Result<Box<dyn FileHandle + 'a>> {
            let handle = self.open_handle_for(t, pattern_flags(t.access))?;
            self.opens.fetch_add(1, Ordering::Relaxed);

            // The length decides between sync and overlapped, and the overlapped flag can only
            // be set at open time. So the sync handle is opened first, to learn the length,
            // and `ReOpenFile` derives the overlapped handle from it (no second id or path
            // resolution, so it cannot land on a different file).
            let length = file_len(&handle).ok();
            if let (OpenKind::Overlapped, Some(length)) = (
                plan_open(t.access, self.read_ahead, length, self.chunk),
                length,
            ) {
                match reopen_overlapped(&handle, t.access)
                    .and_then(|h| OverlappedFile::new(h, self.read_ahead, self.chunk, length))
                {
                    Ok(file) => {
                        self.opens.fetch_add(1, Ordering::Relaxed);
                        return Ok(Box::new(file));
                    }
                    Err(err) => {
                        tracing::debug!(error = %err, "overlapped reopen failed; using a sync handle");
                    }
                }
            }
            Ok(Box::new(SyncFile::new(handle)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHUNK: usize = 64 * 1024;

    #[test]
    fn plan_open_should_choose_overlapped_when_sequential_and_file_is_two_chunks() {
        let kind = plan_open(AccessPattern::Sequential, 4, Some(2 * CHUNK as u64), CHUNK);
        assert_eq!(
            kind,
            OpenKind::Overlapped,
            "a sequential scan of exactly two chunks should overlap"
        );
    }

    #[test]
    fn plan_open_should_choose_sync_when_file_is_one_byte_under_two_chunks() {
        let kind = plan_open(
            AccessPattern::Sequential,
            4,
            Some(2 * CHUNK as u64 - 1),
            CHUNK,
        );
        assert_eq!(
            kind,
            OpenKind::Sync,
            "a file under two chunks should use the sync path"
        );
    }

    #[test]
    fn plan_open_should_choose_sync_when_access_is_random() {
        let kind = plan_open(AccessPattern::Random, 4, Some(1 << 30), CHUNK);
        assert_eq!(
            kind,
            OpenKind::Sync,
            "random reads should never use read-ahead"
        );
    }

    #[test]
    fn plan_open_should_choose_sync_when_read_ahead_is_one() {
        let kind = plan_open(AccessPattern::Sequential, 1, Some(1 << 30), CHUNK);
        assert_eq!(kind, OpenKind::Sync, "a single slot has nothing to overlap");
    }

    #[test]
    fn plan_open_should_choose_sync_when_read_ahead_is_zero() {
        let kind = plan_open(AccessPattern::Sequential, 0, Some(1 << 30), CHUNK);
        assert_eq!(
            kind,
            OpenKind::Sync,
            "zero slots must not select overlapped reads"
        );
    }

    #[test]
    fn plan_open_should_choose_sync_when_length_is_unknown() {
        let kind = plan_open(AccessPattern::Sequential, 4, None, CHUNK);
        assert_eq!(
            kind,
            OpenKind::Sync,
            "an unknown length must take the safe path"
        );
    }

    #[test]
    fn plan_open_should_not_overflow_when_chunk_is_huge() {
        let kind = plan_open(AccessPattern::Sequential, 4, Some(1 << 40), usize::MAX);
        assert_eq!(
            kind,
            OpenKind::Sync,
            "a chunk larger than the file must stay on the sync path"
        );
    }

    #[test]
    fn file_id_to_frn_should_decode_little_endian_when_eight_bytes() {
        let frn: i64 = 0x0123_4567_89AB_CDEF;
        let bytes = frn.to_le_bytes();
        assert_eq!(
            file_id_to_frn(&bytes),
            Some(frn),
            "eight little-endian bytes should decode to the same file reference"
        );
    }

    #[test]
    fn file_id_to_frn_should_return_none_when_length_is_seven() {
        let bytes = [0u8; 7];
        assert_eq!(
            file_id_to_frn(&bytes),
            None,
            "a seven-byte id is not an NTFS file reference"
        );
    }

    #[test]
    fn file_id_to_frn_should_return_none_when_length_is_sixteen() {
        let bytes = [0u8; 16];
        assert_eq!(
            file_id_to_frn(&bytes),
            None,
            "a 128-bit id is not decoded by the 64-bit path"
        );
    }

    #[test]
    fn file_id_to_frn_should_return_none_when_empty() {
        assert_eq!(
            file_id_to_frn(&[]),
            None,
            "an empty id must not be treated as file reference zero"
        );
    }
}
