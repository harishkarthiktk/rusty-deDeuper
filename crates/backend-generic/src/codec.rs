//! Native path <-> [`RawPath`] conversion.
//!
//! Unix: native bytes. Windows: a small WTF-8 encoder/decoder over `encode_wide()`
//! (std explicitly documents `as_encoded_bytes()` as unspecified, not for storage).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use dedupe_core::path::RawPath;
use dedupe_core::traits::PathCodec;

use crate::sys::{Os, Platform};

// Items that exist on some targets only (`wtf8` on Windows).
#[allow(
    unused_imports,
    reason = "the glob is empty on targets that have no such item"
)]
pub use crate::sys::exports::codec::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct GenericPathCodec;

impl PathCodec for GenericPathCodec {
    fn to_raw(&self, path: &OsStr) -> RawPath {
        Os::os_str_to_raw(path)
    }

    fn to_native(&self, path: &RawPath) -> OsString {
        Os::raw_to_os_string(path)
    }

    fn split_component(&self, path: &Path) -> (RawPath, RawPath) {
        let parent = path
            .parent()
            .map(|p| self.path_to_raw(p))
            .unwrap_or_else(RawPath::empty);
        let name = path
            .file_name()
            .map(|n| self.to_raw(n))
            .unwrap_or_else(RawPath::empty);
        (parent, name)
    }
}

impl GenericPathCodec {
    pub fn raw_to_pathbuf(&self, path: &RawPath) -> PathBuf {
        PathBuf::from(self.to_native(path))
    }
}
