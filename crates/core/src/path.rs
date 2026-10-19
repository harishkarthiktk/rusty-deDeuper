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
    ///
    /// Drops a leading `\\?\` before a drive letter (`\\?\C:\x` shows as `C:\x`).
    /// `\\?\UNC\` and `\\?\Volume{...}` keep their prefix.
    pub fn to_display(&self) -> String {
        let lossy: Cow<'_, str> = String::from_utf8_lossy(strip_verbatim_drive(&self.0));
        lossy.nfc().collect()
    }

    /// Lossless where possible, terminal-safe always. For human output and messages only.
    /// Control characters, bidi overrides and invalid UTF-8 are shown as escapes, so a
    /// filename cannot move the cursor, rewrite earlier lines or reorder text. Not
    /// normalized, so two names that differ only in Unicode form stay distinguishable.
    /// A backslash is not escaped (Windows paths use it), so a literal `\x1b` in a name
    /// and an escaped ESC look the same here; JSON is the machine contract.
    pub fn to_terminal(&self) -> Cow<'_, str> {
        escape_terminal(simplify_verbatim(&self.0))
    }

    /// JSON fields for this path: exact `path`, plus `path_bytes` (lowercase hex of the
    /// raw bytes) only when the bytes are not valid UTF-8 and `path` is therefore lossy.
    pub fn json_fields(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        let bytes = simplify_verbatim(&self.0);
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                obj.insert("path".into(), s.into());
            }
            Err(_) => {
                obj.insert(
                    "path".into(),
                    String::from_utf8_lossy(bytes).into_owned().into(),
                );
                obj.insert("path_bytes".into(), to_hex(&self.0).into());
            }
        }
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

/// Strips `\\?\` only when a drive designator (`X:`) follows. Other verbatim forms
/// (`\\?\UNC\...`, `\\?\Volume{...}`) have no shorter equivalent and stay as they are.
pub(crate) fn strip_verbatim_drive(bytes: &[u8]) -> &[u8] {
    match bytes {
        [b'\\', b'\\', b'?', b'\\', drive, b':', ..] if drive.is_ascii_alphabetic() => &bytes[4..],
        _ => bytes,
    }
}

/// True for scalars that must never reach a terminal raw.
fn needs_escape(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
        | '\u{061c}' | '\u{200e}' | '\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        | '\u{2028}' | '\u{2029}')
}

pub(crate) fn escape_terminal(bytes: &[u8]) -> Cow<'_, str> {
    use std::fmt::Write as _;
    // Fast path: valid UTF-8 with nothing to escape borrows.
    if let Ok(s) = std::str::from_utf8(bytes) {
        if !s.chars().any(needs_escape) {
            return Cow::Borrowed(s);
        }
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                c if needs_escape(c) => {
                    let _ = write!(out, "\\u{{{:x}}}", c as u32); // writing to String cannot fail
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    Cow::Owned(out)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}"); // writing to String cannot fail
            s
        })
}

/// Strips `\\?\` before a drive letter only when the short form names the same file
/// under Win32 normalization: under 260 bytes, no component ending in `.` or a space, no
/// reserved device name. Otherwise the prefix is kept.
pub(crate) fn simplify_verbatim(bytes: &[u8]) -> &[u8] {
    let [b'\\', b'\\', b'?', b'\\', d, b':', b'\\', ..] = bytes else {
        return bytes;
    };
    if !d.is_ascii_alphabetic() || bytes.len() - 4 >= 260 {
        return bytes;
    }
    let short = &bytes[4..];
    let safe = short[3..]
        .split(|&b| b == b'\\')
        .all(|comp| !comp.ends_with(b".") && !comp.ends_with(b" ") && !is_reserved_dos_name(comp));
    if safe {
        short
    } else {
        bytes
    }
}

/// Reserved DOS device name: the stem before the first `.`, ASCII case-insensitive,
/// is `CON PRN AUX NUL COM0-9 LPT0-9`.
fn is_reserved_dos_name(comp: &[u8]) -> bool {
    let stem = comp.split(|&b| b == b'.').next().unwrap_or(comp);
    let upper = stem.to_ascii_uppercase();
    match upper.as_slice() {
        b"CON" | b"PRN" | b"AUX" | b"NUL" => true,
        [b'C', b'O', b'M', d] | [b'L', b'P', b'T', d] => d.is_ascii_digit(),
        _ => false,
    }
}

/// Bytes of `path` below `mount`, or `None` when `path` is not under `mount`.
/// Strips `\\?\` before a drive letter on both sides. For Windows-style paths
/// (separator `\` or a drive designator) it compares ASCII case-insensitively. It
/// requires a component boundary, so `/data2` is not under `/data`.
pub(crate) fn strip_mount<'a>(path: &'a [u8], mount: &[u8]) -> Option<&'a [u8]> {
    let p = strip_verbatim_drive(path);
    let m = strip_verbatim_drive(mount);
    if m.is_empty() {
        return Some(p);
    }
    if p.len() < m.len() {
        return None;
    }
    let windows =
        separator_for(m) == b'\\' || matches!(m, [d, b':', ..] if d.is_ascii_alphabetic());
    let head = &p[..m.len()];
    let same = if windows {
        head.eq_ignore_ascii_case(m)
    } else {
        head == m
    };
    if !same {
        return None;
    }
    let rest = &p[m.len()..];
    let mount_ends_sep = matches!(m.last(), Some(b'/' | b'\\'));
    match rest.first() {
        None => Some(rest),
        Some(b'/') => Some(rest),
        Some(b'\\') if windows => Some(rest),
        Some(_) if mount_ends_sep => Some(rest),
        Some(_) => None,
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
