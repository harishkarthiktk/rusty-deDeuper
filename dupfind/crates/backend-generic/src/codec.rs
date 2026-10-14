//! Native path <-> [`RawPath`] conversion.
//!
//! Unix: native bytes. Windows: a small WTF-8 encoder/decoder over `encode_wide()`
//! (std explicitly documents `as_encoded_bytes()` as unspecified, not for storage).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use dupfind_core::path::RawPath;
use dupfind_core::traits::PathCodec;

#[derive(Clone, Copy, Debug, Default)]
pub struct GenericPathCodec;

impl PathCodec for GenericPathCodec {
    fn to_raw(&self, path: &OsStr) -> RawPath {
        os_str_to_raw(path)
    }

    fn to_native(&self, path: &RawPath) -> OsString {
        raw_to_os_string(path)
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

#[cfg(unix)]
fn os_str_to_raw(path: &OsStr) -> RawPath {
    use std::os::unix::ffi::OsStrExt;
    RawPath::new(path.as_bytes().to_vec())
}

#[cfg(unix)]
fn raw_to_os_string(path: &RawPath) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(path.0.clone())
}

#[cfg(windows)]
fn os_str_to_raw(path: &OsStr) -> RawPath {
    use std::os::windows::ffi::OsStrExt;
    RawPath::new(wtf8::encode(path.encode_wide()))
}

#[cfg(windows)]
fn raw_to_os_string(path: &RawPath) -> OsString {
    use std::os::windows::ffi::OsStringExt;
    OsString::from_wide(&wtf8::decode(&path.0))
}

#[cfg(not(any(unix, windows)))]
fn os_str_to_raw(path: &OsStr) -> RawPath {
    RawPath::new(path.to_string_lossy().as_bytes().to_vec())
}

#[cfg(not(any(unix, windows)))]
fn raw_to_os_string(path: &RawPath) -> OsString {
    OsString::from(String::from_utf8_lossy(&path.0).into_owned())
}

/// Minimal WTF-8 codec: preserves unpaired surrogates that valid UTF-8 cannot.
#[cfg(windows)]
pub mod wtf8 {
    /// Encode UTF-16 units (possibly unpaired surrogates) to WTF-8 bytes.
    pub fn encode(units: impl Iterator<Item = u16>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut iter = units.peekable();
        while let Some(unit) = iter.next() {
            if (0xD800..=0xDBFF).contains(&unit) {
                if let Some(&low) = iter.peek() {
                    if (0xDC00..=0xDFFF).contains(&low) {
                        iter.next();
                        let cp = 0x10000 + (((unit as u32 - 0xD800) << 10) | (low as u32 - 0xDC00));
                        push_utf8(&mut out, cp);
                        continue;
                    }
                }
                push_wtf8_3(&mut out, unit as u32);
            } else if (0xDC00..=0xDFFF).contains(&unit) {
                push_wtf8_3(&mut out, unit as u32);
            } else {
                push_utf8(&mut out, unit as u32);
            }
        }
        out
    }

    /// Decode WTF-8 bytes back to UTF-16 units.
    pub fn decode(bytes: &[u8]) -> Vec<u16> {
        let mut units = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let b0 = bytes[i];
            if b0 < 0x80 {
                units.push(b0 as u16);
                i += 1;
            } else if b0 >> 5 == 0b110 {
                if i + 1 >= bytes.len() {
                    break;
                }
                let cp = (((b0 & 0x1f) as u32) << 6) | (bytes[i + 1] & 0x3f) as u32;
                units.push(cp as u16);
                i += 2;
            } else if b0 >> 4 == 0b1110 {
                if i + 2 >= bytes.len() {
                    break;
                }
                let cp = (((b0 & 0x0f) as u32) << 12)
                    | (((bytes[i + 1] & 0x3f) as u32) << 6)
                    | (bytes[i + 2] & 0x3f) as u32;
                units.push(cp as u16); // surrogate or BMP, both fit in u16
                i += 3;
            } else if b0 >> 3 == 0b11110 {
                if i + 3 >= bytes.len() {
                    break;
                }
                let cp = (((b0 & 0x07) as u32) << 18)
                    | (((bytes[i + 1] & 0x3f) as u32) << 12)
                    | (((bytes[i + 2] & 0x3f) as u32) << 6)
                    | (bytes[i + 3] & 0x3f) as u32;
                let cp = cp.saturating_sub(0x10000);
                units.push(0xD800 + ((cp >> 10) as u16));
                units.push(0xDC00 + ((cp & 0x3ff) as u16));
                i += 4;
            } else {
                i += 1;
            }
        }
        units
    }

    fn push_wtf8_3(out: &mut Vec<u8>, cp: u32) {
        out.push(0xE0 | ((cp >> 12) as u8 & 0x0f));
        out.push(0x80 | ((cp >> 6) as u8 & 0x3f));
        out.push(0x80 | (cp as u8 & 0x3f));
    }

    fn push_utf8(out: &mut Vec<u8>, cp: u32) {
        if cp < 0x80 {
            out.push(cp as u8);
        } else if cp < 0x800 {
            out.push(0xC0 | (cp >> 6) as u8);
            out.push(0x80 | (cp & 0x3f) as u8);
        } else if cp < 0x10000 {
            out.push(0xE0 | (cp >> 12) as u8);
            out.push(0x80 | ((cp >> 6) as u8 & 0x3f));
            out.push(0x80 | (cp as u8 & 0x3f));
        } else {
            out.push(0xF0 | (cp >> 18) as u8);
            out.push(0x80 | ((cp >> 12) as u8 & 0x3f));
            out.push(0x80 | ((cp >> 6) as u8 & 0x3f));
            out.push(0x80 | (cp as u8 & 0x3f));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn byte_exact_round_trip() {
        let codec = GenericPathCodec;
        let raw = RawPath::new(vec![0xff, 0xfe, b'/', 0x80, b'a']);
        let native = codec.to_native(&raw);
        assert_eq!(codec.to_raw(&native), raw);
    }
}
