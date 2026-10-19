//! Native path <-> [`RawPath`] conversion.
//!
//! Unix: native bytes. Windows: a small WTF-8 encoder/decoder over `encode_wide()`
//! (std explicitly documents `as_encoded_bytes()` as unspecified, not for storage).

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use dedupe_core::path::RawPath;
use dedupe_core::traits::PathCodec;

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
    let units = match wtf8::decode(&path.0) {
        Ok(units) => units,
        Err(offset) => {
            tracing::warn!(
                offset,
                len = path.0.len(),
                "malformed WTF-8 in stored path; substituting U+FFFD"
            );
            wtf8::decode_lossy(&path.0)
        }
    };
    OsString::from_wide(&units)
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
#[cfg(any(windows, test))]
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

    /// Strictly decode WTF-8 bytes to UTF-16 units.
    ///
    /// Rejects truncated sequences, bad continuation bytes, overlong forms and
    /// code points above U+10FFFF. Returns the byte offset of the first byte of
    /// the first malformed sequence on error. Unpaired surrogates (3-byte forms)
    /// are accepted.
    pub fn decode(bytes: &[u8]) -> Result<Vec<u16>, usize> {
        let mut units = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            let (cp, len) = next_code_point(bytes, i)?;
            push_units(&mut units, cp);
            i += len;
        }
        Ok(units)
    }

    /// Lossy WTF-8 decode: each byte of a malformed sequence becomes U+FFFD and
    /// decoding resumes at the next byte. Valid input matches [`decode`].
    pub fn decode_lossy(bytes: &[u8]) -> Vec<u16> {
        let mut units = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            match next_code_point(bytes, i) {
                Ok((cp, len)) => {
                    push_units(&mut units, cp);
                    i += len;
                }
                Err(_) => {
                    units.push(0xFFFD);
                    i += 1;
                }
            }
        }
        units
    }

    /// Decode the sequence starting at `i`, returning (code point, byte length).
    /// On a malformed sequence returns `Err(i)`.
    fn next_code_point(bytes: &[u8], i: usize) -> Result<(u32, usize), usize> {
        let b0 = bytes[i];
        if b0 < 0x80 {
            return Ok((b0 as u32, 1));
        }
        let (len, min, mut cp) = match b0 {
            0xC2..=0xDF => (2, 0x80, (b0 & 0x1f) as u32),
            0xE0..=0xEF => (3, 0x800, (b0 & 0x0f) as u32),
            0xF0..=0xF4 => (4, 0x1_0000, (b0 & 0x07) as u32),
            // Continuation bytes, 0xC0/0xC1 (always overlong), and 0xF5..=0xFF.
            _ => return Err(i),
        };
        if i + len > bytes.len() {
            return Err(i);
        }
        for &b in &bytes[i + 1..i + len] {
            if b & 0xC0 != 0x80 {
                return Err(i);
            }
            cp = (cp << 6) | (b & 0x3f) as u32;
        }
        if cp < min || cp > 0x10_FFFF {
            return Err(i);
        }
        Ok((cp, len))
    }

    /// Append UTF-16 units for one code point (a surrogate pair above the BMP).
    fn push_units(units: &mut Vec<u16>, cp: u32) {
        if cp >= 0x1_0000 {
            let v = cp - 0x1_0000;
            units.push(0xD800 + (v >> 10) as u16);
            units.push(0xDC00 + (v & 0x3ff) as u16);
        } else {
            units.push(cp as u16);
        }
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

#[cfg(test)]
mod wtf8_tests {
    use super::wtf8;

    fn round_trip(units: &[u16]) {
        let bytes = wtf8::encode(units.iter().copied());
        assert_eq!(wtf8::decode(&bytes), Ok(units.to_vec()));
    }

    #[test]
    fn ascii_bmp_and_pairs_round_trip() {
        let ascii: Vec<u16> = b"hello".iter().map(|&b| b as u16).collect();
        round_trip(&ascii);
        round_trip(&[0x00E9]); // BMP
        round_trip(&[0x4E2D]); // CJK
        round_trip(&[0xD83D, 0xDE00]); // surrogate pair
    }

    #[test]
    fn unpaired_surrogates_preserved() {
        let hi = &[0xD800];
        let lo = &[0xDC00];
        let bytes = wtf8::encode(hi.iter().copied());
        assert_eq!(bytes.len(), 3); // 3-byte WTF-8, not valid UTF-8
        assert_eq!(wtf8::decode(&bytes), Ok(hi.to_vec()));
        assert_eq!(
            wtf8::decode(&wtf8::encode(lo.iter().copied())),
            Ok(lo.to_vec())
        );
    }

    #[test]
    fn rejects_truncated_three_byte() {
        // U+20AC (E2 82 AC) cut after two bytes.
        assert_eq!(wtf8::decode(&[0xE2, 0x82]), Err(0));
    }

    #[test]
    fn rejects_bad_continuation() {
        // Second byte is ASCII '(' instead of a continuation byte.
        assert_eq!(wtf8::decode(&[0xE2, 0x28, 0xAC]), Err(0));
    }

    #[test]
    fn rejects_overlong_four_byte() {
        // F0 80 80 80 encodes U+0000 in four bytes.
        assert_eq!(wtf8::decode(&[0xF0, 0x80, 0x80, 0x80]), Err(0));
    }

    #[test]
    fn rejects_above_max_code_point() {
        // F4 90 80 80 encodes U+110000.
        assert_eq!(wtf8::decode(&[b'a', 0xF4, 0x90, 0x80, 0x80]), Err(1));
    }

    #[test]
    fn lossy_replaces_malformed_bytes() {
        // Valid 'a', then a truncated 3-byte lead, then 'b'.
        let units = wtf8::decode_lossy(&[b'a', 0xE2, 0x82, b'b']);
        assert_eq!(units, vec![b'a' as u16, 0xFFFD, 0xFFFD, b'b' as u16]);
    }

    #[test]
    fn round_trip_should_preserve_lone_high_surrogate_before_pair() {
        // A lone high surrogate followed by a high+low pair must not be merged.
        let units = [0xD800u16, 0xD83D, 0xDE00];
        let bytes = wtf8::encode(units.iter().copied());
        assert_eq!(wtf8::decode(&bytes), Ok(units.to_vec()));
    }

    #[test]
    fn decode_should_accept_encoded_surrogate_form() {
        // WTF-8 3-byte forms: U+D800 is ED A0 80, U+DC00 is ED B0 80.
        assert_eq!(wtf8::decode(&[0xED, 0xA0, 0x80]), Ok(vec![0xD800]));
        assert_eq!(wtf8::decode(&[0xED, 0xB0, 0x80]), Ok(vec![0xDC00]));
        assert_eq!(
            wtf8::encode([0xD800u16].into_iter()),
            vec![0xED, 0xA0, 0x80]
        );
    }

    #[test]
    fn decode_should_return_empty_when_input_empty() {
        assert_eq!(wtf8::decode(&[]), Ok(Vec::<u16>::new()));
        assert!(wtf8::decode_lossy(&[]).is_empty());
        assert!(wtf8::encode(std::iter::empty::<u16>()).is_empty());
    }

    #[test]
    fn decode_lossy_should_resume_after_bad_byte() {
        // 0xFF is never valid. The decoder must emit U+FFFD and continue with 'a'.
        assert_eq!(wtf8::decode_lossy(&[0xFF, b'a']), vec![0xFFFD, b'a' as u16]);
        // Bad continuation: E2 fails, '(' is valid, lone AC is a stray continuation byte.
        assert_eq!(
            wtf8::decode_lossy(&[0xE2, 0x28, 0xAC]),
            vec![0xFFFD, 0x28, 0xFFFD]
        );
    }
}
