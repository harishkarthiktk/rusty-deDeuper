//! Visit order for a pass's rows, chosen by the storage profile.
//!
//! On rotational disks, visiting files in on-disk order reduces seeks, so the
//! `Hdd` variant sorts. SSDs have no seek penalty, so the `Ssd` variant leaves the
//! rows alone and adds no work.

use crate::error::Result;
use crate::model::FileEntry;
use crate::storage_profile::Profile;

/// Chooses how a pass orders its rows. Built from a [`Profile`] by `From`.
#[derive(Clone, Copy, Debug)]
pub enum Scheduler {
    Hdd(Profile),
    Ssd(Profile),
}

impl From<Profile> for Scheduler {
    fn from(profile: Profile) -> Self {
        if profile.sort_by_extent {
            Self::Hdd(profile)
        } else {
            Self::Ssd(profile)
        }
    }
}

impl Scheduler {
    /// Concurrent hash jobs for this profile. `1` means the sequential path.
    pub fn queue_depth(&self) -> usize {
        match self {
            Self::Hdd(profile) | Self::Ssd(profile) => profile.queue_depth,
        }
    }

    /// Reorder `rows` in place by physical position.
    ///
    /// `key` returns a row's first physical offset, or `None` when unknown. Rows
    /// with no key sort last. Ties break on `FileEntry::id`, then on input
    /// position, so the output is a pure function of the input. `key` runs once
    /// per row. The `Ssd` variant returns without calling it.
    ///
    /// The key is a closure, not a trait call, so this module does not depend on
    /// the reader or the path cache. The caller decides how to look up offsets.
    ///
    /// # Errors
    /// Returns the first error from `key`. `rows` is unchanged in that case.
    pub fn order(
        &self,
        rows: &mut Vec<FileEntry>,
        mut key: impl FnMut(&FileEntry) -> Result<Option<u64>>,
    ) -> Result<()> {
        match self {
            Self::Ssd(_) => Ok(()),
            Self::Hdd(_) => {
                // Compute every key before moving any row, so a failing `key`
                // leaves `rows` intact for the caller.
                let keys = rows
                    .iter()
                    .map(|row| key(row).map(|offset| offset.unwrap_or(u64::MAX)))
                    .collect::<Result<Vec<u64>>>()?;

                let mut order: Vec<usize> = (0..rows.len()).collect();
                order.sort_unstable_by_key(|&i| (keys[i], rows[i].id, i));

                // Move rows out by index; each index appears once in `order`, so
                // every slot is taken exactly once and no row is cloned.
                let mut slots: Vec<Option<FileEntry>> =
                    std::mem::take(rows).into_iter().map(Some).collect();
                *rows = order.into_iter().filter_map(|i| slots[i].take()).collect();
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;
    use crate::path::RawPath;
    use crate::storage_profile::StorageKind;

    fn hdd_profile() -> Profile {
        Profile {
            kind: StorageKind::Hdd,
            queue_depth: 1,
            buf_size: 1024 * 1024,
            sort_by_extent: true,
        }
    }

    fn ssd_profile() -> Profile {
        Profile {
            kind: StorageKind::Ssd,
            queue_depth: 4,
            buf_size: 64 * 1024,
            sort_by_extent: false,
        }
    }

    /// A row whose `size` doubles as its physical offset in the tests below.
    fn row(id: i64, size: i64) -> FileEntry {
        let mut entry = FileEntry::enumerated(id, 1, 1, RawPath::new(format!("f{id}")), 1);
        entry.size = size;
        entry
    }

    fn ids(rows: &[FileEntry]) -> Vec<i64> {
        rows.iter().map(|row| row.id).collect()
    }

    fn fail_on_row_two(row: &FileEntry) -> Result<Option<u64>> {
        if row.id == 2 {
            Err(CoreError::Other("key lookup failed".to_string()))
        } else {
            Ok(Some(row.size as u64))
        }
    }

    #[test]
    fn scheduler_from_profile_should_pick_hdd_when_sort_by_extent() {
        let scheduler = Scheduler::from(hdd_profile());
        assert!(
            matches!(scheduler, Scheduler::Hdd(_)),
            "profile with sort_by_extent=true should give Scheduler::Hdd, got {scheduler:?}"
        );
    }

    mod order {
        use super::*;

        #[test]
        fn hdd_should_order_by_key_then_id() {
            let mut rows = vec![row(3, 200), row(1, 100), row(2, 100), row(4, 50)];
            Scheduler::Hdd(hdd_profile())
                .order(&mut rows, |r| Ok(Some(r.size as u64)))
                .expect("order with a total key should succeed");
            assert_eq!(
                ids(&rows),
                [4, 1, 2, 3],
                "rows should sort by key then id, got {:?}",
                ids(&rows)
            );
        }

        #[test]
        fn hdd_should_put_rows_without_key_last() {
            let mut rows = vec![row(1, 0), row(2, 10), row(3, 5)];
            Scheduler::Hdd(hdd_profile())
                .order(&mut rows, |r| {
                    Ok(if r.id == 1 { None } else { Some(r.size as u64) })
                })
                .expect("order with one missing key should succeed");
            assert_eq!(
                ids(&rows),
                [3, 2, 1],
                "row with no key should sort last, got {:?}",
                ids(&rows)
            );
        }

        #[test]
        fn hdd_should_leave_rows_unchanged_when_key_errors() {
            let mut rows = vec![row(1, 10), row(2, 20), row(3, 30)];
            Scheduler::Hdd(hdd_profile())
                .order(&mut rows, fail_on_row_two)
                .expect_err("a failing key should make order return an error");
            assert_eq!(
                ids(&rows),
                [1, 2, 3],
                "rows should be unchanged after a key error, got {:?}",
                ids(&rows)
            );
        }

        #[test]
        fn hdd_should_return_key_error_when_key_fails() {
            let mut rows = vec![row(1, 10), row(2, 20), row(3, 30)];
            let err = Scheduler::Hdd(hdd_profile())
                .order(&mut rows, fail_on_row_two)
                .expect_err("a failing key should make order return an error");
            assert!(
                matches!(&err, CoreError::Other(msg) if msg == "key lookup failed"),
                "key error should propagate unchanged, got {err:?}"
            );
        }

        #[test]
        fn ssd_should_not_call_key_or_reorder() {
            let mut rows = vec![row(3, 30), row(1, 10), row(2, 20)];
            let mut calls = 0usize;
            Scheduler::Ssd(ssd_profile())
                .order(&mut rows, |_| {
                    calls += 1;
                    Ok(Some(0))
                })
                .expect("ssd order should succeed");
            assert_eq!(
                (calls, ids(&rows)),
                (0, vec![3, 1, 2]),
                "ssd should make no key calls and keep row order, got (calls, ids) = {:?}",
                (calls, ids(&rows))
            );
        }
    }
}
