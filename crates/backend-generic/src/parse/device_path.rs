/// Device path that opens the whole volume, from a volume root as `GetVolumePathNameW`
/// reports it. `X:\` maps to `\\.\X:` and `\\?\Volume{GUID}\` maps to `\\?\Volume{GUID}`.
/// Anything else (UNC shares, folder mounts) is `None`, because opening it would address
/// a directory on a volume rather than the volume itself.
pub(crate) fn volume_device_path(root: &str) -> Option<String> {
    const VOLUME_GUID_PREFIX: &str = r"\\?\Volume{";
    match root.as_bytes() {
        [letter, b':', b'\\'] if letter.is_ascii_alphabetic() => {
            Some(format!(r"\\.\{}:", char::from(*letter)))
        }
        _ if root.starts_with(VOLUME_GUID_PREFIX) && root.ends_with('\\') => {
            Some(root.trim_end_matches('\\').to_owned())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::volume_device_path;

    #[test]
    fn volume_device_path_should_map_drive_root_to_device_when_drive_letter() {
        let device = volume_device_path("C:\\");
        assert_eq!(
            device.as_deref(),
            Some(r"\\.\C:"),
            "drive root C:\\ should map to the volume device \\\\.\\C:, got {device:?}"
        );
    }

    #[test]
    fn volume_device_path_should_strip_trailing_backslash_when_volume_guid() {
        let device = volume_device_path(r"\\?\Volume{1234-abcd}\");
        assert_eq!(
            device.as_deref(),
            Some(r"\\?\Volume{1234-abcd}"),
            "a volume GUID root should open without its trailing backslash, got {device:?}"
        );
    }

    #[test]
    fn volume_device_path_should_return_none_when_unc_share() {
        let device = volume_device_path(r"\\server\share\");
        assert_eq!(
            device, None,
            "a network share has no local volume device and should give None, got {device:?}"
        );
    }

    #[test]
    fn volume_device_path_should_return_none_when_folder_mount() {
        let device = volume_device_path(r"C:\mnt\data\");
        assert_eq!(
            device, None,
            "a folder mount addresses a directory, not the volume, and should give None, got {device:?}"
        );
    }
}
