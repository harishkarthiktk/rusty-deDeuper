//! Walks every `$MFT` record and feeds the resulting items to a visitor.
//!
//! Portable: [`walk_mft`] is generic over [`RecordSource`], so its tests drive it with an
//! in-memory source. [`NtfsRecordSource`] is the real source over any `Read + Seek` volume.

use std::error::Error;
use std::fmt;
use std::io::{Read, Seek};
use std::sync::Arc;

use ntfs::Ntfs;

use dedupe_core::error::{CoreError, Result as CoreResult};
use dedupe_core::path::RawPath;
use dedupe_core::traits::WalkItem;

use crate::records::{ensure_mft_runlist_in_base, extract_record, RecordError};
use crate::sector::SectorReader;
use crate::tree::{BudgetExceeded, RecordFacts, VolumeTree};

/// Cache chunk for the sector reader. A multiple of 4 KiB, so each volume read covers many
/// records, and a multiple of every common sector size.
const READ_CHUNK_BYTES: u64 = 4 << 20;
/// How often the walk polls the cancel flag, counted in records and in visited items.
const CANCEL_CHECK_INTERVAL: u64 = 4096;

/// What the record-read loop reports. Progress only; the walk never depends on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MftEvent {
    /// The walk will read `records` record slots.
    Started { records: u64 },
    /// `done` slots have been read so far.
    Records { done: u64 },
    /// A cached index was loaded and holds `records` records. Sent before any journal entry is applied.
    CacheLoaded { records: u64 },
    /// `entries` journal entries were applied, re-reading `reread` distinct records.
    DeltaApplied { entries: u64, reread: u64 },
    /// The `$MFT` attempt ended, by success, fallback, or stop.
    Finished,
}

/// Receives [`MftEvent`]s. Shared so the cli can hold the other end.
pub type MftObserver = Arc<dyn Fn(MftEvent) + Send + Sync>;

/// Volume layout from `FSCTL_GET_NTFS_VOLUME_DATA`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeGeometry {
    pub bytes_per_sector: u64,
    pub bytes_per_cluster: u64,
    pub bytes_per_record: u64,
    /// Initialized length of `$MFT`, in bytes. Records past it are not walked.
    pub mft_valid_length: u64,
    pub total_bytes: u64,
}

impl VolumeGeometry {
    /// Number of record slots in the valid part of `$MFT`. Zero when the record size is zero.
    pub fn record_count(&self) -> u64 {
        self.mft_valid_length
            .checked_div(self.bytes_per_record)
            .unwrap_or(0)
    }
}

/// Supplies file records by number. Implemented by [`NtfsRecordSource`] and by test fakes.
pub trait RecordSource {
    /// Number of record slots to walk, numbered from 0.
    fn record_count(&self) -> u64;

    /// Reads one record. `Ok(None)` means the slot holds no file.
    fn record(&mut self, number: u64) -> Result<Option<RecordFacts>, RecordError>;

    /// Forgets any cached bytes before a retry. Default: nothing is cached.
    fn invalidate(&mut self) {}
}

/// Reads records from a volume image through the `ntfs` crate.
pub struct NtfsRecordSource<R: Read + Seek> {
    fs: SectorReader<R>,
    ntfs: Ntfs,
    record_count: u64,
}

impl<R: Read + Seek> NtfsRecordSource<R> {
    /// Opens the NTFS structures on `src`.
    ///
    /// Fails when the boot sector cannot be parsed, or when its record size disagrees with the
    /// geometry from `FSCTL_GET_NTFS_VOLUME_DATA`. A disagreement means the record offsets would
    /// be wrong, so no record is trusted.
    pub fn open(src: R, geom: &VolumeGeometry) -> Result<Self, RecordError> {
        let mut fs = SectorReader::new(
            src,
            geom.bytes_per_sector,
            READ_CHUNK_BYTES,
            geom.total_bytes,
        );
        let ntfs = Ntfs::new(&mut fs)?;
        // `Ntfs::file` re-reads record 0 for every record. Pin its chunk so those reads never
        // evict the streaming window.
        if let Some(mft) = ntfs.mft_position().value() {
            fs.pin(mft.get());
        }
        if u64::from(ntfs.file_record_size()) != geom.bytes_per_record {
            return Err(RecordError::Corrupt(
                "boot sector record size differs from the volume geometry",
            ));
        }
        ensure_mft_runlist_in_base(&ntfs, &mut fs)?;
        Ok(Self {
            fs,
            ntfs,
            record_count: geom.record_count(),
        })
    }
}

impl<R: Read + Seek> RecordSource for NtfsRecordSource<R> {
    fn record_count(&self) -> u64 {
        self.record_count
    }

    fn record(&mut self, number: u64) -> Result<Option<RecordFacts>, RecordError> {
        extract_record(&self.ntfs, &mut self.fs, number)
    }

    fn invalidate(&mut self) {
        self.fs.invalidate();
    }
}

/// One retry, for a record caught mid-write (a fixup mismatch on a live volume). The retry
/// bypasses the chunk cache.
fn read_with_retry<S: RecordSource>(
    source: &mut S,
    number: u64,
) -> Result<Option<RecordFacts>, RecordError> {
    match source.record(number) {
        Err(_) => {
            source.invalidate();
            source.record(number)
        }
        ok => ok,
    }
}

/// Counters for one completed walk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MftStats {
    /// Record slots looked at, including unused and skipped ones.
    pub records_seen: u64,
    /// Records that failed to read after one retry. The walk stops at the first, so this is
    /// 0 for a completed walk.
    pub records_skipped_errors: u64,
    /// Items passed to the visitor.
    pub items_emitted: u64,
}

/// Why a walk stopped before finishing.
#[derive(Debug)]
pub enum MftError {
    /// The in-memory index would exceed `mem_budget`. Nothing was visited, so the caller can
    /// fall back to the walker.
    BudgetExceeded(BudgetExceeded),
    /// A record could not be read after one retry. Any gap can hide a subtree that the
    /// rescan would then delete as stale, so the whole MFT result is discarded and the
    /// caller falls back to the walker.
    RecordFailed { record: u64, error: RecordError },
    /// The scan root record is absent, filtered, or not a directory.
    RootUnusable(&'static str),
    /// The cancel flag was set.
    Cancelled,
    /// The visitor returned an error. The walk stopped at that item.
    Visit(CoreError),
}

impl fmt::Display for MftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BudgetExceeded(err) => write!(f, "{err}"),
            Self::RecordFailed { record, error } => {
                write!(f, "MFT record {record} could not be read: {error}")
            }
            Self::RootUnusable(why) => write!(f, "scan root is not usable on the MFT path: {why}"),
            Self::Cancelled => write!(f, "MFT walk cancelled"),
            Self::Visit(err) => write!(f, "visitor failed: {err}"),
        }
    }
}

impl Error for MftError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::BudgetExceeded(err) => Some(err),
            Self::Visit(err) => Some(err),
            Self::RecordFailed { error, .. } => Some(error),
            Self::RootUnusable(_) | Self::Cancelled => None,
        }
    }
}

/// Walks records `0..source.record_count()` and visits the items under `root_record`.
///
/// The walk has two parts. [`read_tree`] reads every record into a [`VolumeTree`]. Then
/// [`emit_tree`] visits the items under the root. Splitting them lets a caller keep or reuse the
/// tree between the two steps; `walk_mft` runs both in order.
///
/// Records are read into a [`VolumeTree`] first. That is where memory goes: `mem_budget` counts
/// the stored record facts, and it is checked before any item is visited. Emission then
/// streams: every directory is visited first, in path order, so a parent precedes its
/// children, then each file in turn. A file path is built as its item is visited, so the walk
/// never holds every item path at once. The cached directory paths are not counted in
/// `mem_budget`.
///
/// A visitor error stops the walk at the item that failed. No later item is visited, and the
/// error is returned as [`MftError::Visit`].
///
/// A record that fails is retried once (a torn read of a live record shows up as a fixup
/// mismatch). If it fails again the walk returns [`MftError::RecordFailed`] and the caller
/// falls back to the walker: one skipped directory record would hide a subtree that the
/// rescan then deletes as stale, so there is no tolerance.
///
/// `to_component` converts one UTF-16 name to a path component. `cancelled` is polled every
/// [`CANCEL_CHECK_INTERVAL`] records and items. `on_event` gets [`MftEvent::Started`] once, then
/// [`MftEvent::Records`] at the same interval as the cancel check.
#[allow(clippy::too_many_arguments)]
pub fn walk_mft<S: RecordSource>(
    source: &mut S,
    root_record: u64,
    dev: u64,
    root_prefix: &RawPath,
    to_component: &dyn Fn(&[u16]) -> RawPath,
    mem_budget: usize,
    cancelled: &dyn Fn() -> bool,
    on_event: &dyn Fn(MftEvent),
    visit: &mut dyn FnMut(WalkItem) -> CoreResult<()>,
) -> Result<MftStats, MftError> {
    let (tree, mut stats) = read_tree(source, root_record, mem_budget, cancelled, on_event)?;
    emit_tree(
        tree,
        &mut stats,
        dev,
        root_prefix,
        to_component,
        cancelled,
        visit,
    )?;
    Ok(stats)
}

/// Reads every record slot into a [`VolumeTree`] of the items under `root_record`.
///
/// This is the first half of [`walk_mft`]. It fails on the first record error that survives a
/// retry, and checks `mem_budget` on each stored record. `cancelled` and `on_event` behave as in
/// [`walk_mft`]. The returned stats hold the record counters; `items_emitted` stays zero until
/// [`emit_tree`] runs.
pub fn read_tree<S: RecordSource>(
    source: &mut S,
    root_record: u64,
    mem_budget: usize,
    cancelled: &dyn Fn() -> bool,
    on_event: &dyn Fn(MftEvent),
) -> Result<(VolumeTree, MftStats), MftError> {
    let record_count = source.record_count();
    let mut tree = VolumeTree::new(root_record, mem_budget);
    let mut stats = MftStats::default();

    on_event(MftEvent::Started {
        records: record_count,
    });
    for number in 0..record_count {
        if number % CANCEL_CHECK_INTERVAL == 0 {
            if cancelled() {
                return Err(MftError::Cancelled);
            }
            on_event(MftEvent::Records { done: number });
        }
        stats.records_seen += 1;
        match read_with_retry(source, number) {
            Ok(Some(facts)) => tree.push(facts).map_err(MftError::BudgetExceeded)?,
            Ok(None) => {}
            Err(error) => {
                return Err(MftError::RecordFailed {
                    record: number,
                    error,
                });
            }
        }
    }
    Ok((tree, stats))
}

/// Visits the items of a tree from [`read_tree`] and counts them in `stats.items_emitted`.
///
/// This is the second half of [`walk_mft`]. It consumes `tree`, so the records are released as
/// emission proceeds. `cancelled` is polled every [`CANCEL_CHECK_INTERVAL`] items. A visitor
/// error stops emission and is returned as [`MftError::Visit`].
pub fn emit_tree(
    tree: VolumeTree,
    stats: &mut MftStats,
    dev: u64,
    root_prefix: &RawPath,
    to_component: &dyn Fn(&[u16]) -> RawPath,
    cancelled: &dyn Fn() -> bool,
    visit: &mut dyn FnMut(WalkItem) -> CoreResult<()>,
) -> Result<(), MftError> {
    tree.emit_items(
        dev,
        root_prefix,
        to_component,
        &mut |item: WalkItem| -> Result<(), MftError> {
            if stats.items_emitted.is_multiple_of(CANCEL_CHECK_INTERVAL) && cancelled() {
                return Err(MftError::Cancelled);
            }
            visit(item).map_err(MftError::Visit)?;
            stats.items_emitted += 1;
            Ok(())
        },
    )
}

#[cfg(test)]
mod tests {
    use std::io;

    use dedupe_core::model::attrs;
    use dedupe_core::traits::EntryKind;

    use super::*;
    use crate::tree::NameFact;
    use crate::tree::NameSpace;

    const ROOT_PREFIX: &[u8] = b"C:\\scan";
    /// First record past the NTFS metafile range (0..=23).
    const SCAN_ROOT: u64 = 24;
    const BIG_BUDGET: usize = 1 << 24;

    /// In-memory source. Slot `n` answers record `n`: `None` is unused, `Err` fails with an
    /// I/O error carrying the message.
    struct FakeSource {
        slots: Vec<Option<Result<RecordFacts, &'static str>>>,
    }

    impl FakeSource {
        /// `total` slots, with `records` placed at their record numbers and `failing` slots
        /// set to errors.
        fn new(total: usize, records: Vec<RecordFacts>, failing: &[usize]) -> Self {
            let mut slots = vec![None; total];
            for record in records {
                let index = usize::try_from(record.record).expect("test record fits usize");
                slots[index] = Some(Ok(record));
            }
            for &index in failing {
                slots[index] = Some(Err("bad record"));
            }
            Self { slots }
        }
    }

    /// Fails the first read of one record, then answers like the inner source. Models a
    /// torn read of a live record that a retry recovers.
    struct FlakySource {
        inner: FakeSource,
        fail_first_call_of: u64,
        calls: u32,
    }

    impl RecordSource for FlakySource {
        fn record_count(&self) -> u64 {
            self.inner.record_count()
        }

        fn record(&mut self, number: u64) -> Result<Option<RecordFacts>, RecordError> {
            if number == self.fail_first_call_of {
                self.calls += 1;
                if self.calls == 1 {
                    return Err(RecordError::Io(io::Error::other("torn read")));
                }
            }
            self.inner.record(number)
        }
    }

    impl RecordSource for FakeSource {
        fn record_count(&self) -> u64 {
            u64::try_from(self.slots.len()).expect("test slot count fits u64")
        }

        fn record(&mut self, number: u64) -> Result<Option<RecordFacts>, RecordError> {
            let slot = usize::try_from(number)
                .ok()
                .and_then(|index| self.slots.get(index))
                .cloned();
            match slot {
                None | Some(None) => Ok(None),
                Some(Some(Ok(facts))) => Ok(Some(facts)),
                Some(Some(Err(message))) => Err(RecordError::Io(io::Error::other(message))),
            }
        }
    }

    fn prefix() -> RawPath {
        RawPath::new(ROOT_PREFIX)
    }

    fn to_component(units: &[u16]) -> RawPath {
        RawPath::new(String::from_utf16_lossy(units).into_bytes())
    }

    fn win32(parent: u64, text: &str) -> NameFact {
        NameFact {
            parent,
            name: text.encode_utf16().collect(),
            namespace: NameSpace::Win32,
        }
    }

    fn facts(record: u64, is_dir: bool, names: Vec<NameFact>) -> RecordFacts {
        RecordFacts {
            record,
            sequence: 1,
            in_use: true,
            is_dir,
            attrs: 0,
            reparse_tag: None,
            mtime_ft: 0,
            ctime_ft: 0,
            size: 0,
            names,
        }
    }

    /// A directory named "." under itself, which is how NTFS names a walk root.
    fn root(record: u64) -> RecordFacts {
        facts(record, true, vec![win32(record, ".")])
    }

    /// Runs a walk over `source`, returning the result and every visited item in order.
    fn walk<S: RecordSource>(
        source: &mut S,
        root_record: u64,
        mem_budget: usize,
        cancel: bool,
    ) -> (Result<MftStats, MftError>, Vec<WalkItem>) {
        let mut visited = Vec::new();
        let result = walk_mft(
            source,
            root_record,
            0,
            &prefix(),
            &to_component,
            mem_budget,
            &|| cancel,
            &|_| {},
            &mut |item: WalkItem| {
                visited.push(item);
                Ok(())
            },
        );
        (result, visited)
    }

    fn paths(items: &[WalkItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                WalkItem::Dir { path }
                | WalkItem::File { path, .. }
                | WalkItem::Error { path, .. } => path.to_display(),
            })
            .collect()
    }

    /// Root at 24, directory "Users" at 25, file "a.txt" at 26, and a second tree at 30
    /// (under record 5) that the walk must not reach.
    fn sample_records() -> Vec<RecordFacts> {
        vec![
            root(SCAN_ROOT),
            facts(25, true, vec![win32(SCAN_ROOT, "Users")]),
            facts(26, false, vec![win32(25, "a.txt")]),
            facts(30, true, vec![win32(5, "Elsewhere")]),
            facts(31, false, vec![win32(30, "other.txt")]),
        ]
    }

    #[test]
    fn walk_mft_should_emit_parents_before_children_when_tree_is_nested() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (result, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert!(result.is_ok(), "walk failed: {:?}", result.err());
        assert_eq!(
            paths(&visited),
            vec![
                "C:\\scan".to_string(),
                "C:\\scan\\Users".to_string(),
                "C:\\scan\\Users\\a.txt".to_string(),
            ],
            "items must be the root, then its directory, then the file inside it"
        );
    }

    #[test]
    fn walk_mft_should_skip_records_outside_root_when_other_tree_exists() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (_, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert!(
            !paths(&visited)
                .iter()
                .any(|path| path.contains("Elsewhere")),
            "a tree that is not under the walk root must not be visited, got {:?}",
            paths(&visited)
        );
    }

    #[test]
    fn walk_mft_should_yield_no_items_when_record_count_is_zero() {
        let mut source = FakeSource::new(0, Vec::new(), &[]);
        let (result, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert_eq!(
            result.expect("empty volume walks cleanly"),
            MftStats::default(),
            "an empty MFT must produce zeroed stats"
        );
        assert!(
            visited.is_empty(),
            "an empty MFT must visit nothing, got {visited:?}"
        );
    }

    #[test]
    fn walk_mft_should_skip_unused_slots_when_slot_is_none() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (result, _) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        let stats = result.expect("unused slots are not errors");
        assert_eq!(
            stats.records_skipped_errors, 0,
            "unused slots must not be counted as errors"
        );
    }

    #[test]
    fn walk_mft_should_count_every_slot_as_seen_when_walk_completes() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (result, _) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert_eq!(
            result.expect("walk completes").records_seen,
            32,
            "every record slot, used or not, must be counted as seen"
        );
    }

    #[test]
    fn walk_mft_should_fail_when_record_errors_twice() {
        let mut source = FakeSource::new(100, sample_records(), &[40, 41]);
        let (result, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert!(
            matches!(result, Err(MftError::RecordFailed { record: 40, .. })),
            "the first record that fails twice must fail the walk, got {result:?}"
        );
        assert!(
            visited.is_empty(),
            "a failed read must visit nothing, got {visited:?}"
        );
    }

    #[test]
    fn walk_mft_should_succeed_when_record_fails_once_then_reads() {
        let mut source = FlakySource {
            inner: FakeSource::new(32, sample_records(), &[]),
            fail_first_call_of: SCAN_ROOT,
            calls: 0,
        };
        let (result, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        let stats = result.expect("one retry must recover a torn read");
        assert_eq!(
            stats.records_skipped_errors, 0,
            "a recovered record is not a skipped record"
        );
        assert_eq!(visited.len(), 3, "the retried record must still be visited");
    }

    #[test]
    fn walk_mft_should_report_record_number_when_record_fails() {
        let failing: Vec<usize> = (0..1001).collect();
        let mut source = FakeSource::new(1001, Vec::new(), &failing);
        let (result, _) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert!(
            matches!(result, Err(MftError::RecordFailed { record: 0, .. })),
            "the walk must stop at the first failing record, got {result:?}"
        );
    }

    #[test]
    fn walk_mft_should_carry_error_text_when_record_fails() {
        let mut source = FakeSource::new(32, sample_records(), &[3]);
        let (result, _) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        let Err(err @ MftError::RecordFailed { .. }) = result else {
            panic!("expected RecordFailed, got {result:?}");
        };
        assert!(
            err.to_string().contains("bad record"),
            "the record error must be carried for diagnostics, got {err}"
        );
    }

    #[test]
    fn walk_mft_should_return_budget_exceeded_when_budget_is_too_small() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (result, visited) = walk(&mut source, SCAN_ROOT, 1, false);
        assert!(
            matches!(result, Err(MftError::BudgetExceeded(_))),
            "a budget smaller than the index must fail with BudgetExceeded, got {result:?}"
        );
        assert!(
            visited.is_empty(),
            "nothing may be visited when the index does not fit the budget"
        );
    }

    #[test]
    fn walk_mft_should_return_cancelled_when_flag_is_set() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (result, _) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, true);
        assert!(
            matches!(result, Err(MftError::Cancelled)),
            "a set cancel flag must stop the walk with Cancelled, got {result:?}"
        );
    }

    #[test]
    fn walk_mft_should_return_visit_error_when_visitor_fails() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let result = walk_mft(
            &mut source,
            SCAN_ROOT,
            0,
            &prefix(),
            &to_component,
            BIG_BUDGET,
            &|| false,
            &|_| {},
            &mut |_item: WalkItem| Err(CoreError::Other("stop".to_string())),
        );
        assert!(
            matches!(result, Err(MftError::Visit(_))),
            "a visitor error must be returned as MftError::Visit, got {result:?}"
        );
    }

    #[test]
    fn walk_mft_should_stop_visiting_when_visitor_fails() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let mut calls = 0usize;
        let _ = walk_mft(
            &mut source,
            SCAN_ROOT,
            0,
            &prefix(),
            &to_component,
            BIG_BUDGET,
            &|| false,
            &|_| {},
            &mut |_item: WalkItem| {
                calls += 1;
                Err(CoreError::Other("stop".to_string()))
            },
        );
        assert_eq!(
            calls, 1,
            "the walk must stop at the first visitor error, not continue"
        );
    }

    #[test]
    fn walk_mft_should_abort_without_further_visits_when_visit_fails_midway() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let mut calls = 0usize;
        let result = walk_mft(
            &mut source,
            SCAN_ROOT,
            0,
            &prefix(),
            &to_component,
            BIG_BUDGET,
            &|| false,
            &|_| {},
            &mut |_item: WalkItem| {
                calls += 1;
                if calls == 2 {
                    Err(CoreError::Other("stop".to_string()))
                } else {
                    Ok(())
                }
            },
        );
        assert!(
            matches!(result, Err(MftError::Visit(_))),
            "a visit error on the second item must surface as MftError::Visit, got {result:?}"
        );
        assert_eq!(
            calls, 2,
            "no item may be visited after the failing one, got {calls} visits"
        );
    }

    #[test]
    fn walk_mft_should_count_emitted_items_when_walk_succeeds() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (result, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        assert_eq!(
            result.expect("walk completes").items_emitted,
            u64::try_from(visited.len()).expect("test item count fits u64"),
            "items_emitted must equal the number of items the visitor received"
        );
    }

    #[test]
    fn walk_mft_should_report_started_then_increasing_records_when_observer_set() {
        let events = std::cell::RefCell::new(Vec::new());
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let _ = walk_mft(
            &mut source,
            SCAN_ROOT,
            0,
            &prefix(),
            &to_component,
            BIG_BUDGET,
            &|| false,
            &|ev| events.borrow_mut().push(ev),
            &mut |_item: WalkItem| Ok(()),
        );
        let events = events.into_inner();
        assert_eq!(
            events.first(),
            Some(&MftEvent::Started { records: 32 }),
            "first event is Started"
        );
        assert!(
            events
                .iter()
                .skip(1)
                .all(|ev| matches!(ev, MftEvent::Records { .. })),
            "only Records events follow Started: {events:?}"
        );
    }

    #[test]
    fn walk_mft_should_emit_file_kind_and_links_when_file_has_one_name() {
        let mut source = FakeSource::new(32, sample_records(), &[]);
        let (_, visited) = walk(&mut source, SCAN_ROOT, BIG_BUDGET, false);
        let file = visited.iter().find_map(|item| match item {
            WalkItem::File { meta, .. } => Some(meta.clone()),
            WalkItem::Dir { .. } | WalkItem::Error { .. } => None,
        });
        let Some(meta) = file else {
            panic!("the walk must yield the file, got {visited:?}");
        };
        assert_eq!(
            meta.kind,
            EntryKind::RegularFile,
            "a plain file record must be a regular file"
        );
        assert_eq!(meta.nlink, 1, "one $FILE_NAME means one link");
        assert_eq!(
            meta.attrs & attrs::HARDLINK,
            0,
            "a single-name file must not carry the hardlink attribute"
        );
    }

    #[test]
    fn read_tree_then_emit_tree_should_match_walk_mft_when_same_source() {
        let mut walked_source = FakeSource::new(32, sample_records(), &[]);
        let (walked_result, walked) = walk(&mut walked_source, SCAN_ROOT, BIG_BUDGET, false);
        let walked_stats = walked_result.expect("walk completes");

        let mut split_source = FakeSource::new(32, sample_records(), &[]);
        let (tree, mut stats) =
            read_tree(&mut split_source, SCAN_ROOT, BIG_BUDGET, &|| false, &|_| {})
                .expect("read completes");
        let mut split = Vec::new();
        emit_tree(
            tree,
            &mut stats,
            0,
            &prefix(),
            &to_component,
            &|| false,
            &mut |item: WalkItem| {
                split.push(item);
                Ok(())
            },
        )
        .expect("emit completes");

        assert_eq!(
            paths(&split),
            paths(&walked),
            "read_tree then emit_tree must visit the same items in the same order as walk_mft"
        );
        assert_eq!(
            stats, walked_stats,
            "read_tree then emit_tree must report the same stats as walk_mft"
        );
    }

    #[test]
    fn volume_geometry_record_count_should_divide_valid_length_when_record_size_is_set() {
        let geom = VolumeGeometry {
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            bytes_per_record: 1024,
            mft_valid_length: 10 * 1024 + 512,
            total_bytes: 1 << 30,
        };
        assert_eq!(
            geom.record_count(),
            10,
            "a partial trailing record must not be counted"
        );
    }

    #[test]
    fn volume_geometry_record_count_should_return_zero_when_record_size_is_zero() {
        let geom = VolumeGeometry {
            bytes_per_sector: 512,
            bytes_per_cluster: 4096,
            bytes_per_record: 0,
            mft_valid_length: 1 << 20,
            total_bytes: 1 << 30,
        };
        assert_eq!(
            geom.record_count(),
            0,
            "a zero record size must yield zero records, not a division panic"
        );
    }
}
