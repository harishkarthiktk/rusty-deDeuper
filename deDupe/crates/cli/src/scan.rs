//! `dedupe scan`. Persists by default (`.dedupe.db` under the first root); `--no-db`
//! runs the naive Phase 0 pipeline with no persistence; `--db` selects an explicit path.
//!
//! `make_backend` is the one place that chooses backend types. Windows builds get the
//! NTFS-aware enumerator and the open-by-id reader; every other build gets the generic pieces.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
#[cfg(not(windows))]
use dedupe_backend_generic::GenericBackend;
use dedupe_backend_generic::{GenericPathCodec, GenericStorageProbe, GenericVolumeIdentity};
#[cfg(windows)]
use dedupe_backend_windows::{AutoEnumerator, EnumNotices, HandleReader};
use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::hash::Blake3Hasher;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::{scan_naive, Services};
use dedupe_core::progress::SharedSink;
use dedupe_core::report::Report;
use dedupe_core::storage_profile::{self, Profile, StorageKind};
use dedupe_core::traits::{ContentReader, Enumerator, StorageProbe};

use crate::progress::{should_draw, BarSink};
use crate::{OutputFormat, ScanArgs};

/// Smallest read chunk for overlapped reads. A small `--buf-size` would otherwise turn each
/// overlapped read into a tiny request, which costs more than the overlap saves.
#[cfg(windows)]
const MIN_OVERLAPPED_CHUNK: usize = 256 * 1024;

/// The backend pieces one scan uses. `Services` borrows them for the length of the scan.
struct Backend {
    enumerator: Box<dyn Enumerator>,
    reader: Box<dyn ContentReader>,
    codec: GenericPathCodec,
    volume: GenericVolumeIdentity,
    /// Messages the enumerator queued during the scan, such as why the walker was used.
    /// Drained after the scan, on success and on failure alike.
    #[cfg(windows)]
    notices: Arc<EnumNotices>,
}

pub fn run(args: ScanArgs, cancel: &Token) -> Result<()> {
    // Persistence mode: --no-db -> naive (no persistence); --db -> explicit path;
    // otherwise default to <first root>/.dedupe.db.
    let db_path = if args.no_db {
        None
    } else if let Some(db) = args.db.clone() {
        Some(db)
    } else {
        Some(crate::scan_default_db(&args.roots)?)
    };

    let hasher = Blake3Hasher;

    // The probe runs before the profile exists, and the profile decides how the backend
    // reads files, so the probe cannot come from `make_backend`.
    //
    // A failed probe is dropped on purpose: `None` means unknown, and core maps
    // unknown storage to the SSD policy, so a probe error must not abort the scan.
    let probe = GenericStorageProbe;
    let rotational = args
        .roots
        .first()
        .and_then(|root| probe.is_rotational(root).ok().flatten());
    let buf_size = args
        .buf_size
        .map(|bytes| {
            usize::try_from(bytes).with_context(|| {
                format!("--buf-size {bytes} does not fit in usize on this platform")
            })
        })
        .transpose()?;
    let profile =
        storage_profile::resolve(args.profile.into(), rotational, args.queue_depth, buf_size)
            .context("resolve scan profile")?;

    // `--no-db` runs the naive pipeline, which never reads the profile, so a notice
    // there would describe a policy that does not run. stderr only, so JSON on
    // stdout stays clean.
    if let Some(note) = db_path
        .as_ref()
        .and_then(|_| unknown_storage_notice(&profile, args.queue_depth.is_some()))
    {
        eprintln!("{note}");
    }

    let backend = make_backend(&args, &profile, cancel);

    let defaults = Config::default();
    let cfg = Config {
        db_path,
        include_placeholders: args.include_placeholders,
        force: args.force,
        one_file_system: !args.cross_mounts,
        verify: args.verify.into(),
        profile,
        mem_budget: args
            .mem_budget
            .map(|b| b as usize)
            .unwrap_or(defaults.mem_budget),
        ..defaults
    };

    #[cfg(feature = "test-hooks")]
    let stop_after = args.stop_after_pass.map(|n| n as i64);
    #[cfg(not(feature = "test-hooks"))]
    let stop_after = None;

    // Only the DB path reports progress; `--no-db` (naive) keeps the null sink.
    let draw = should_draw(
        args.progress,
        args.format == OutputFormat::Json,
        std::io::stderr().is_terminal(),
    );
    let sink: SharedSink = if draw {
        Arc::new(Mutex::new(BarSink::new()))
    } else {
        dedupe_core::progress::null_sink()
    };
    let svc = Services {
        enumr: &*backend.enumerator,
        reader: &*backend.reader,
        codec: &backend.codec,
        volume: &backend.volume,
        hasher: &hasher,
        cancel,
        sink: &sink,
    };

    let outcome = match &cfg.db_path {
        None => scan_naive(
            &args.roots,
            svc.enumr,
            svc.reader,
            svc.hasher,
            svc.codec,
            &cfg,
            cancel,
        )
        .context("scan failed")
        .map(|(report, _stats)| emit(&report, args.format)),
        Some(db) => {
            let opts = ScanOptions {
                cli_args: std::env::args().collect::<Vec<_>>().join(" "),
                resume: args.resume,
                stop_after,
            };
            run_scan(db, &args.roots, &cfg, &svc, &opts)
                .context("scan failed")
                .and_then(|run_id| {
                    let dbh =
                        dedupe_core::db::Db::open_readonly(db).context("open db for report")?;
                    let report = Report::from_db(&dbh.conn, 0).context("build report")?;
                    emit(&report, args.format);
                    eprintln!("scan run {run_id} finished");
                    Ok(())
                })
        }
    };

    // Drained after the scan returns, so a failed scan still shows why the fast path was
    // not used. Each message is printed once, because draining empties the queue.
    for note in drain_notices(&backend) {
        eprintln!("note: {note}");
    }
    outcome
}

/// Builds the backend for one scan. Windows picks the NTFS fast path with the open-by-id
/// reader; the reader's overlapped read-ahead depends on the storage profile. The token goes to
/// the enumerator so Ctrl-C can stop the `$MFT` read, which the trait has no way to receive.
#[cfg(windows)]
fn make_backend(args: &ScanArgs, profile: &Profile, cancel: &Token) -> Backend {
    let notices = Arc::new(EnumNotices::default());
    let chunk = profile.buf_size.max(MIN_OVERLAPPED_CHUNK);
    Backend {
        enumerator: Box::new(AutoEnumerator::new(
            args.no_mft,
            Arc::clone(&notices),
            cancel.clone(),
        )),
        reader: Box::new(HandleReader::new(read_ahead_for(profile), chunk)),
        codec: GenericPathCodec,
        volume: GenericVolumeIdentity,
        notices,
    }
}

#[cfg(not(windows))]
fn make_backend(_args: &ScanArgs, _profile: &Profile, _cancel: &Token) -> Backend {
    let generic = GenericBackend::new();
    Backend {
        enumerator: Box::new(generic.enumerator),
        reader: Box::new(generic.reader),
        codec: generic.codec,
        volume: generic.volume,
    }
}

/// Notices queued by the enumerator during the scan. Always empty outside Windows.
#[cfg(windows)]
fn drain_notices(backend: &Backend) -> Vec<String> {
    backend.notices.drain()
}

#[cfg(not(windows))]
fn drain_notices(_backend: &Backend) -> Vec<String> {
    Vec::new()
}

/// Overlapped read slots for a profile. SSD gets 4, so several reads are in flight per file.
/// HDD and unknown storage get 1: parallel reads make a seek-bound disk jump between files,
/// which is the cost the HDD policy exists to avoid.
#[cfg(any(windows, test))]
fn read_ahead_for(profile: &Profile) -> usize {
    match profile.kind {
        StorageKind::Ssd => 4,
        StorageKind::Hdd | StorageKind::Unknown => 1,
    }
}

fn emit(report: &Report, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", report.to_json()),
        OutputFormat::Human => print!("{}", report.to_human()),
    }
}

/// One-line stderr note for a device the probe could not classify, or `None` when
/// no note is due. An explicit `--queue-depth` already states the concurrency the
/// user wants, so the note is skipped then. Kept pure so the wording and the
/// condition can be tested without a disk.
fn unknown_storage_notice(profile: &Profile, queue_depth_overridden: bool) -> Option<String> {
    if profile.kind != StorageKind::Unknown || queue_depth_overridden {
        return None;
    }
    Some(format!(
        "note: could not detect the storage type; reading {} files at a time. \
         For a hard drive pass --profile hdd, for an SSD pass --profile ssd \
         (or set --queue-depth).",
        profile.queue_depth
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dedupe_core::storage_profile::ProfileChoice;

    fn resolved(choice: ProfileChoice, rotational: Option<bool>) -> Profile {
        storage_profile::resolve(choice, rotational, None, None)
            .expect("resolving a profile without overrides should succeed")
    }

    #[test]
    fn notice_should_be_none_for_known_ssd() {
        let profile = resolved(ProfileChoice::Auto, Some(false));
        assert!(
            unknown_storage_notice(&profile, false).is_none(),
            "a probed SSD should not produce a notice, got {profile:?}"
        );
    }

    #[test]
    fn notice_should_be_none_for_known_hdd() {
        let profile = resolved(ProfileChoice::Auto, Some(true));
        assert!(
            unknown_storage_notice(&profile, false).is_none(),
            "a probed HDD should not produce a notice, got {profile:?}"
        );
    }

    #[test]
    fn notice_should_mention_profile_flag_when_kind_is_unknown() {
        let profile = resolved(ProfileChoice::Auto, None);
        let expected = format!(
            "note: could not detect the storage type; reading {} files at a time. \
             For a hard drive pass --profile hdd, for an SSD pass --profile ssd \
             (or set --queue-depth).",
            profile.queue_depth
        );
        assert_eq!(
            unknown_storage_notice(&profile, false).as_deref(),
            Some(expected.as_str()),
            "unknown storage should produce the exact notice text for depth {}",
            profile.queue_depth
        );
    }

    #[test]
    fn notice_should_be_none_when_queue_depth_is_overridden() {
        let profile = resolved(ProfileChoice::Auto, None);
        assert!(
            unknown_storage_notice(&profile, true).is_none(),
            "an explicit --queue-depth should suppress the unknown-storage notice"
        );
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
