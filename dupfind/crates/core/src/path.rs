//! Lossless path bytes.
//!
//! `RawPath` holds bytes only. Conversion to and from the native `OsStr` lives in
//! each backend's `PathCodec` impl. Nothing is normalized on write.

use std::borrow::Cow;

use unicode_normalization::UnicodeNormalization;

/// Raw, lossless path bytes. Linux: native bytes. Windows: WTF-8. macOS: bytes as
/// returned by the OS. Never normalized on write.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RawPath(pub Vec<u8>);

impl RawPath {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Self {
        RawPath(bytes.into())
    }

    pub fn empty() -> Self {
        RawPath(Vec::new())
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Lossy, NFC-normalized string for display only. Never feed back into the DB.
    pub fn to_display(&self) -> String {
        let lossy: Cow<'_, str> = String::from_utf8_lossy(&self.0);
        lossy.nfc().collect()
    }

    /// Join a child component. Does not duplicate an existing trailing separator.
    pub fn join_component(&self, name: &[u8]) -> RawPath {
        if self.0.is_empty() {
            return RawPath(name.to_vec());
        }
        let mut out = self.0.clone();
        if !out.ends_with(b"/") && !out.ends_with(b"\\") {
            out.push(b'/');
        }
        out.extend_from_slice(name);
        RawPath(out)
    }

    /// The final component (bytes after the last `/`), or the whole path if none.
    pub fn component(&self) -> &[u8] {
        match self.0.iter().rposition(|&b| b == b'/') {
            Some(i) => &self.0[i + 1..],
            None => &self.0,
        }
    }

    /// The parent path, or `None` at the root.
    pub fn parent(&self) -> Option<RawPath> {
        self.0.iter().rposition(|&b| b == b'/').map(|i| {
            if i == 0 {
                RawPath(vec![b'/'])
            } else {
                RawPath(self.0[..i].to_vec())
            }
        })
    }
}

impl From<Vec<u8>> for RawPath {
    fn from(v: Vec<u8>) -> Self {
        RawPath(v)
    }
}

impl AsRef<[u8]> for RawPath {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_normalizes_nfc() {
        // "e" + combining acute (NFD) becomes single precomposed scalar (NFC).
        let nfd = RawPath::new("e\u{0301}".as_bytes().to_vec());
        assert_eq!(nfd.to_display(), "\u{00e9}");
    }

    #[test]
    fn component_and_parent() {
        let p = RawPath::new(b"/a/b/c".to_vec());
        assert_eq!(p.component(), b"c");
        assert_eq!(p.parent().unwrap(), RawPath::new(b"/a/b".to_vec()));
        assert_eq!(
            RawPath::new(b"/a".to_vec()).parent().unwrap().as_bytes(),
            b"/"
        );
    }
}
