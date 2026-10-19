//! I/O policy for a scan: queue depth, read buffer size, and visit order.
//!
//! The core never probes the disk. The CLI probes the first root, calls
//! [`resolve`], and stores the result in `Config::profile`. This keeps the core
//! free of `cfg(target_os)` and lets every backend feed the same probe result.
//!
//! The defaults below are a starting point, not measured optima. Tune them with
//! `fio` on the target hardware before changing them. A device the probe cannot
//! classify gets the SSD buffer and ordering with a capped queue depth.

use crate::error::{CoreError, Result};
use crate::hash::READ_CHUNK;

/// HDD read buffer. Larger reads turn one hash pass into fewer, longer
/// sequential transfers, which suits a seek-bound device.
const HDD_BUF_SIZE: usize = 1024 * 1024;

/// Cap on the SSD default queue depth. Without a cap, a many-core machine would
/// spawn one hash worker per core by default. The right value depends on the
/// drive and the workload, so measure with `fio` before raising it.
const MAX_SSD_QUEUE_DEPTH: usize = 8;

/// Full-hash queue depth when the probe cannot classify the device. An
/// unclassified device may be an external hard drive, where the SSD default of up
/// to 8 concurrent reads causes seek thrashing. fclones uses 1 sequential and 4
/// random for unknown devices. This value is a judgment call, not a measurement,
/// so re-tune it after `fio` runs on the target hardware.
const UNKNOWN_QUEUE_DEPTH: usize = 2;

/// Storage class, as determined by the probe or by an explicit user choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageKind {
    Hdd,
    Ssd,
    /// The probe could not classify the device. Scans use the SSD buffer and
    /// ordering, with the queue depth capped at `UNKNOWN_QUEUE_DEPTH`.
    Unknown,
}

/// User choice from `--profile`. `Auto` defers to the probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProfileChoice {
    #[default]
    Auto,
    Hdd,
    Ssd,
}

/// Resolved I/O policy for one scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub kind: StorageKind,
    /// Concurrent full-file hash jobs. `1` selects the sequential path.
    pub queue_depth: usize,
    /// Read buffer size in bytes for hashing and byte verification.
    pub buf_size: usize,
    /// Visit rows in on-disk (extent) order. Only meaningful on rotational media.
    pub sort_by_extent: bool,
}

impl Profile {
    /// Policy values for a storage kind. `Unknown` takes the SSD buffer and
    /// ordering so an undetectable device never gets HDD-only behavior by accident,
    /// but with a shallower queue than SSD (see `UNKNOWN_QUEUE_DEPTH`).
    fn for_kind(kind: StorageKind) -> Self {
        match kind {
            StorageKind::Hdd => Self {
                kind,
                queue_depth: 1,
                buf_size: HDD_BUF_SIZE,
                sort_by_extent: true,
            },
            StorageKind::Ssd => Self {
                kind,
                queue_depth: ssd_queue_depth(),
                buf_size: READ_CHUNK,
                sort_by_extent: false,
            },
            // Capped at the SSD depth so a single-core machine never gets more
            // workers than its SSD policy would.
            StorageKind::Unknown => Self {
                kind,
                queue_depth: UNKNOWN_QUEUE_DEPTH.min(ssd_queue_depth()),
                buf_size: READ_CHUNK,
                sort_by_extent: false,
            },
        }
    }
}

impl Default for Profile {
    /// The SSD policy. Used when no probe ran, for example in library callers
    /// that never set `Config::profile`.
    fn default() -> Self {
        Self::for_kind(StorageKind::Ssd)
    }
}

/// Resolve the scan profile from the user's choice and the probe result.
///
/// Precedence:
/// 1. An explicit `Hdd` or `Ssd` choice wins over the probe.
/// 2. `Auto` uses the probe: rotational maps to HDD, non-rotational to SSD.
/// 3. `Auto` with an unknown probe result uses the `Unknown` policy (SSD buffer,
///    queue depth capped at `UNKNOWN_QUEUE_DEPTH`) and logs it.
///
/// `queue_depth` and `buf_size`, when given, replace the policy values and must
/// be at least 1. They do not change hash output, so resume is unaffected.
///
/// # Errors
/// Returns [`CoreError::InvalidArg`] when an override is zero.
pub fn resolve(
    choice: ProfileChoice,
    rotational: Option<bool>,
    queue_depth: Option<usize>,
    buf_size: Option<usize>,
) -> Result<Profile> {
    let kind = match (choice, rotational) {
        (ProfileChoice::Hdd, _) => StorageKind::Hdd,
        (ProfileChoice::Ssd, _) => StorageKind::Ssd,
        (ProfileChoice::Auto, Some(true)) => StorageKind::Hdd,
        (ProfileChoice::Auto, Some(false)) => StorageKind::Ssd,
        (ProfileChoice::Auto, None) => {
            tracing::info!("unknown storage, capping queue depth");
            StorageKind::Unknown
        }
    };

    let mut profile = Profile::for_kind(kind);
    if let Some(depth) = queue_depth {
        profile.queue_depth = non_zero("queue_depth", depth)?;
    }
    if let Some(size) = buf_size {
        profile.buf_size = non_zero("buf_size", size)?;
    }
    Ok(profile)
}

fn non_zero(name: &str, value: usize) -> Result<usize> {
    if value == 0 {
        return Err(CoreError::InvalidArg(format!("{name} must be at least 1")));
    }
    Ok(value)
}

/// Available parallelism, clamped to `1..=MAX_SSD_QUEUE_DEPTH`. Falls back to 1
/// when the platform cannot report it, because the sequential path is always
/// correct and is the safe failure mode.
fn ssd_queue_depth() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().clamp(1, MAX_SSD_QUEUE_DEPTH))
}

#[cfg(test)]
mod tests {
    use super::*;

    mod resolve {
        use super::*;

        #[test]
        fn auto_with_rotational_true_should_pick_hdd() {
            let profile = resolve(ProfileChoice::Auto, Some(true), None, None)
                .expect("auto with a rotational probe should resolve");
            assert_eq!(
                profile.kind,
                StorageKind::Hdd,
                "auto with rotational=true should pick Hdd, got {:?}",
                profile.kind
            );
        }

        #[test]
        fn auto_with_rotational_false_should_pick_ssd() {
            let profile = resolve(ProfileChoice::Auto, Some(false), None, None)
                .expect("auto with a non-rotational probe should resolve");
            assert_eq!(
                profile.kind,
                StorageKind::Ssd,
                "auto with rotational=false should pick Ssd, got {:?}",
                profile.kind
            );
        }

        #[test]
        fn auto_with_unknown_probe_should_cap_queue_depth_and_keep_ssd_buffer() {
            let profile = resolve(ProfileChoice::Auto, None, None, None)
                .expect("auto with an unknown probe should resolve");
            // The expected depth is computed, not hard-coded, because the cap is
            // clamped to the host's core count (1 on a single-core machine).
            let expected_depth = UNKNOWN_QUEUE_DEPTH.min(ssd_queue_depth());
            assert_eq!(
                (
                    profile.kind,
                    profile.queue_depth,
                    profile.buf_size,
                    profile.sort_by_extent
                ),
                (StorageKind::Unknown, expected_depth, READ_CHUNK, false),
                "unknown probe should give (kind, queue_depth, buf_size, sort_by_extent) of \
                 (Unknown, {expected_depth}, READ_CHUNK, false), got {profile:?}"
            );
        }

        #[test]
        fn explicit_hdd_should_win_over_ssd_probe() {
            let profile = resolve(ProfileChoice::Hdd, Some(false), None, None)
                .expect("explicit hdd should resolve");
            assert_eq!(
                profile.kind,
                StorageKind::Hdd,
                "explicit Hdd should beat rotational=false, got {:?}",
                profile.kind
            );
        }

        #[test]
        fn explicit_ssd_should_win_over_hdd_probe() {
            let profile = resolve(ProfileChoice::Ssd, Some(true), None, None)
                .expect("explicit ssd should resolve");
            assert_eq!(
                profile.kind,
                StorageKind::Ssd,
                "explicit Ssd should beat rotational=true, got {:?}",
                profile.kind
            );
        }

        #[test]
        fn queue_depth_override_should_replace_policy_value() {
            // The HDD policy depth is 1, so an override of 3 is visibly different.
            let profile = resolve(ProfileChoice::Auto, Some(true), Some(3), None)
                .expect("queue depth override of 3 should resolve");
            assert_eq!(
                profile.queue_depth, 3,
                "queue_depth override should replace the HDD policy value 1, got {}",
                profile.queue_depth
            );
        }

        #[test]
        fn buf_size_override_should_replace_policy_value() {
            // The HDD policy buffer is 1 MiB, so an override of 4096 is visibly different.
            let profile = resolve(ProfileChoice::Auto, Some(true), None, Some(4096))
                .expect("buf size override of 4096 should resolve");
            assert_eq!(
                profile.buf_size, 4096,
                "buf_size override should replace the HDD policy value, got {}",
                profile.buf_size
            );
        }

        #[test]
        fn zero_queue_depth_should_be_rejected() {
            let result = resolve(ProfileChoice::Auto, None, Some(0), None);
            assert!(
                matches!(&result, Err(CoreError::InvalidArg(_))),
                "queue_depth 0 should give CoreError::InvalidArg, got {result:?}"
            );
        }

        #[test]
        fn zero_buf_size_should_be_rejected() {
            let result = resolve(ProfileChoice::Auto, None, None, Some(0));
            assert!(
                matches!(&result, Err(CoreError::InvalidArg(_))),
                "buf_size 0 should give CoreError::InvalidArg, got {result:?}"
            );
        }

        #[test]
        fn explicit_queue_depth_should_override_unknown_default() {
            // 5 is above UNKNOWN_QUEUE_DEPTH, so the explicit value is not capped.
            let profile = resolve(ProfileChoice::Auto, None, Some(5), None)
                .expect("queue depth override of 5 should resolve");
            assert_eq!(
                profile.queue_depth, 5,
                "explicit queue_depth should replace the unknown default, got {}",
                profile.queue_depth
            );
        }
    }

    mod for_kind {
        use super::*;

        #[test]
        fn unknown_kind_should_not_exceed_ssd_queue_depth() {
            let profile = Profile::for_kind(StorageKind::Unknown);
            assert!(
                profile.queue_depth <= ssd_queue_depth(),
                "unknown kind queue_depth {} should not exceed the SSD depth {}",
                profile.queue_depth,
                ssd_queue_depth()
            );
        }
    }
}
