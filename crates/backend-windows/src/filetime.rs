//! Conversion from Windows `FILETIME` values to Unix nanoseconds.
//!
//! A `FILETIME` counts 100 ns ticks since 1601-01-01 UTC. The helpers here follow the
//! generic backend's time semantics so MFT-derived metadata compares equal to what
//! `std::fs` reports: `mtime` falls back to 0 and `ctime` to `None` before 1970.

/// Ticks between 1601-01-01 and 1970-01-01 (11_644_473_600 seconds at 10^7 ticks/s).
const UNIX_EPOCH_TICKS: u64 = 116_444_736_000_000_000;

/// One `FILETIME` tick is 100 ns.
const NS_PER_TICK: i64 = 100;

/// Converts a `FILETIME` to Unix nanoseconds.
///
/// Returns `None` for instants before 1970 and for values whose nanosecond count does
/// not fit in `i64` (about year 2262).
pub fn filetime_to_unix_ns(ft: u64) -> Option<i64> {
    let ticks_since_unix = ft.checked_sub(UNIX_EPOCH_TICKS)?;
    let ticks = i64::try_from(ticks_since_unix).ok()?;
    ticks.checked_mul(NS_PER_TICK)
}

/// Modification time in Unix nanoseconds; 0 when the time is before 1970.
///
/// Matches the generic backend's `modified_ns`, which uses 0 as its fallback.
pub fn mtime_ns(ft: u64) -> i64 {
    filetime_to_unix_ns(ft).unwrap_or(0)
}

/// Creation time in Unix nanoseconds, used as `ctime` on Windows.
///
/// Matches the generic backend's `created()`-based `ctime`: `None` when unavailable.
pub fn ctime_ns(ft: u64) -> Option<i64> {
    filetime_to_unix_ns(ft)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_to_unix_ns_should_return_zero_when_ticks_equal_unix_epoch() {
        assert_eq!(
            filetime_to_unix_ns(UNIX_EPOCH_TICKS),
            Some(0),
            "the Unix epoch must map to 0 ns"
        );
    }

    #[test]
    fn filetime_to_unix_ns_should_return_100_when_one_tick_after_epoch() {
        assert_eq!(
            filetime_to_unix_ns(UNIX_EPOCH_TICKS + 1),
            Some(100),
            "one 100 ns tick after the epoch must map to 100 ns"
        );
    }

    #[test]
    fn filetime_to_unix_ns_should_match_known_value_when_date_is_2021() {
        // 2021-01-01T00:00:00Z is Unix 1_609_459_200 s.
        assert_eq!(
            filetime_to_unix_ns(132_539_328_000_000_000),
            Some(1_609_459_200_000_000_000),
            "2021-01-01T00:00:00Z must map to its Unix nanosecond value"
        );
    }

    #[test]
    fn filetime_to_unix_ns_should_return_none_when_before_epoch() {
        assert_eq!(
            filetime_to_unix_ns(UNIX_EPOCH_TICKS - 1),
            None,
            "one tick before 1970 must be None"
        );
    }

    #[test]
    fn filetime_to_unix_ns_should_return_none_when_value_is_zero() {
        assert_eq!(
            filetime_to_unix_ns(0),
            None,
            "FILETIME 0 (1601) must be None"
        );
    }

    #[test]
    fn filetime_to_unix_ns_should_return_none_when_value_overflows() {
        assert_eq!(
            filetime_to_unix_ns(u64::MAX),
            None,
            "u64::MAX ticks overflow i64 nanoseconds and must be None"
        );
    }

    #[test]
    fn mtime_ns_should_return_zero_when_before_epoch() {
        assert_eq!(
            mtime_ns(1),
            0,
            "a pre-1970 modification time must fall back to 0"
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
    fn ctime_ns_should_return_none_when_before_epoch() {
        assert_eq!(
            ctime_ns(1),
            None,
            "a pre-1970 creation time must be None, matching created()"
        );
    }

    #[test]
    fn ctime_ns_should_return_converted_value_when_after_epoch() {
        assert_eq!(
            ctime_ns(UNIX_EPOCH_TICKS + 1),
            Some(100),
            "a post-1970 creation time must be converted"
        );
    }
}
