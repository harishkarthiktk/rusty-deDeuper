//! The Windows enumerator: the NTFS `$MFT` when it can run, the standard walker otherwise.

use std::ffi::OsString;
use std::fmt::Display;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use dedupe_backend_generic::{GenericPathCodec, WalkdirEnumerator};
use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::error::{CoreError, Result};
use dedupe_core::traits::{Enumerator, PathCodec, WalkItem};

use crate::caps::{choose_mode, probe_capabilities, FallbackReason, Mode};
use crate::mft_walk::{walk_mft, MftError, MftStats, NtfsRecordSource};
use crate::volume_io::{frn_of, ntfs_volume_data, open_raw_volume, record_number_of};

/// Messages the enumerator wants the user to see, such as why the walker was used.
///
/// Shared between the enumerator and the cli, which drains it after the scan. A message is never
/// dropped, so the user can always learn why the fast path was not used.
#[derive(Debug, Default)]
pub struct EnumNotices {
    messages: Mutex<Vec<String>>,
}

impl EnumNotices {
    pub fn push(&self, message: impl Into<String>) {
        self.lock().push(message.into());
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

/// Uses the NTFS `$MFT` for each root when [`choose_mode`] allows it, and the walker otherwise.
///
/// `Enumerator::walk` takes no cancel token, so the token is held here. It lets Ctrl-C stop the
/// `$MFT` read itself, which can run for minutes on a large volume.
pub struct AutoEnumerator {
    no_mft: bool,
    fallback: WalkdirEnumerator,
    notices: Arc<EnumNotices>,
    cancel: Token,
}

impl AutoEnumerator {
    pub fn new(no_mft: bool, notices: Arc<EnumNotices>, cancel: Token) -> Self {
        Self {
            no_mft,
            fallback: WalkdirEnumerator,
            notices,
            cancel,
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
                self.fallback.walk(root, cfg, visit)
            }
            Mode::Mft => match walk_mft_volume(root, cfg, &self.cancel, visit) {
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
                    // Items the MFT pass already yielded are yielded again here. That is safe
                    // because enumeration is an idempotent upsert keyed by path.
                    self.fallback.walk(root, cfg, visit)
                }
            },
        }
    }
}

/// Walks the `$MFT` of the volume holding `root` and yields the items under `root`.
///
/// Yielded paths start with `root` exactly as the caller passed it. They are never canonicalized
/// to `\\?\`, because the pipeline finds a file's folder by comparing these prefixes.
fn walk_mft_volume(
    root: &Path,
    cfg: &Config,
    cancel: &Token,
    visit: &mut dyn FnMut(WalkItem) -> Result<()>,
) -> std::result::Result<MftStats, MftFailure> {
    let volume = open_raw_volume(root).map_err(at_stage("open raw volume"))?;
    let geometry = ntfs_volume_data(&volume).map_err(at_stage("read NTFS volume data"))?;
    let root_record = frn_of(root)
        .map(record_number_of)
        .map_err(at_stage("read the root file id"))?;

    let codec = GenericPathCodec;
    let root_prefix = codec.path_to_raw(root);
    let to_component = |units: &[u16]| codec.to_raw(&OsString::from_wide(units));

    let mut source =
        NtfsRecordSource::open(volume, &geometry).map_err(at_stage("open NTFS records"))?;
    walk_mft(
        &mut source,
        root_record,
        &root_prefix,
        &to_component,
        cfg.mem_budget,
        &|| cancel.is_cancelled(),
        visit,
    )
    .map_err(classify)
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
