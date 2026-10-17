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
pub use probe::GenericStorageProbe;
pub use reader::FsContentReader;
pub use volume::GenericVolumeIdentity;
pub use walk::WalkdirEnumerator;

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
    use std::ffi::OsStr;
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::null_mut;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, MAX_PATH};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, GetVolumePathNameW, OPEN_EXISTING};

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
}
