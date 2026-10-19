//! Whether the NTFS `$MFT` fast path can run for a scan root, and the rule that decides it.
//!
//! [`choose_mode`] is pure and tested on every OS. [`probe_capabilities`] reads the machine and is
//! Windows-only. It never fails: a probe that errors reports that capability as absent.

use std::fmt;

/// What the machine offers for the `$MFT` path. Each field is measured on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The process token is elevated.
    pub admin: bool,
    /// The volume holding the scan root reports the file-system name `NTFS`.
    pub ntfs: bool,
    /// The raw volume `\\.\X:` opened read-only in a probe.
    pub raw_volume_openable: bool,
}

/// The enumeration strategy chosen for one scan root.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Read the NTFS `$MFT` directly.
    Mft,
    /// Use the standard directory walker, for the given reason.
    Fallback(FallbackReason),
}

/// Why the `$MFT` fast path was not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackReason {
    /// The user passed `--no-mft`.
    Disabled,
    /// The process is not elevated, so the raw volume cannot be read.
    NotElevated,
    /// The volume is a different file system.
    NotNtfs,
    /// The raw volume could not be opened for another reason.
    RawVolumeUnavailable,
}

impl fmt::Display for FallbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => f.write_str("MFT fast path disabled by --no-mft."),
            Self::NotElevated => {
                f.write_str("not running as administrator, so the NTFS MFT fast path is off.")
            }
            Self::NotNtfs => {
                f.write_str("the volume is not NTFS, so the NTFS MFT fast path is off.")
            }
            Self::RawVolumeUnavailable => {
                f.write_str("the raw volume could not be opened, so the NTFS MFT fast path is off.")
            }
        }
    }
}

/// Chooses the strategy for one root.
///
/// `--no-mft` wins over everything. The other checks run in the order a user can act on them:
/// elevation first, because it is the one the cli hint names. The first failing check decides the
/// reason, so the message always names a cause the user can fix or accept.
pub fn choose_mode(caps: &Capabilities, no_mft: bool) -> Mode {
    if no_mft {
        return Mode::Fallback(FallbackReason::Disabled);
    }
    if !caps.admin {
        return Mode::Fallback(FallbackReason::NotElevated);
    }
    if !caps.ntfs {
        return Mode::Fallback(FallbackReason::NotNtfs);
    }
    if !caps.raw_volume_openable {
        return Mode::Fallback(FallbackReason::RawVolumeUnavailable);
    }
    Mode::Mft
}

#[cfg(windows)]
pub use win32::probe_capabilities;

#[cfg(windows)]
mod win32 {
    #![expect(
        unsafe_code,
        reason = "token and volume queries through Win32; each unsafe block states its invariants"
    )]

    use std::mem::size_of;
    use std::path::Path;
    use std::ptr::{from_mut, null_mut};

    use dedupe_backend_generic::win_sys::{to_wide_nul, volume_root, OwnedHandle};
    use windows_sys::Win32::Foundation::{HANDLE, MAX_PATH};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows_sys::Win32::Storage::FileSystem::GetVolumeInformationW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    use super::Capabilities;
    use crate::volume_io::open_raw_volume;

    /// Probes the volume that holds `path`. Each field is `false` when its probe fails.
    pub fn probe_capabilities(path: &Path) -> Capabilities {
        let root = volume_root(path);
        Capabilities {
            admin: is_elevated(),
            ntfs: root.as_deref().is_some_and(is_ntfs),
            raw_volume_openable: root
                .as_deref()
                .is_some_and(|root| open_raw_volume(Path::new(root)).is_ok()),
        }
    }

    /// Whether the process token is elevated.
    ///
    /// Group membership (`IsUserAnAdmin`) is deliberately not used. An administrator in an
    /// unelevated UAC token passes that check but still cannot read the raw volume.
    fn is_elevated() -> bool {
        let mut token: HANDLE = null_mut();
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle that never needs closing.
        // `token` is a live out-parameter that the call writes only on success.
        let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
        if opened == 0 {
            return false;
        }
        // SAFETY: `OpenProcessToken` succeeded, so `token` is a new handle this function owns.
        // The guard is its only closer.
        let token = unsafe { OwnedHandle::from_raw(token) };

        let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
        let mut returned: u32 = 0;
        let Ok(size) = u32::try_from(size_of::<TOKEN_ELEVATION>()) else {
            return false;
        };
        // SAFETY: `elevation` is one u32 and `size` is its exact size, so the kernel writes only
        // inside it. `returned` is a live u32 out-parameter. `token` stays alive until this
        // function returns.
        let ok = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenElevation,
                from_mut(&mut elevation).cast(),
                size,
                &mut returned,
            )
        };
        ok != 0 && elevation.TokenIsElevated != 0
    }

    /// Whether the file-system name reported for `root` is `NTFS`.
    fn is_ntfs(root: &str) -> bool {
        let wide_root = to_wide_nul(root);
        let mut name = [0u16; MAX_PATH as usize];
        // SAFETY: `wide_root` is NUL-terminated and lives for the call. `name` is writable for
        // `MAX_PATH` u16 units, and that count is passed, so the kernel cannot write past it.
        // Every other output pointer is null, which the API accepts as "not requested".
        let ok = unsafe {
            GetVolumeInformationW(
                wide_root.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                name.as_mut_ptr(),
                MAX_PATH,
            )
        };
        if ok == 0 {
            return false;
        }
        let end = name
            .iter()
            .position(|&unit| unit == 0)
            .unwrap_or(name.len());
        name.get(..end)
            .and_then(|units| String::from_utf16(units).ok())
            .is_some_and(|fs| fs == "NTFS")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_GOOD: Capabilities = Capabilities {
        admin: true,
        ntfs: true,
        raw_volume_openable: true,
    };

    #[test]
    fn choose_mode_should_return_disabled_when_no_mft_is_set_and_everything_is_good() {
        assert_eq!(
            choose_mode(&ALL_GOOD, true),
            Mode::Fallback(FallbackReason::Disabled),
            "--no-mft must force the walker even when the MFT path is available"
        );
    }

    #[test]
    fn choose_mode_should_return_disabled_when_no_mft_is_set_and_no_capability_holds() {
        let none = Capabilities {
            admin: false,
            ntfs: false,
            raw_volume_openable: false,
        };
        assert_eq!(
            choose_mode(&none, true),
            Mode::Fallback(FallbackReason::Disabled),
            "--no-mft must win over every capability check"
        );
    }

    #[test]
    fn choose_mode_should_return_not_elevated_when_admin_is_false() {
        let caps = Capabilities {
            admin: false,
            ..ALL_GOOD
        };
        assert_eq!(
            choose_mode(&caps, false),
            Mode::Fallback(FallbackReason::NotElevated),
            "an unelevated process cannot read the raw volume"
        );
    }

    #[test]
    fn choose_mode_should_return_not_elevated_when_admin_and_ntfs_are_both_false() {
        let caps = Capabilities {
            admin: false,
            ntfs: false,
            ..ALL_GOOD
        };
        assert_eq!(
            choose_mode(&caps, false),
            Mode::Fallback(FallbackReason::NotElevated),
            "elevation is reported first because the hint names it"
        );
    }

    #[test]
    fn choose_mode_should_return_not_ntfs_when_ntfs_is_false_and_admin_holds() {
        let caps = Capabilities {
            ntfs: false,
            ..ALL_GOOD
        };
        assert_eq!(
            choose_mode(&caps, false),
            Mode::Fallback(FallbackReason::NotNtfs),
            "a non-NTFS volume has no MFT to read"
        );
    }

    #[test]
    fn choose_mode_should_return_not_ntfs_when_ntfs_and_raw_volume_are_both_false() {
        let caps = Capabilities {
            ntfs: false,
            raw_volume_openable: false,
            ..ALL_GOOD
        };
        assert_eq!(
            choose_mode(&caps, false),
            Mode::Fallback(FallbackReason::NotNtfs),
            "the file system is checked before the raw volume"
        );
    }

    #[test]
    fn choose_mode_should_return_raw_volume_unavailable_when_only_raw_volume_is_false() {
        let caps = Capabilities {
            raw_volume_openable: false,
            ..ALL_GOOD
        };
        assert_eq!(
            choose_mode(&caps, false),
            Mode::Fallback(FallbackReason::RawVolumeUnavailable),
            "an elevated NTFS root whose volume will not open must fall back"
        );
    }

    #[test]
    fn choose_mode_should_return_mft_when_every_capability_holds_and_no_mft_is_unset() {
        assert_eq!(
            choose_mode(&ALL_GOOD, false),
            Mode::Mft,
            "all checks passing must select the MFT path"
        );
    }

    #[test]
    fn choose_mode_should_return_mft_only_for_the_single_all_true_combination() {
        let mut mft_count = 0;
        for bits in 0u8..8 {
            let caps = Capabilities {
                admin: bits & 1 != 0,
                ntfs: bits & 2 != 0,
                raw_volume_openable: bits & 4 != 0,
            };
            for no_mft in [false, true] {
                if choose_mode(&caps, no_mft) == Mode::Mft {
                    mft_count += 1;
                    assert_eq!(
                        (caps, no_mft),
                        (ALL_GOOD, false),
                        "only the all-true, no_mft=false combination may select Mft"
                    );
                }
            }
        }
        assert_eq!(
            mft_count, 1,
            "exactly one of the 16 combinations selects Mft"
        );
    }

    #[test]
    fn fallback_reason_display_should_name_the_flag_when_disabled() {
        assert_eq!(
            FallbackReason::Disabled.to_string(),
            "MFT fast path disabled by --no-mft.",
            "the disabled message must name the flag the user passed"
        );
    }

    #[test]
    fn fallback_reason_display_should_mention_administrator_when_not_elevated() {
        assert_eq!(
            FallbackReason::NotElevated.to_string(),
            "not running as administrator, so the NTFS MFT fast path is off.",
            "the elevation message must say what to do"
        );
    }

    #[test]
    fn fallback_reason_display_should_say_not_ntfs_when_volume_differs() {
        assert_eq!(
            FallbackReason::NotNtfs.to_string(),
            "the volume is not NTFS, so the NTFS MFT fast path is off.",
            "the file-system message must name NTFS"
        );
    }

    #[test]
    fn fallback_reason_display_should_say_raw_volume_when_it_cannot_open() {
        assert_eq!(
            FallbackReason::RawVolumeUnavailable.to_string(),
            "the raw volume could not be opened, so the NTFS MFT fast path is off.",
            "the raw-volume message must name the volume"
        );
    }
}
