//! Shared time conversions. Convention: signed nanoseconds since the Unix epoch
//! on every platform (pre-1970 is negative). Out-of-range values are `None`.

use std::time::{SystemTime, UNIX_EPOCH};

const NS_PER_SEC: i64 = 1_000_000_000;
const FILETIME_UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;

/// Signed Unix nanoseconds; `None` outside the i64 range.
pub fn system_time_to_unix_ns(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(e) => i64::try_from(e.duration().as_nanos()).ok()?.checked_neg(),
    }
}

/// `secs` + `nsec` (as from `st_ctime`/`st_ctime_nsec`), checked.
pub fn unix_parts_to_ns(secs: i64, nsec: i64) -> Option<i64> {
    secs.checked_mul(NS_PER_SEC)?.checked_add(nsec)
}

/// FILETIME (100 ns ticks since 1601) to signed Unix nanoseconds, checked.
pub fn filetime_to_unix_ns(ft: u64) -> Option<i64> {
    let ticks = i128::from(ft) - FILETIME_UNIX_EPOCH_TICKS;
    i64::try_from(ticks.checked_mul(100)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn system_time_epoch_is_zero() {
        assert_eq!(system_time_to_unix_ns(UNIX_EPOCH), Some(0));
    }

    #[test]
    fn system_time_one_ns_before_epoch_is_minus_one() {
        let t = UNIX_EPOCH.checked_sub(Duration::from_nanos(1)).unwrap();
        assert_eq!(system_time_to_unix_ns(t), Some(-1));
    }

    #[test]
    fn system_time_year_2262_boundary() {
        let max = UNIX_EPOCH
            .checked_add(Duration::from_nanos(i64::MAX as u64))
            .unwrap();
        assert_eq!(system_time_to_unix_ns(max), Some(i64::MAX));
        let over = max.checked_add(Duration::from_nanos(1)).unwrap();
        assert_eq!(system_time_to_unix_ns(over), None);
    }

    #[test]
    fn system_time_pre_1970_below_i64_min_is_none() {
        let t = UNIX_EPOCH
            .checked_sub(Duration::from_nanos(i64::MAX as u64))
            .and_then(|t| t.checked_sub(Duration::from_nanos(2)))
            .unwrap();
        assert_eq!(system_time_to_unix_ns(t), None);
    }

    #[test]
    fn unix_parts_basic_and_negative() {
        assert_eq!(unix_parts_to_ns(0, 0), Some(0));
        assert_eq!(unix_parts_to_ns(1, 500), Some(1_000_000_500));
        assert_eq!(unix_parts_to_ns(-1, 999_999_999), Some(-1));
    }

    #[test]
    fn unix_parts_year_2262_boundary() {
        assert_eq!(unix_parts_to_ns(9_223_372_036, 854_775_807), Some(i64::MAX));
        assert_eq!(unix_parts_to_ns(9_223_372_036, 854_775_808), None);
    }

    #[test]
    fn unix_parts_huge_secs_is_none_without_panic() {
        assert_eq!(unix_parts_to_ns(i64::MAX, 0), None);
        assert_eq!(unix_parts_to_ns(i64::MIN, 0), None);
    }

    #[test]
    fn filetime_1601_is_none() {
        assert_eq!(filetime_to_unix_ns(0), None);
    }

    #[test]
    fn filetime_unix_epoch_is_zero() {
        let ft = u64::try_from(FILETIME_UNIX_EPOCH_TICKS).unwrap();
        assert_eq!(filetime_to_unix_ns(ft), Some(0));
    }

    #[test]
    fn filetime_one_tick_after_epoch_is_100_ns() {
        let ft = u64::try_from(FILETIME_UNIX_EPOCH_TICKS).unwrap() + 1;
        assert_eq!(filetime_to_unix_ns(ft), Some(100));
    }

    #[test]
    fn filetime_year_2262_boundary() {
        // i64::MAX / 100 = 92_233_720_368_547_758 ticks past the Unix epoch.
        let last = u64::try_from(FILETIME_UNIX_EPOCH_TICKS + 92_233_720_368_547_758).unwrap();
        assert_eq!(filetime_to_unix_ns(last), Some(9_223_372_036_854_775_800));
        assert_eq!(filetime_to_unix_ns(last + 1), None);
    }

    #[test]
    fn filetime_pre_1970_lower_boundary() {
        // -(i64::MAX / 100) - 1 ticks would be below i64::MIN after scaling.
        let first = u64::try_from(FILETIME_UNIX_EPOCH_TICKS - 92_233_720_368_547_758).unwrap();
        assert_eq!(filetime_to_unix_ns(first), Some(-9_223_372_036_854_775_800));
        assert_eq!(filetime_to_unix_ns(first - 1), None);
    }

    #[test]
    fn filetime_matches_system_time_conversion() {
        let ticks_after = 1_234_567_u64;
        let ft = u64::try_from(FILETIME_UNIX_EPOCH_TICKS).unwrap() + ticks_after;
        let t = UNIX_EPOCH
            .checked_add(Duration::from_nanos(ticks_after * 100))
            .unwrap();
        assert_eq!(filetime_to_unix_ns(ft), system_time_to_unix_ns(t));

        let ticks_before = 5_u64;
        let ft = u64::try_from(FILETIME_UNIX_EPOCH_TICKS).unwrap() - ticks_before;
        let t = UNIX_EPOCH
            .checked_sub(Duration::from_nanos(ticks_before * 100))
            .unwrap();
        assert_eq!(filetime_to_unix_ns(ft), system_time_to_unix_ns(t));
    }
}
