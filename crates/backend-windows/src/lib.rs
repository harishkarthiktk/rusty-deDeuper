//! Windows backend: NTFS `$MFT` enumeration, open-by-id reads and overlapped read-ahead.
//!
//! Most modules here are portable: no `cfg(windows)`, no `unsafe`, no Win32 types. Their parsing,
//! path-building and scheduling logic builds and is unit-tested on every OS.
//! Win32 calls are confined to `cfg(windows)` items in `caps`, `volume_io` and `enumerator`.
//! `volume_io` and `caps` are declared on every OS because their pure helpers are tested there.
//! `handle` is declared on every OS too (under `test`) for its pure open-plan and file-id helpers.

// `deny`, not `forbid`: the Win32 modules opt in to `unsafe` locally with `#[expect]`, and
// `forbid` cannot be overridden.
#![deny(unsafe_code)]

pub mod caps;
#[cfg(windows)]
pub mod enumerator;
pub mod filetime;
#[cfg(any(windows, test))]
pub mod handle;
pub mod mft_cache;
pub mod mft_walk;
#[cfg(windows)]
mod overlapped;
pub mod readahead;
pub mod records;
pub mod sector;
pub mod tree;
pub mod usn;
pub mod volume_io;

pub use caps::{choose_mode, Capabilities, FallbackReason, Mode};
#[cfg(windows)]
pub use enumerator::{AutoEnumerator, CacheConfig, EnumNotices};
#[cfg(windows)]
pub use handle::HandleReader;
#[cfg(windows)]
pub use mft_walk::{MftEvent, MftObserver};
/// Exposed for `tests/handle_reader.rs` only. Not part of the crate's API.
#[cfg(windows)]
#[doc(hidden)]
pub use overlapped::OverlappedFile;
