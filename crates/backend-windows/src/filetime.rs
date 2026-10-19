//! Windows `FILETIME` values as Unix nanoseconds.
//!
//! The conversion lives in `dedupe_core::time` so every backend shares one convention: signed
//! nanoseconds since the Unix epoch, pre-1970 values negative, out-of-range values `None`. This
//! module keeps the `$MFT` path's fallbacks: `mtime` becomes 0 when it has no value, and `ctime`
//! stays `None`.

pub use dedupe_core::time::filetime_to_unix_ns;

/// Modification time in Unix nanoseconds; 0 when the value is out of range.
pub fn mtime_ns(ft: u64) -> i64 {
    filetime_to_unix_ns(ft).unwrap_or(0)
}

/// Change time in Unix nanoseconds (`$STANDARD_INFORMATION` MFT record modification time, which
/// matches the walker's `ChangeTime`). `None` when the value is out of the `i64` range.
pub fn ctime_ns(ft: u64) -> Option<i64> {
    filetime_to_unix_ns(ft)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticks between 1601-01-01 and 1970-01-01 (11_644_473_600 seconds at 10^7 ticks/s).
    const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

    #[test]
    fn mtime_ns_should_return_zero_when_ticks_are_out_of_range() {
        assert_eq!(
            mtime_ns(1),
            0,
            "a FILETIME whose nanoseconds overflow i64 must fall back to 0"
        );
    }

    #[test]
    fn mtime_ns_should_return_converted_value_when_after_epoch() {
        assert_eq!(
            mtime_ns(UNIX_EPOCH_TICKS + 1),
            100,
            "a post-1970 modification time must be converted"
        );
    }

    #[test]
    fn mtime_ns_should_return_negative_value_when_just_before_epoch() {
        assert_eq!(
            mtime_ns(UNIX_EPOCH_TICKS - 1),
            -100,
            "a pre-1970 modification time within i64 range must be negative, not 0"
        );
    }

    #[test]
    fn ctime_ns_should_return_none_when_ticks_are_out_of_range() {
        assert_eq!(
            ctime_ns(1),
            None,
            "a FILETIME whose nanoseconds overflow i64 must be None"
        );
    }

    #[test]
    fn ctime_ns_should_return_converted_value_when_after_epoch() {
        assert_eq!(
            ctime_ns(UNIX_EPOCH_TICKS + 1),
            Some(100),
            "a post-1970 change time must be converted"
        );
    }

    #[test]
    fn ctime_ns_should_return_negative_value_when_just_before_epoch() {
        assert_eq!(
            ctime_ns(UNIX_EPOCH_TICKS - 1),
            Some(-100),
            "a pre-1970 change time within i64 range must be negative, not None"
        );
    }

    #[test]
    fn ctime_ns_should_match_core_conversion() {
        for ft in [0, 1, UNIX_EPOCH_TICKS, UNIX_EPOCH_TICKS - 1, u64::MAX] {
            assert_eq!(
                ctime_ns(ft),
                filetime_to_unix_ns(ft),
                "ctime_ns must delegate to dedupe_core::time for {ft}"
            );
        }
    }
}
