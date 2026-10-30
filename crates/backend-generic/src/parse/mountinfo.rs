//! `/proc/self/mountinfo` parsing used for volume identity.

/// Fields 1-3 never contain spaces (the mount point is escaped), so splitting on
/// spaces is safe up to the `-` separator.
pub(crate) fn parse_mountinfo(text: &str, want: &str) -> Option<(String, String)> {
    text.lines().find_map(|line| {
        let (pre, post) = line.split_once(" - ")?;
        if pre.split(' ').nth(2)? != want {
            return None;
        }
        let mut f = post.split(' ');
        let fstype = f.next()?.to_owned();
        let source = unescape_octal(f.next()?);
        Some((fstype, source))
    })
}

/// Decodes the kernel's `\040`-style octal escapes (space, tab, newline, backslash).
pub(crate) fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let oct = b
            .get(i + 1..i + 4)
            .filter(|d| d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match (b[i], oct) {
            (b'\\', Some(d)) => {
                out.push((d[0] - b'0') << 6 | (d[1] - b'0') << 3 | (d[2] - b'0'));
                i += 4;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::{parse_mountinfo, unescape_octal};

    #[test]
    fn parse_mountinfo_should_return_fstype_and_source_when_mount_matches_dev() {
        let text = "\
22 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
36 22 0:45 / /mnt/my\\040share rw,nosuid - fuse.sshfs user@host:/srv rw
40 22 0:50 / /mnt/plain rw - nfs4 server:/export rw
";
        assert_eq!(
            parse_mountinfo(text, "0:45"),
            Some(("fuse.sshfs".to_owned(), "user@host:/srv".to_owned()))
        );
        assert_eq!(
            parse_mountinfo(text, "8:1"),
            Some(("ext4".to_owned(), "/dev/sda1".to_owned()))
        );
        assert_eq!(parse_mountinfo(text, "9:9"), None, "unknown dev");
    }

    #[test]
    fn parse_mountinfo_should_skip_optional_fields_when_present() {
        let text = "51 22 0:60 / /x rw shared:3 master:2 - cifs //srv/share rw\n";
        assert_eq!(
            parse_mountinfo(text, "0:60"),
            Some(("cifs".to_owned(), "//srv/share".to_owned()))
        );
    }

    #[test]
    fn unescape_octal_should_decode_space_tab_newline_and_backslash() {
        assert_eq!(unescape_octal(r"a\040b"), "a b");
        assert_eq!(unescape_octal(r"a\011b"), "a\tb");
        assert_eq!(unescape_octal(r"a\012b"), "a\nb");
        assert_eq!(unescape_octal(r"a\134b"), "a\\b");
        assert_eq!(
            unescape_octal(r"a\99b"),
            r"a\99b",
            "non-octal digits stay literal"
        );
    }

    #[test]
    fn parse_mountinfo_should_return_none_when_line_is_malformed() {
        let no_separator = "1 2 0:70 / /x rw ext4 /dev/x rw\n";
        assert_eq!(parse_mountinfo(no_separator, "0:70"), None, "no ' - '");
        let no_source = "5 1 0:71 / /y rw - ext4\n";
        assert_eq!(parse_mountinfo(no_source, "0:71"), None, "missing source");
    }
}
