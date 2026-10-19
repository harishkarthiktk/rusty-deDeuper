//! The Windows enumerator: the NTFS `$MFT` when it can run, the standard walker otherwise.

use std::ffi::OsString;
use std::fmt::Display;
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dedupe_backend_generic::{GenericPathCodec, WalkdirEnumerator};
use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::error::{CoreError, Result};
use dedupe_core::traits::{Enumerator, PathCodec, WalkItem};

use crate::caps::{choose_mode, probe_capabilities, FallbackReason, Mode};
use crate::mft_cache::{
    self, apply_delta, cache_path, max_touched, validate, CacheError, CachedIndex, DeltaStats,
    JournalStamp, VolumeStamp,
};
use crate::mft_walk::{
    emit_tree, read_tree, MftError, MftEvent, MftObserver, MftStats, NtfsRecordSource,
    RecordSource, VolumeGeometry,
};
use crate::tree::VolumeTree;
use crate::usn::{query_journal, read_journal, JournalInfo, UsnEntry};
use crate::volume_io::{
    ntfs_volume_data, ntfs_volume_serial, open_raw_volume, record_number_of, resolve_root,
    RawVolume,
};

/// Messages the enumerator wants the user to see, such as why the walker was used.
///
/// Shared between the enumerator and the cli, which drains it after the scan. A message is never
/// dropped, so the user can always learn why the fast path was not used.
#[derive(Debug, Default)]
pub struct EnumNotices {
    messages: Mutex<Vec<String>>,
    walker_used: AtomicBool,
}

impl EnumNotices {
    pub fn push(&self, message: impl Into<String>) {
        self.lock().push(message.into());
    }

    /// Whether any part of the scan went through the standard walker, by choice
    /// (`--no-mft`), by capability, or after a failed `$MFT` read.
    pub fn walker_used(&self) -> bool {
        self.walker_used.load(Ordering::Relaxed)
    }

    fn mark_walker_used(&self) {
        self.walker_used.store(true, Ordering::Relaxed);
    }

    /// Takes every message queued so far and leaves the queue empty.
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut *self.lock())
    }

    fn lock(&self) -> MutexGuard<'_, Vec<String>> {
        // A poisoned lock still holds a valid Vec. A lost notice is not worth failing the scan.
        self.messages.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Where the `$MFT` index cache is kept and how it is used.
#[derive(Clone, Debug, Default)]
pub struct CacheConfig {
    /// Directory that holds one cache file per volume. `None` disables the cache entirely, which
    /// is the default. Nothing is read or written then.
    pub dir: Option<PathBuf>,
    /// Ignores an existing cache file and reads the whole volume, but still writes a fresh cache.
    pub rebuild: bool,
}

/// Uses the NTFS `$MFT` for each root when [`choose_mode`] allows it, and the walker otherwise.
///
/// `Enumerator::walk` takes no cancel token, so the token is held here. It lets Ctrl-C stop the
/// `$MFT` read itself, which can run for minutes on a large volume.
pub struct AutoEnumerator {
    no_mft: bool,
    fallback: WalkdirEnumerator,
    notices: Arc<EnumNotices>,
    cancel: Token,
    observer: Option<MftObserver>,
    cache: CacheConfig,
}

impl AutoEnumerator {
    pub fn new(no_mft: bool, notices: Arc<EnumNotices>, cancel: Token) -> Self {
        Self {
            no_mft,
            fallback: WalkdirEnumerator,
            notices,
            cancel,
            observer: None,
            cache: CacheConfig::default(),
        }
    }

    /// Uses the on-disk `$MFT` index cache described by `cache`.
    pub fn with_cache(mut self, cache: CacheConfig) -> Self {
        self.cache = cache;
        self
    }

    /// Reports `$MFT` read progress to `observer`.
    pub fn with_observer(mut self, observer: MftObserver) -> Self {
        self.observer = Some(observer);
        self
    }

    fn notify(&self, ev: MftEvent) {
        if let Some(observer) = &self.observer {
            observer(ev);
        }
    }
}

/// Why the `$MFT` attempt ended before it finished.
enum MftFailure {
    /// Setup or the MFT read failed. The walker is used instead.
    Fallback(String),
    /// The visitor failed or the scan was cancelled. A visitor error would repeat in the walker,
    /// and a cancelled scan must stop rather than start another walk. Neither gets a notice.
    Stop(CoreError),
}

impl Enumerator for AutoEnumerator {
    fn walk(
        &self,
        root: &Path,
        cfg: &Config,
        visit: &mut dyn FnMut(WalkItem) -> Result<()>,
    ) -> Result<()> {
        match choose_mode(&probe_capabilities(root), self.no_mft) {
            Mode::Fallback(reason) => {
                // `--no-mft` is the user's own choice, so it needs no notice.
                if reason != FallbackReason::Disabled {
                    self.notices.push(reason.to_string());
                }
                self.notices.mark_walker_used();
                self.fallback.walk(root, cfg, visit)
            }
            Mode::Mft => {
                let result = walk_mft_volume(
                    root,
                    cfg,
                    &self.cancel,
                    &self.cache,
                    &self.notices,
                    &|ev| self.notify(ev),
                    visit,
                );
                // Sent on every path, so a progress display never outlives the `$MFT` attempt.
                self.notify(MftEvent::Finished);
                match result {
                    Ok(stats) => {
                        tracing::debug!(
                            records = stats.records_seen,
                            skipped = stats.records_skipped_errors,
                            items = stats.items_emitted,
                            "MFT walk finished"
                        );
                        Ok(())
                    }
                    Err(MftFailure::Stop(err)) => Err(err),
                    Err(MftFailure::Fallback(reason)) => {
                        tracing::warn!(
                            root = %root.display(),
                            %reason,
                            "MFT walk failed; using the standard walker"
                        );
                        self.notices
                            .push(format!("fell back to the standard walker: {reason}"));
                        self.notices.mark_walker_used();
                        // Items the MFT pass already yielded are yielded again here. That is
                        // safe because enumeration is an idempotent upsert keyed by path.
                        self.fallback.walk(root, cfg, visit)
                    }
                }
            }
        }
    }
}

/// Walks the `$MFT` of the volume holding `root` and yields the items under `root`.
///
/// Yielded paths start with `root` exactly as the caller passed it. They are never canonicalized
/// to `\\?\`, because the pipeline finds a file's folder by comparing these prefixes.
///
/// With `cache` enabled, the whole-volume tree comes from the cache file plus the USN journal
/// entries written since it was saved, when that is possible. Otherwise every record is read.
fn walk_mft_volume(
    root: &Path,
    cfg: &Config,
    cancel: &Token,
    cache: &CacheConfig,
    notices: &EnumNotices,
    on_event: &dyn Fn(MftEvent),
    visit: &mut dyn FnMut(WalkItem) -> Result<()>,
) -> std::result::Result<MftStats, MftFailure> {
    // Resolve the root once, by handle: the record number and the volume to read both come
    // from the object the root really is, so a junction cannot mix two volumes.
    let resolved = resolve_root(root).map_err(at_stage("resolve the scan root"))?;
    let volume = open_raw_volume(Path::new(&OsString::from_wide(&resolved.volume_root)))
        .map_err(at_stage("open raw volume"))?;
    let geometry = ntfs_volume_data(&volume).map_err(at_stage("read NTFS volume data"))?;
    let serial = ntfs_volume_serial(&volume).map_err(at_stage("read NTFS volume data"))?;
    // `dwVolumeSerialNumber` is compared on its low 32 bits only: safe whether or not it
    // is the full NTFS serial.
    if serial as u32 != resolved.serial_low {
        return Err(MftFailure::Fallback(
            "scan root resolves to a different volume than the one opened".to_owned(),
        ));
    }
    let root_record = record_number_of(resolved.frn);

    let codec = GenericPathCodec;
    let root_prefix = codec.path_to_raw(root);
    let to_component = |units: &[u16]| codec.to_raw(&OsString::from_wide(units));

    // Read before any record, so a change made during the read is replayed by the next scan.
    let site = cache_site(&volume, &geometry, cache, notices);
    let plan = match &site {
        Some(site) if !cache.rebuild => load_cache_plan(site, &volume, notices),
        _ => Plan::Full,
    };

    let mut source =
        NtfsRecordSource::open(volume, &geometry).map_err(at_stage("open NTFS records"))?;
    let (tree, mut stats, delta) = obtain_tree(
        plan,
        &mut source,
        root_record,
        cfg.mem_budget,
        cancel,
        on_event,
        notices,
    )?;
    tree.check_root()
        .map_err(|why| MftFailure::Fallback(MftError::RootUnusable(why).to_string()))?;
    // A delta with no journal entries changed nothing, so the file on disk is already current.
    let unchanged = matches!(delta, Some(DeltaStats { entries: 0, .. }));
    // Only a complete tree is ever written. `read_tree` already fails on any record error;
    // this guard keeps that true if the policy is ever relaxed.
    if stats.records_skipped_errors == 0 {
        if let Some(site) = site.as_ref().filter(|_| !unchanged) {
            store_cache(site, &tree, notices);
        }
    }

    emit_tree(
        tree,
        &mut stats,
        &root_prefix,
        &to_component,
        &|| cancel.is_cancelled(),
        visit,
    )
    .map_err(classify)?;
    Ok(stats)
}

/// How the whole-volume tree is obtained: from the cache plus journal entries, or by a full read.
enum Plan {
    Full,
    Delta {
        cached: CachedIndex,
        entries: Vec<UsnEntry>,
    },
}

/// The cache file for one volume, and the live facts a cache must match before it is trusted.
struct CacheSite {
    path: PathBuf,
    volume: VolumeStamp,
    journal: JournalInfo,
}

/// Resolves the cache file and the live volume and journal facts. `None` turns caching off for
/// this scan. A failed query is reported once as a notice.
fn cache_site(
    volume: &RawVolume,
    geometry: &VolumeGeometry,
    cache: &CacheConfig,
    notices: &EnumNotices,
) -> Option<CacheSite> {
    let dir = cache.dir.as_deref()?;
    let (serial, journal) = match (ntfs_volume_serial(volume), query_journal(volume)) {
        (Ok(serial), Ok(journal)) => (serial, journal),
        (Err(err), _) => {
            notices.push(format!("MFT cache is off for this scan: {err}"));
            return None;
        }
        (Ok(_), Err(err)) => {
            notices.push(format!("MFT cache is off for this scan: {err}"));
            return None;
        }
    };
    Some(CacheSite {
        path: cache_path(dir, serial),
        volume: VolumeStamp {
            volume_serial: serial,
            bytes_per_record: geometry.bytes_per_record,
            total_bytes: geometry.total_bytes,
            mft_valid_length: geometry.mft_valid_length,
        },
        journal,
    })
}

/// Decides whether the cache file can seed this scan. Every reason to refuse it becomes a notice,
/// except a missing file, which is the normal first scan.
fn load_cache_plan(site: &CacheSite, volume: &RawVolume, notices: &EnumNotices) -> Plan {
    let cached = match mft_cache::read(&site.path) {
        Ok(cached) => cached,
        Err(CacheError::Io(err)) if err.kind() == io::ErrorKind::NotFound => return Plan::Full,
        Err(err) => {
            notices.push(format!("MFT cache discarded: {err}"));
            return Plan::Full;
        }
    };
    let journal = &site.journal;
    if let Some(reason) = validate(
        &cached,
        &site.volume,
        journal.journal_id,
        journal.lowest_valid_usn,
        journal.next_usn,
    ) {
        notices.push(format!("MFT cache discarded: {reason}"));
        return Plan::Full;
    }
    match read_journal(
        volume,
        journal.journal_id,
        cached.journal.next_usn,
        journal.next_usn,
    ) {
        Ok(entries) => Plan::Delta { cached, entries },
        Err(err) => {
            notices.push(format!(
                "MFT cache discarded: USN journal read failed: {err}"
            ));
            Plan::Full
        }
    }
}

/// Gets the whole-volume tree. A usable cache plus its journal entries is refreshed in memory.
/// Any refusal becomes a notice, and the records are then read in full from `source`.
///
/// The returned `DeltaStats` is `Some` only when the tree came from the cache.
fn obtain_tree<S: RecordSource>(
    plan: Plan,
    source: &mut S,
    root_record: u64,
    mem_budget: usize,
    cancel: &Token,
    on_event: &dyn Fn(MftEvent),
    notices: &EnumNotices,
) -> std::result::Result<(VolumeTree, MftStats, Option<DeltaStats>), MftFailure> {
    if let Plan::Delta { cached, entries } = plan {
        match refresh_cached(cached, &entries, source, root_record, mem_budget, on_event) {
            Ok((tree, delta)) => {
                let stats = MftStats {
                    records_seen: tree.records_iter().len() as u64,
                    ..MftStats::default()
                };
                return Ok((tree, stats, Some(delta)));
            }
            Err(reason) => notices.push(format!("MFT cache discarded: {reason}")),
        }
    }
    let (tree, stats) = read_tree(
        source,
        root_record,
        mem_budget,
        &|| cancel.is_cancelled(),
        on_event,
    )
    .map_err(classify)?;
    Ok((tree, stats, None))
}

/// Rebuilds the tree from the cached records, then applies the journal entries to it. The error
/// text is the reason for the notice. A failed refresh drops its partly updated tree.
fn refresh_cached<S: RecordSource>(
    cached: CachedIndex,
    entries: &[UsnEntry],
    source: &mut S,
    root_record: u64,
    mem_budget: usize,
    on_event: &dyn Fn(MftEvent),
) -> std::result::Result<(VolumeTree, DeltaStats), String> {
    let mut tree = VolumeTree::from_facts(root_record, mem_budget, cached.records)
        .map_err(|err| err.to_string())?;
    let records = tree.records_iter().len();
    on_event(MftEvent::CacheLoaded {
        records: records as u64,
    });
    let delta = apply_delta(&mut tree, source, entries, max_touched(records))
        .map_err(|err| err.to_string())?;
    on_event(MftEvent::DeltaApplied {
        entries: delta.entries as u64,
        reread: delta.reread as u64,
    });
    Ok((tree, delta))
}

/// Writes the cache for `tree`. A failed write is a notice, so it never fails the scan.
fn store_cache(site: &CacheSite, tree: &VolumeTree, notices: &EnumNotices) {
    let journal = JournalStamp {
        journal_id: site.journal.journal_id,
        next_usn: site.journal.next_usn,
    };
    match mft_cache::write_atomic_stream(&site.path, &site.volume, &journal, tree.records_iter()) {
        Ok(()) => notices.push(format!(
            "MFT cache written: {} ({} records)",
            site.path.display(),
            tree.records_iter().len()
        )),
        Err(err) => notices.push(format!("could not write the MFT cache: {err}")),
    }
}

/// Keeps the visitor's errors and cancellation as they are. Every other MFT failure falls back.
fn classify(err: MftError) -> MftFailure {
    match err {
        MftError::Visit(err) => MftFailure::Stop(err),
        MftError::Cancelled => MftFailure::Stop(CoreError::Cancelled),
        other => MftFailure::Fallback(other.to_string()),
    }
}

/// Tags a setup error with the step that failed, for the warning and the notice.
fn at_stage<E: Display>(stage: &'static str) -> impl Fn(E) -> MftFailure {
    move |err| MftFailure::Fallback(format!("{stage}: {err}"))
}
