//! UTF-16 volume-name helpers (`GetVolumePathNameW`, `GetVolumeInformationW`).

/// Spell `root` (from `GetVolumePathNameW`) the way `input` is spelled: verbatim (`\\?\`,
/// `\\?\UNC\`) or not, so `relative_components` can strip it as a byte prefix.
pub(crate) fn match_verbatim(input: &[u16], root: &[u16]) -> Vec<u16> {
    let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let (vb, unc_vb, unc) = (w(r"\\?\"), w(r"\\?\UNC\"), w(r"\\"));
    let in_vb = input.starts_with(&vb);
    let root_vb = root.starts_with(&vb);
    match (in_vb, root_vb) {
        (true, false) if root.starts_with(&unc) => [&unc_vb[..], &root[2..]].concat(),
        (true, false) => [&vb[..], root].concat(),
        (false, true) if root.starts_with(&unc_vb) => [&unc[..], &root[unc_vb.len()..]].concat(),
        (false, true) if !root[vb.len()..].starts_with(&w("Volume{")) => root[vb.len()..].to_vec(),
        _ => root.to_vec(),
    }
}

/// File-system name from a `GetVolumeInformationW` buffer, up to the first NUL.
pub(crate) fn fs_type_from_wide(name: &[u16]) -> dedupe_core::model::FsType {
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    dedupe_core::model::FsType::parse(&String::from_utf16_lossy(&name[..len]))
}

#[cfg(test)]
mod match_verbatim_tests {
    use super::match_verbatim;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().collect()
    }

    #[test]
    fn match_verbatim_should_spell_root_like_input_when_forms_differ() {
        let cases = [
            // Plain drive input, plain root: unchanged.
            (r"C:\dir\f", r"C:\", r"C:\"),
            // Verbatim input, plain root: add the verbatim prefix.
            (r"\\?\C:\dir\f", r"C:\", r"\\?\C:\"),
            // Verbatim input, plain UNC root: verbatim UNC form.
            (
                r"\\?\UNC\srv\share\f",
                r"\\srv\share\",
                r"\\?\UNC\srv\share\",
            ),
            // Plain UNC input, verbatim UNC root: strip to plain UNC.
            (r"\\srv\share\f", r"\\?\UNC\srv\share\", r"\\srv\share\"),
            // Plain input, verbatim drive root: strip the prefix.
            (r"C:\dir\f", r"\\?\C:\", r"C:\"),
            // Volume GUID root is kept verbatim for a plain input.
            (r"C:\dir\f", r"\\?\Volume{1234}\", r"\\?\Volume{1234}\"),
            // Already matching verbatim input and root: unchanged.
            (r"\\?\C:\dir\f", r"\\?\C:\", r"\\?\C:\"),
        ];
        for (input, root, want) in cases {
            assert_eq!(
                match_verbatim(&wide(input), &wide(root)),
                wide(want),
                "input {input:?}, root {root:?}"
            );
        }
    }
}

#[cfg(test)]
mod fs_type_tests {
    use super::fs_type_from_wide;
    use dedupe_core::model::FsType;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    #[test]
    fn fs_type_from_wide_should_return_ntfs_when_name_is_ntfs() {
        assert_eq!(fs_type_from_wide(&wide("NTFS")), FsType::Ntfs, "NTFS name");
    }

    #[test]
    fn fs_type_from_wide_should_return_fat32_when_name_is_fat32() {
        assert_eq!(
            fs_type_from_wide(&wide("FAT32")),
            FsType::Fat32,
            "FAT32 name"
        );
    }

    #[test]
    fn fs_type_from_wide_should_return_exfat_when_name_is_exfat() {
        assert_eq!(
            fs_type_from_wide(&wide("exFAT")),
            FsType::Exfat,
            "exFAT name"
        );
    }

    #[test]
    fn fs_type_from_wide_should_return_other_when_name_is_refs_or_empty() {
        assert_eq!(
            fs_type_from_wide(&wide("ReFS")),
            FsType::Other,
            "ReFS has no variant"
        );
        assert_eq!(fs_type_from_wide(&[]), FsType::Other, "empty buffer");
    }

    #[test]
    fn fs_type_from_wide_should_match_case_insensitively_when_name_is_lowercase() {
        assert_eq!(fs_type_from_wide(&wide("ntfs")), FsType::Ntfs, "lowercase");
    }

    #[test]
    fn fs_type_from_wide_should_stop_at_first_nul_when_buffer_has_trailing_data() {
        let mut buf = wide("NTFS");
        buf.extend(wide("FAT32"));
        assert_eq!(fs_type_from_wide(&buf), FsType::Ntfs, "embedded NUL");
    }

    #[test]
    fn fs_type_from_wide_should_use_full_length_when_buffer_has_no_nul() {
        let buf: Vec<u16> = "exFAT".encode_utf16().collect();
        assert_eq!(fs_type_from_wide(&buf), FsType::Exfat, "no terminator");
    }

    #[test]
    fn fs_type_from_wide_should_return_other_when_name_has_unpaired_surrogate() {
        let buf = [0xD800_u16, 0x0041, 0];
        assert_eq!(fs_type_from_wide(&buf), FsType::Other, "lossy UTF-16");
    }
}
