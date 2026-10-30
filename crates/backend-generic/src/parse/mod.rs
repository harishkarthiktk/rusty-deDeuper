//! Pure parsers. No I/O and no `unsafe`; each is unit-tested on every OS where its
//! gating says so, while the OS code that feeds it lives in `sys/`.

#[cfg(any(windows, test))]
pub(crate) mod device_path;
#[cfg(any(windows, test))]
pub(crate) mod extents;
#[cfg(target_os = "linux")]
pub(crate) mod mountinfo;
#[cfg(any(target_os = "macos", test))]
pub(crate) mod plist;
#[cfg(any(windows, test))]
pub(crate) mod seek_penalty;
#[cfg(any(target_os = "linux", test))]
pub(crate) mod sysfs;
#[cfg(any(windows, test))]
pub(crate) mod wide;
#[cfg(any(windows, test))]
pub mod wtf8;
