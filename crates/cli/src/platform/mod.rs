//! The one place the CLI picks platform backend types.
//!
//! Windows builds get the NTFS-aware enumerator and the open-by-id reader; every other
//! build gets the generic pieces.

use dedupe_backend_generic::{GenericPathCodec, GenericVolumeIdentity};
use dedupe_core::storage_profile::{Profile, StorageKind};
use dedupe_core::traits::{ContentReader, Enumerator};

#[cfg(not(windows))]
mod generic;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
pub use generic::{make_backend, make_reader, Extras};
#[cfg(windows)]
pub use windows::{make_backend, make_reader, Extras};

/// What each platform's `Extras` must provide after a scan. Static dispatch via the alias above.
pub trait ScanExtras {
    /// Value for `scan_runs.enumerator`.
    fn enumerator_label(&self, no_mft: bool) -> &'static str;
    /// Notices queued by the enumerator during the scan. Always empty outside Windows.
    fn drain_notices(&self) -> Vec<String>;
}

/// The backend pieces one scan uses. `Services` borrows them for the length of the scan.
pub struct Backend {
    pub enumerator: Box<dyn Enumerator>,
    pub reader: Box<dyn ContentReader>,
    pub codec: GenericPathCodec,
    pub volume: GenericVolumeIdentity,
    /// Platform state read after the scan, such as why the walker was used.
    pub extras: Extras,
}

/// Owner-only permissions for a secret file (the mask vault). No-op outside Unix, where
/// the file inherits the folder's ACL.
#[cfg(unix)]
pub fn restrict_perms(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
pub fn restrict_perms(_path: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}

/// Overlapped read slots for a profile. SSD gets 4, so several reads are in flight per file.
/// HDD and unknown storage get 1: parallel reads make a seek-bound disk jump between files,
/// which is the cost the HDD policy exists to avoid.
// Lives here, not in windows.rs, so its tests also run on Linux.
#[cfg_attr(
    all(not(windows), not(test)),
    allow(dead_code, reason = "only the Windows reader uses it")
)]
fn read_ahead_for(profile: &Profile) -> usize {
    match profile.kind {
        StorageKind::Ssd => 4,
        StorageKind::Hdd | StorageKind::Unknown => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dedupe_core::storage_profile::{self, ProfileChoice};

    fn resolved(choice: ProfileChoice, rotational: Option<bool>) -> Profile {
        storage_profile::resolve(choice, rotational, None, None)
            .expect("resolving a profile without overrides should succeed")
    }

    #[test]
    fn read_ahead_for_should_return_four_when_profile_is_ssd() {
        let profile = resolved(ProfileChoice::Ssd, None);
        assert_eq!(
            read_ahead_for(&profile),
            4,
            "an SSD profile should keep four overlapped reads in flight, got {profile:?}"
        );
    }

    #[test]
    fn read_ahead_for_should_return_one_when_profile_is_hdd() {
        let profile = resolved(ProfileChoice::Hdd, None);
        assert_eq!(
            read_ahead_for(&profile),
            1,
            "an HDD profile should read one slot at a time, got {profile:?}"
        );
    }

    #[test]
    fn read_ahead_for_should_return_one_when_profile_is_unknown() {
        let profile = resolved(ProfileChoice::Auto, None);
        assert_eq!(
            read_ahead_for(&profile),
            1,
            "an unknown profile should stay conservative with one slot, got {profile:?}"
        );
    }
}
