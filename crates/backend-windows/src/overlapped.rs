//! Overlapped (asynchronous) positional reads over one file handle, scheduled by `ReadAhead`.
//!
//! Each slot owns a boxed `OVERLAPPED` (its address stays fixed while the kernel holds it), a
//! manual-reset event the kernel signals on completion, and a buffer. `ReadAhead` decides what
//! to submit and when to wait. This module turns those calls into Win32 calls and keeps every
//! buffer alive until the kernel has finished with it.
//!
//! Windows-only: declared behind `cfg(windows)` in `lib.rs`.

#![expect(
    unsafe_code,
    reason = "overlapped ReadFile and GetOverlappedResult write into memory the kernel holds until completion; each unsafe block states its invariant"
)]

use std::io;
use std::mem::replace;
use std::path::Path;
use std::ptr::{null, null_mut};

use dedupe_backend_generic::win_sys::{open_handle, to_wide_nul, OwnedHandle};
use dedupe_core::error::Result;
use dedupe_core::traits::FileHandle;
use windows_sys::Win32::Foundation::{
    ERROR_HANDLE_EOF, ERROR_IO_PENDING, FALSE, GENERIC_READ, HANDLE, TRUE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, FILE_FLAG_OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};
use windows_sys::Win32::System::IO::{
    CancelIoEx, GetOverlappedResult, OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0,
};

use crate::handle::{file_len, SHARE_ALL};
use crate::readahead::{AsyncBackend, ReadAhead};

/// What a slot is doing.
enum SlotState {
    Idle,
    /// A read the kernel may still be writing into. Only `complete` or `cancel_all` may wait on it.
    InFlight,
    /// A read that finished during `cancel_all`, or one that needs no wait (end of file). The
    /// result waits here until `complete` returns it.
    Waited(io::Result<usize>),
}

struct Slot {
    /// Boxed so the address the kernel holds stays fixed even when `slots` is moved.
    overlapped: Box<OVERLAPPED>,
    /// Manual-reset event named by `overlapped.hEvent`. Owned here so it outlives the read.
    event: OwnedHandle,
    /// The kernel writes into this buffer until the read completes.
    buf: Box<[u8]>,
    state: SlotState,
    /// Bytes the last completed read reported. `data` returns this prefix of `buf`.
    valid: usize,
}

/// Overlapped reads over one handle with a fixed set of slots.
pub(crate) struct OverlappedBackend {
    /// Must be open with `FILE_FLAG_OVERLAPPED`. A handle without that flag still works, because
    /// the read then completes synchronously, but the read-ahead gives no overlap.
    handle: OwnedHandle,
    slots: Vec<Slot>,
}

impl OverlappedBackend {
    /// `depth` slots of `chunk` bytes. Both must be at least 1, which the caller ensures.
    fn new(handle: OwnedHandle, depth: usize, chunk: usize) -> io::Result<Self> {
        // ReadFile takes a u32 length. Refusing an oversized chunk here avoids a failure on
        // every read later.
        u32::try_from(chunk).map_err(|_| invalid_input("chunk size must fit in u32"))?;
        let slots = (0..depth)
            .map(|_| Slot::new(chunk))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self { handle, slots })
    }
}

impl Slot {
    fn new(chunk: usize) -> io::Result<Self> {
        let event = create_event()?;
        Ok(Self {
            overlapped: Box::new(OVERLAPPED {
                Internal: 0,
                InternalHigh: 0,
                Anonymous: OVERLAPPED_0 {
                    Anonymous: OVERLAPPED_0_0 {
                        Offset: 0,
                        OffsetHigh: 0,
                    },
                },
                hEvent: event.raw(),
            }),
            event,
            buf: vec![0u8; chunk].into_boxed_slice(),
            state: SlotState::Idle,
            valid: 0,
        })
    }
}

/// A manual-reset, initially unsignaled event that `ResetEvent` clears before each read.
fn create_event() -> io::Result<OwnedHandle> {
    // SAFETY: the security attributes and the name are optional, and null is passed for both.
    // No pointer outlives the call.
    let raw = unsafe { CreateEventW(null(), TRUE, FALSE, null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `raw` is a non-null event handle that `CreateEventW` just returned. Nothing else
    // owns it, so the guard is its only closer.
    Ok(unsafe { OwnedHandle::from_raw(raw) })
}

impl AsyncBackend for OverlappedBackend {
    fn submit(&mut self, slot: usize, offset: u64, len: usize) -> io::Result<()> {
        let handle = self.handle.raw();
        let s = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| invalid_input("slot index is out of range"))?;
        if !matches!(s.state, SlotState::Idle) {
            return Err(invalid_input("slot already has a read in flight"));
        }
        if len > s.buf.len() {
            return Err(invalid_input("read is longer than the slot buffer"));
        }
        let bytes =
            u32::try_from(len).map_err(|_| invalid_input("read length does not fit in u32"))?;
        let (low, high) = split_offset(offset);
        s.overlapped.Anonymous = OVERLAPPED_0 {
            Anonymous: OVERLAPPED_0_0 {
                Offset: low,
                OffsetHigh: high,
            },
        };
        s.overlapped.hEvent = s.event.raw();

        // SAFETY: `s.event` is a live manual-reset event owned by this slot. Clearing it first
        // keeps `complete` from mistaking a signal left by an earlier read for this one.
        if unsafe { ResetEvent(s.event.raw()) } == FALSE {
            return Err(io::Error::last_os_error());
        }

        let overlapped: *mut OVERLAPPED = std::ptr::from_mut(&mut *s.overlapped);
        // SAFETY: `handle` is open and owned by `self`. `s.buf` is writable for `len` bytes,
        // checked above, and `bytes` is that same length. `overlapped` points into a Box owned
        // by this slot, so its address is fixed until `complete` or `cancel_all` has returned
        // for this slot. The byte-count out-parameter is null, which is allowed when an
        // OVERLAPPED is given. From now until completion the kernel may write `s.buf` and
        // `*overlapped`, and no Rust code reads or frees them while the slot is InFlight.
        let ok = unsafe {
            ReadFile(
                handle,
                s.buf.as_mut_ptr().cast(),
                bytes,
                null_mut(),
                overlapped,
            )
        };
        if ok != FALSE {
            // Completed synchronously. `complete` still reads the result through the OVERLAPPED.
            s.state = SlotState::InFlight;
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if is_code(&err, ERROR_IO_PENDING) {
            s.state = SlotState::InFlight;
            Ok(())
        } else if is_code(&err, ERROR_HANDLE_EOF) {
            // Nothing was started, so there is nothing to wait for. `complete` still runs once.
            s.valid = 0;
            s.state = SlotState::Waited(Ok(0));
            Ok(())
        } else {
            Err(err)
        }
    }

    fn complete(&mut self, slot: usize) -> io::Result<usize> {
        let handle = self.handle.raw();
        let s = self
            .slots
            .get_mut(slot)
            .ok_or_else(|| invalid_input("slot index is out of range"))?;
        let result = match replace(&mut s.state, SlotState::Idle) {
            SlotState::Waited(result) => result,
            SlotState::InFlight => wait_for(handle, &s.overlapped),
            SlotState::Idle => Err(invalid_input("complete called on a slot with no read")),
        };
        s.valid = result.as_ref().map_or(0, |bytes| *bytes);
        result
    }

    fn data(&self, slot: usize) -> &[u8] {
        self.slots
            .get(slot)
            .and_then(|s| s.buf.get(..s.valid))
            .unwrap_or(&[])
    }

    fn cancel_all(&mut self) {
        if !self
            .slots
            .iter()
            .any(|s| matches!(s.state, SlotState::InFlight))
        {
            return;
        }
        // SAFETY: `handle` is live. A null OVERLAPPED cancels every pending operation on the
        // handle, so no kernel write into a slot buffer can continue after this call. A failure
        // (for example, nothing pending) leaves the waits below to finish the job, so it is
        // ignored.
        let _ = unsafe { CancelIoEx(self.handle.raw(), null()) };
        let handle = self.handle.raw();
        for s in &mut self.slots {
            if matches!(s.state, SlotState::InFlight) {
                // The result is kept, not dropped, so the matching `complete` still returns it.
                s.state = SlotState::Waited(wait_for(handle, &s.overlapped));
            }
        }
    }
}

impl Drop for OverlappedBackend {
    fn drop(&mut self) {
        // ReadAhead cancels before it drops the backend. This covers a backend dropped on its
        // own. It must run before the fields drop, because the kernel may still write into the
        // buffers until every read has been waited for.
        self.cancel_all();
    }
}

// SAFETY: the backend moves between threads as a whole and is never shared, because it is not
// Sync. Its raw pointers are a kernel file handle, event handles, and OVERLAPPED values. The
// kernel reads and writes the OVERLAPPED values and buffers only while a read is pending. Every
// pending read is waited for in `complete` or `cancel_all`, and `Drop` runs `cancel_all`, so no
// kernel write can happen after the owner has moved on. The handle is a kernel object index and
// is not tied to the thread that opened it.
unsafe impl Send for OverlappedBackend {}

/// A file read through overlapped I/O with read-ahead.
///
/// Built by `HandleReader` for sequential reads of large files. `open` is a public entry point
/// for tests and tools that want overlapped reads without going through `HandleReader`.
pub struct OverlappedFile {
    reads: ReadAhead<OverlappedBackend>,
}

impl OverlappedFile {
    /// Wraps `handle`, which should be open with `FILE_FLAG_OVERLAPPED`, for a file of
    /// `file_len` bytes. A `depth` or `chunk` of 0 is raised to 1.
    pub fn new(handle: OwnedHandle, depth: usize, chunk: usize, file_len: u64) -> io::Result<Self> {
        let depth = depth.max(1);
        let chunk = chunk.max(1);
        let backend = OverlappedBackend::new(handle, depth, chunk)?;
        Ok(Self {
            reads: ReadAhead::new(backend, depth, chunk, file_len),
        })
    }

    /// Opens `path` for overlapped reads of its whole length.
    pub fn open(path: &Path, depth: usize, chunk: usize) -> io::Result<Self> {
        let handle = open_handle(
            &to_wide_nul(path.as_os_str()),
            GENERIC_READ,
            SHARE_ALL,
            FILE_FLAG_OVERLAPPED,
        )?;
        let length = file_len(&handle)?;
        Self::new(handle, depth, chunk, length)
    }
}

impl FileHandle for OverlappedFile {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        Ok(self.reads.read_at(offset, buf)?)
    }
}

/// Waits until the read on `overlapped` finishes and returns the bytes it moved. End of file
/// counts as zero bytes.
fn wait_for(handle: HANDLE, overlapped: &OVERLAPPED) -> io::Result<usize> {
    let mut transferred: u32 = 0;
    // SAFETY: `handle` is live, since its owner outlives every call here. `overlapped` points to
    // the slot's boxed OVERLAPPED, which is not moved or freed while its read is pending. The
    // caller only calls this for an InFlight slot. `transferred` is a live u32 out-parameter.
    // bWait is TRUE, so the call returns only after the kernel has finished writing the buffer.
    let ok = unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, TRUE) };
    if ok != FALSE {
        return usize::try_from(transferred)
            .map_err(|_| io::Error::other("transferred byte count does not fit in usize"));
    }
    let err = io::Error::last_os_error();
    if is_code(&err, ERROR_HANDLE_EOF) {
        Ok(0)
    } else {
        Err(err)
    }
}

/// Splits a 64-bit offset into the low and high halves that `OVERLAPPED` stores.
fn split_offset(offset: u64) -> (u32, u32) {
    let [a, b, c, d, e, f, g, h] = offset.to_le_bytes();
    (
        u32::from_le_bytes([a, b, c, d]),
        u32::from_le_bytes([e, f, g, h]),
    )
}

fn is_code(err: &io::Error, code: u32) -> bool {
    err.raw_os_error()
        .and_then(|raw| u32::try_from(raw).ok())
        .is_some_and(|raw| raw == code)
}

fn invalid_input(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
