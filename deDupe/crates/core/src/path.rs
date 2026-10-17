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

    /// Join a child component with the separator the existing path already uses.
    /// Does not duplicate an existing trailing separator.
    pub fn join_component(&self, name: &[u8]) -> RawPath {
        if self.0.is_empty() {
            return RawPath(name.to_vec());
        }
        let mut out = self.0.clone();
        if !out.ends_with(b"/") && !out.ends_with(b"\\") {
            // Walker paths are native: a hard-coded '/' would corrupt C:\Users\me.
            out.push(separator_for(&self.0));
        }
        out.extend_from_slice(name);
        RawPath(out)
    }

    /// The final component (bytes after the last separator), or the whole path if none.
    /// The separator follows the path's own rule (see `separator_for`), so Windows
    /// paths split on `\`.
    pub fn component(&self) -> &[u8] {
        let sep = separator_for(&self.0);
        match self.0.iter().rposition(|&b| b == sep) {
            Some(i) => &self.0[i + 1..],
            None => &self.0,
        }
    }

    /// The parent path, or `None` at a root (`/`, `C:\`) or when there is no separator.
    ///
    /// A trailing separator does not start a new component, so `/a/` has parent `/`.
    /// A drive root keeps its separator (`C:\a` has parent `C:\`), because `C:` alone
    /// names the current directory on that drive, not its root.
    pub fn parent(&self) -> Option<RawPath> {
        let sep = separator_for(&self.0);
        let end = self.0.iter().rposition(|&b| b != sep)? + 1;
        let i = self.0[..end].iter().rposition(|&b| b == sep)?;
        if i == 0 {
            return Some(RawPath(vec![sep]));
        }
        if i == 2 && self.0[1] == b':' {
            return Some(RawPath(self.0[..=i].to_vec()));
        }
        Some(RawPath(self.0[..i].to_vec()))
    }
}

/// Separator used to extend a path whose bytes are `bytes`.
///
/// A path counts as Windows only when it contains `\` and no `/`. Mixed paths keep `/`,
/// and an empty path defaults to `/`. Shared with the DB reconstruction so both agree.
pub(crate) fn separator_for(bytes: &[u8]) -> u8 {
    if bytes.contains(&b'\\') && !bytes.contains(&b'/') {
        b'\\'
    } else {
        b'/'
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
    fn join_component_should_use_backslash_when_path_uses_backslashes() {
        let dir = RawPath::new(br"C:\Users".to_vec());
        assert_eq!(
            dir.join_component(b"me"),
            RawPath::new(br"C:\Users\me".to_vec()),
            "a backslash path must be extended with a backslash"
        );
    }

    #[test]
    fn join_component_should_use_slash_for_unix_paths() {
        let dir = RawPath::new(b"/home/me".to_vec());
        assert_eq!(
            dir.join_component(b"Pictures"),
            RawPath::new(b"/home/me/Pictures".to_vec()),
            "a Unix path must be extended with a slash"
        );
    }

    #[test]
    fn join_component_should_not_duplicate_trailing_backslash() {
        let root = RawPath::new(br"C:\".to_vec());
        assert_eq!(
            root.join_component(b"Users"),
            RawPath::new(br"C:\Users".to_vec()),
            "a trailing backslash must not get a second separator"
        );
    }

    #[test]
    fn join_component_should_keep_building_native_chain_for_deep_windows_root() {
        let path = RawPath::new(br"C:\".to_vec())
            .join_component(b"Users")
            .join_component(b"me")
            .join_component(b"Pictures");
        assert_eq!(
            path,
            RawPath::new(br"C:\Users\me\Pictures".to_vec()),
            "each level below a drive root must keep the backslash separator"
        );
    }

    #[test]
    fn separator_for_should_return_backslash_when_bytes_have_only_backslashes() {
        assert_eq!(
            separator_for(br"C:\a"),
            b'\\',
            "a path with only backslashes must use a backslash"
        );
    }

    #[test]
    fn separator_for_should_return_slash_when_bytes_mix_separators() {
        assert_eq!(
            separator_for(br"C:\a/b"),
            b'/',
            "a path containing both separators must use a slash"
        );
    }

    #[test]
    fn separator_for_should_return_slash_when_bytes_are_empty() {
        assert_eq!(
            separator_for(b""),
            b'/',
            "an empty path must default to a slash"
        );
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

    #[test]
    fn component_should_return_last_backslash_part_for_windows_path() {
        let p = RawPath::new(br"C:\a\b".to_vec());
        assert_eq!(
            p.component(),
            b"b",
            "the component of C:\\a\\b must be the part after the last backslash"
        );
    }

    #[test]
    fn parent_should_keep_drive_root() {
        let p = RawPath::new(br"C:\a".to_vec());
        assert_eq!(
            p.parent(),
            Some(RawPath::new(br"C:\".to_vec())),
            "the parent of C:\\a must keep the drive root's trailing backslash"
        );
    }

    #[test]
    fn parent_should_return_none_for_drive_root() {
        let p = RawPath::new(br"C:\".to_vec());
        assert_eq!(p.parent(), None, "C:\\ is a root and must have no parent");
    }

    #[test]
    fn parent_should_walk_up_windows_chain() {
        let chain: Vec<RawPath> =
            std::iter::successors(Some(RawPath::new(br"C:\a\b\c".to_vec())), RawPath::parent)
                .collect();
        assert_eq!(
            chain,
            vec![
                RawPath::new(br"C:\a\b\c".to_vec()),
                RawPath::new(br"C:\a\b".to_vec()),
                RawPath::new(br"C:\a".to_vec()),
                RawPath::new(br"C:\".to_vec()),
            ],
            "walking parent() from C:\\a\\b\\c must visit each level down to the drive root"
        );
    }

    #[test]
    fn parent_and_component_should_keep_unix_behavior() {
        assert_eq!(
            RawPath::new(b"/a/b".to_vec()).parent(),
            Some(RawPath::new(b"/a".to_vec())),
            "the parent of /a/b must be /a"
        );
        assert_eq!(
            RawPath::new(b"/a".to_vec()).parent(),
            Some(RawPath::new(b"/".to_vec())),
            "the parent of /a must be /"
        );
        assert_eq!(
            RawPath::new(b"/".to_vec()).parent(),
            None,
            "/ is a root and must have no parent"
        );
        assert_eq!(
            RawPath::new(b"abc".to_vec()).parent(),
            None,
            "a path with no separator must have no parent"
        );
        assert_eq!(
            RawPath::new(b"abc".to_vec()).component(),
            b"abc",
            "a path with no separator must be its own component"
        );
    }
}
