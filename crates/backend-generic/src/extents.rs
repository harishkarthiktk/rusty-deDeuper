//! First physical extent of a file: `FS_IOC_FIEMAP` on Linux and
//! `FSCTL_GET_RETRIEVAL_POINTERS` on Windows.
//!
//! The offset orders HDD scans so the head sweeps the platter once instead of
//! seeking per file. It is a scheduling hint, never a correctness input.

// `first_extent_offset` exists on Linux and Windows only; it is re-exported from `sys`.
#[allow(
    unused_imports,
    reason = "the glob is empty on targets that have no such item"
)]
pub use crate::sys::exports::extents::*;
