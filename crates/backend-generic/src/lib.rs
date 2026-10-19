//! Generic backend: `std::fs` walking + `pread`, portable path codec, and
//! per-OS volume identity. All `cfg` and `unsafe` live here, never in `core`.

pub mod codec;
// Ungated on purpose: the pure parsing helpers in `extents` are unit-tested on every OS.
pub mod extents;
pub mod meta;
pub mod probe;
pub mod reader;
pub mod volume;
pub mod walk;

pub use codec::GenericPathCodec;
pub use probe::{fs_locality, FsLocality, GenericStorageProbe};
pub use reader::FsContentReader;
pub use volume::GenericVolumeIdentity;
pub use walk::WalkdirEnumerator;

/// Stable file identity from one attributes-only handle. 8 bytes when the 128-bit id is the
/// zero-extended 64-bit index (NTFS), so walker and `$MFT` ids stay equal; otherwise all 16
/// bytes (ReFS), never truncated. With no 128-bit id, the 64-bit index is used.
#[cfg(any(windows, test))]
pub fn file_id_bytes(id128: Option<&[u8; 16]>, index: u64) -> Vec<u8> {
    let low = index.to_le_bytes();
    match id128 {
        Some(id) if id[8..] == [0u8; 8] && id[..8] == low => low.to_vec(),
        Some(id) => id.to_vec(),
        None => low.to_vec(),
    }
}

/// Convenience bundle so `cli` can pass one object around.
pub struct GenericBackend {
    pub codec: GenericPathCodec,
    pub reader: FsContentReader,
    pub enumerator: WalkdirEnumerator,
    pub volume: GenericVolumeIdentity,
    pub probe: GenericStorageProbe,
}

impl Default for GenericBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GenericBackend {
    pub fn new() -> Self {
        GenericBackend {
            codec: GenericPathCodec,
            reader: FsContentReader::new(),
            enumerator: WalkdirEnumerator,
            volume: GenericVolumeIdentity,
            probe: GenericStorageProbe,
        }
    }
}

/// Win32 handle and path helpers shared by the storage probe and the extent lookup.
/// Calls that are specific to one module stay in that module.
#[cfg(windows)]
pub mod win_sys {
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::null_mut;
    use std::sync::{Arc, OnceLock, PoisonError, RwLock};

    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, MAX_PATH,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileBasicInfo, FileIdInfo, GetFileInformationByHandle,
        GetFileInformationByHandleEx, GetVolumePathNameW, BY_HANDLE_FILE_INFORMATION,
        FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_INFO_BY_HANDLE_CLASS, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };

    /// A Win32 handle that is closed on drop, so every return path releases it.
    pub struct OwnedHandle(HANDLE);

    impl OwnedHandle {
        /// Takes ownership of a handle returned by an open call such as `OpenProcessToken`.
        ///
        /// # Safety
        /// `handle` must be a valid handle that the caller owns and that nothing else
        /// closes. The guard closes it on drop.
        pub unsafe fn from_raw(handle: HANDLE) -> Self {
            Self(handle)
        }

        pub fn raw(&self) -> HANDLE {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: `self.0` came from a successful open call (`CreateFileW`, or
            // `OpenProcessToken` through `from_raw`) and only this guard closes it, so it is
            // closed exactly once. A failed close has no recovery path, so the result is
            // dropped.
            let _ = unsafe { CloseHandle(self.0) };
        }
    }

    /// NUL-terminated UTF-16, the form every `...W` Win32 call takes.
    pub fn to_wide_nul(s: impl AsRef<OsStr>) -> Vec<u16> {
        s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Opens an existing file or volume. Errors carry the OS error code.
    pub fn open_handle(
        wide_path: &[u16],
        access: u32,
        share: u32,
        flags: u32,
    ) -> io::Result<OwnedHandle> {
        // The wide string is passed as a raw pointer, so the NUL terminator is what keeps
        // the kernel inside the slice. Check it here rather than trust every caller.
        if wide_path.last() != Some(&0) {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        // SAFETY: `wide_path` is NUL-terminated (checked above) and outlives the call. The
        // security attributes and template handle are optional and passed as null.
        let handle = unsafe {
            CreateFileW(
                wide_path.as_ptr(),
                access,
                share,
                std::ptr::null(),
                OPEN_EXISTING,
                flags,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(OwnedHandle(handle))
    }

    /// Everything one attributes-only open reveals about a file. All of it comes from a
    /// single handle, so a file costs one open.
    pub struct WinFileInfo {
        pub attrs: u32,
        pub size: u64,
        /// FILETIME ticks (100 ns since 1601).
        pub created: u64,
        /// FILETIME ticks.
        pub written: u64,
        /// `FILE_BASIC_INFO.ChangeTime` in FILETIME ticks. `None` if the query failed.
        pub change_time: Option<u64>,
        /// Stable identity, see `file_id_bytes`.
        pub file_id: Vec<u8>,
        pub nlink: u32,
        /// `dwVolumeSerialNumber`, the device the `$MFT` path must report too.
        pub volume_serial: u32,
    }

    /// Info for `path` without following a final symlink. `None` when the file cannot be
    /// opened (access denied, path too long, vanished), so callers fall back to cached
    /// metadata. The 128-bit id query is optional: without it the 64-bit index is used.
    pub fn file_info(path: &Path) -> Option<WinFileInfo> {
        let wide = to_wide_nul(path.as_os_str());
        // Access 0 asks for attributes only, so no read lock is taken on the file.
        let handle = open_handle(
            &wide,
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
        )
        .ok()?;
        // SAFETY: `BY_HANDLE_FILE_INFORMATION` holds only integers and `FILETIME`s (two
        // integers each), so the all-zero pattern is a valid value.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `handle` is a live handle owned by the guard, and `info` is a writable,
        // correctly sized out-parameter for the duration of the call.
        let ok = unsafe { GetFileInformationByHandle(handle.raw(), &mut info) };
        if ok == 0 {
            return None;
        }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        let id128 =
            query_by_handle::<FILE_ID_INFO>(&handle, FileIdInfo).map(|id| id.FileId.Identifier);
        let change_time = query_by_handle::<FILE_BASIC_INFO>(&handle, FileBasicInfo)
            .and_then(|basic| u64::try_from(basic.ChangeTime).ok());
        Some(WinFileInfo {
            attrs: info.dwFileAttributes,
            size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
            created: filetime_bits(info.ftCreationTime),
            written: filetime_bits(info.ftLastWriteTime),
            change_time,
            file_id: super::file_id_bytes(id128.as_ref(), index),
            nlink: info.nNumberOfLinks,
            volume_serial: info.dwVolumeSerialNumber,
        })
    }

    /// One `GetFileInformationByHandleEx` call for a plain-old-data class. `None` on failure.
    fn query_by_handle<T: Copy>(
        handle: &OwnedHandle,
        class: FILE_INFO_BY_HANDLE_CLASS,
    ) -> Option<T> {
        // SAFETY: `T` is one of the `FILE_*_INFO` structs made of integers only, so the
        // all-zero pattern is a valid value.
        let mut out: T = unsafe { std::mem::zeroed() };
        let len = u32::try_from(std::mem::size_of::<T>()).ok()?;
        // SAFETY: `handle` is live; `out` is writable for exactly `len` bytes, and `class`
        // selects a structure of that size.
        let ok = unsafe {
            GetFileInformationByHandleEx(handle.raw(), class, (&mut out as *mut T).cast(), len)
        };
        (ok != 0).then_some(out)
    }

    /// FILETIME as one 64-bit tick count.
    fn filetime_bits(ft: FILETIME) -> u64 {
        (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime)
    }

    /// Volume root that `GetVolumePathNameW` reports for `path`, such as `C:\` or
    /// `\\?\Volume{GUID}\`. `None` on failure.
    pub fn volume_root(path: &Path) -> Option<String> {
        let wide = to_wide_nul(path.as_os_str());
        let mut root = [0u16; MAX_PATH as usize];
        // SAFETY: `wide` is NUL-terminated and alive for the call. `root` is writable for
        // exactly `MAX_PATH` u16 units, and that count is passed, so the write stays in bounds.
        let ok = unsafe { GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), MAX_PATH) };
        if ok == 0 {
            return None;
        }
        let len = root.iter().position(|&unit| unit == 0)?;
        String::from_utf16(root.get(..len)?).ok()
    }

    /// Entry cap for the per-directory memo. The memo is cleared when it reaches this size.
    const DIR_MEMO_CAP: usize = 65_536;

    /// Counts real `volume_root` calls (memo misses), so tests can observe the memo.
    #[cfg(test)]
    static VOLUME_ROOT_CALLS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    /// `volume_root(path)`, memoized per parent directory. Every entry directly inside one
    /// directory is on that directory's volume, so one lookup serves them all. `None` is
    /// memoized too. A path with no parent is looked up each time.
    pub fn volume_root_cached(path: &Path) -> Option<Arc<str>> {
        static BY_DIR: OnceLock<RwLock<HashMap<PathBuf, Option<Arc<str>>>>> = OnceLock::new();
        let Some(dir) = path.parent() else {
            return volume_root(path).map(Arc::from);
        };
        let memo = BY_DIR.get_or_init(Default::default);
        // Warm hits take only the shared lock.
        if let Some(hit) = memo.read().unwrap_or_else(PoisonError::into_inner).get(dir) {
            return hit.clone();
        }
        #[cfg(test)]
        VOLUME_ROOT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root: Option<Arc<str>> = volume_root(path).map(Arc::from);
        let mut w = memo.write().unwrap_or_else(PoisonError::into_inner);
        if w.len() >= DIR_MEMO_CAP {
            w.clear();
        }
        w.insert(dir.to_path_buf(), root.clone());
        root
    }

    #[cfg(test)]
    mod memo_tests {
        use std::sync::atomic::Ordering;

        use super::{volume_root, volume_root_cached, VOLUME_ROOT_CALLS};

        #[test]
        fn volume_root_cached_should_match_volume_root_and_skip_repeat_lookups() {
            let base = std::env::temp_dir().join(format!("dedupe_memo_{}", std::process::id()));
            let dir_a = base.join("a");
            let dir_b = base.join("b");
            std::fs::create_dir_all(&dir_a).expect("create dir a");
            std::fs::create_dir_all(&dir_b).expect("create dir b");
            let a1 = dir_a.join("a1.txt");
            let a2 = dir_a.join("a2.txt");
            let b1 = dir_b.join("b1.txt");
            for file in [&a1, &a2, &b1] {
                std::fs::write(file, b"x").expect("write fixture");
            }

            let start = VOLUME_ROOT_CALLS.load(Ordering::Relaxed);
            assert_eq!(
                volume_root_cached(&a1).as_deref(),
                volume_root(&a1).as_deref()
            );
            let after_first = VOLUME_ROOT_CALLS.load(Ordering::Relaxed);
            assert_eq!(
                after_first - start,
                1,
                "first lookup in dir a must miss once"
            );

            // Same directory: memo hit, no new volume_root call.
            assert_eq!(
                volume_root_cached(&a2).as_deref(),
                volume_root(&a2).as_deref()
            );
            assert_eq!(VOLUME_ROOT_CALLS.load(Ordering::Relaxed), after_first);

            // Second directory: one new miss, and the value still matches.
            assert_eq!(
                volume_root_cached(&b1).as_deref(),
                volume_root(&b1).as_deref()
            );
            assert_eq!(VOLUME_ROOT_CALLS.load(Ordering::Relaxed), after_first + 1);

            let _ = std::fs::remove_dir_all(&base);
        }
    }
}

#[cfg(test)]
mod file_id_tests {
    use super::file_id_bytes;

    #[test]
    fn file_id_without_128_bit_id_uses_index_bytes() {
        assert_eq!(
            file_id_bytes(None, 0x0102),
            vec![0x02, 0x01, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn file_id_zero_extended_ntfs_id_is_8_bytes() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(file_id_bytes(Some(&id), 42), 42u64.to_le_bytes().to_vec());
    }

    #[test]
    fn file_id_with_high_bits_is_full_16_bytes() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&42u64.to_le_bytes());
        id[15] = 1;
        assert_eq!(file_id_bytes(Some(&id), 42), id.to_vec());
    }

    #[test]
    fn file_id_low_part_mismatch_is_full_16_bytes() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(file_id_bytes(Some(&id), 42), id.to_vec());
    }
}
