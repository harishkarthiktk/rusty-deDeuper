//! `dedupe scan`. Persists by default (`.dedupe.db` under the first root); `--no-db`
//! runs the naive Phase 0 pipeline with no persistence; `--db` selects an explicit path.

use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use dedupe_backend_generic::GenericStorageProbe;
use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::db::queries;
use dedupe_core::exclude::ExcludeSet;
use dedupe_core::hash::Blake3Hasher;
use dedupe_core::mask::Masker;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::{scan_naive, Services};
use dedupe_core::progress::SharedSink;
use dedupe_core::report::Report;
use dedupe_core::storage_profile::{self, Profile, StorageKind};
use dedupe_core::traits::StorageProbe;
use rusqlite::Connection;

use crate::platform::{self, ScanExtras};
use crate::progress::{should_draw, BarSink};

/// Globs skipped by default (off with `--include-dev-artifacts`): package installs, build output and VCS internals.
/// Rust `target` is matched with its profile folder, so a folder named `target` holding media is kept.
const DEV_ARTIFACT_GLOBS: &[&str] = &[
    "**/site-packages",
    "**/dist-packages",
    "**/node_modules",
    "**/__pycache__",
    "**/target/debug",
    "**/target/release",
    "**/.fingerprint",
    "**/.git",
    "**/*.pyc",
    "**/*.pyo",
    "**/*.o",
    "**/*.obj",
    "**/*.pdb",
    "**/*.rlib",
    "**/*.rmeta",
];
use crate::{Resolved, ScanArgs};

/// Command line as recorded in `scan_runs.cli_args`. Informational only, so it is lossy
/// per argument (never panics on non-Unicode) and quotes what would otherwise be ambiguous.
fn recorded_args() -> String {
    std::env::args_os()
        .map(|a| match a.to_str() {
            Some(s)
                if !s.is_empty()
                    && !s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') =>
            {
                s.to_owned()
            }
            _ => format!("{a:?}"), // OsStr Debug escapes invalid bytes
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// SQLite WAL needs shared memory and reliable file locks, which network filesystems do
/// not provide. Refuse a default location there; warn when the user chose it or when the
/// disk sits behind WSL's 9p.
fn check_db_locality(db: &std::path::Path, explicit: bool) -> Result<()> {
    use dedupe_backend_generic::{fs_locality, FsLocality};
    // The DB file may not exist yet, so classify the folder that will hold it.
    let dir = db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    match (fs_locality(dir), explicit) {
        (FsLocality::Remote, false) => anyhow::bail!(
            "database would live on a network filesystem ({}); pass --db <local path>",
            dir.display()
        ),
        (FsLocality::Remote, true) => eprintln!(
            "note: database is on a network filesystem; SQLite WAL may be unreliable there"
        ),
        (FsLocality::Virtualized, _) => eprintln!(
            "note: database is on a virtualized mount (WSL 9p); SQLite WAL locking may be unreliable"
        ),
        _ => {}
    }
    Ok(())
}

pub fn run(args: ScanArgs, cancel: &Token) -> Result<()> {
    let format = args
        .format
        .resolve(args.json, std::io::stdout().is_terminal());
    // Persistence mode: --no-db -> naive (no persistence); --db -> explicit path;
    // otherwise default to <first root>/.dedupe.db.
    let db_path = if args.no_db {
        None
    } else if let Some(db) = args.db.clone() {
        Some(db)
    } else {
        Some(crate::scan_default_db(&args.roots)?)
    };
    if let Some(db) = &db_path {
        check_db_locality(db, args.db.is_some())?;
    }

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

    // Only the DB path reports progress; `--no-db` (naive) keeps the null sink.
    let draw = should_draw(
        args.progress,
        format == Resolved::Json,
        std::io::stderr().is_terminal(),
    );
    let bar_sink = if draw { Some(BarSink::new()) } else { None };
    let backend = platform::make_backend(
        &args,
        &profile,
        cancel,
        bar_sink.as_ref().map(BarSink::multi),
    );

    let mut patterns = args.exclude.clone();
    if !args.include_dev_artifacts {
        patterns.extend(DEV_ARTIFACT_GLOBS.iter().map(|glob| glob.to_string()));
    }
    let exclude_dirs = resolve_exclude_dirs(&args.exclude_dir, &args.roots)?;
    let defaults = Config::default();
    let cfg = Config {
        db_path,
        include_placeholders: args.include_placeholders,
        exclude: ExcludeSet::new(&patterns, &exclude_dirs).context("parse --exclude")?,
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

    let sink: SharedSink = match bar_sink {
        Some(bar) => Arc::new(Mutex::new(bar)),
        None => dedupe_core::progress::null_sink(),
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
        .and_then(|(report, _stats)| {
            let masking = crate::mask::masker_no_db(&args.roots[0], args.unmasked)?;
            emit(&report, format, masking)
        }),
        Some(db) => {
            if args.no_lock {
                eprintln!("note: running without a database lock (--no-lock)");
            }
            let opts = ScanOptions {
                cli_args: recorded_args(),
                resume: args.resume,
                stop_after,
                no_lock: args.no_lock,
            };
            run_scan(db, &args.roots, &cfg, &svc, &opts)
                .context("scan failed")
                .and_then(|run_id| {
                    // The enumerator is only known once the walk is over, so it is recorded
                    // here and not when the run row is created.
                    let enumerator = backend.extras.enumerator_label(args.no_mft);
                    let writer = dedupe_core::db::Db::open(db, &cfg)
                        .context("open db to record the enumerator")?;
                    queries::set_run_enumerator(&writer.conn, run_id, enumerator)
                        .context("record the enumerator")?;
                    drop(writer);
                    let dbh =
                        dedupe_core::db::Db::open_readonly(db).context("open db for report")?;
                    let report = Report::from_db(&dbh.conn, 0).context("build report")?;
                    let masking = crate::mask::masker_for(db, args.unmasked)?;
                    emit(&report, format, masking)?;
                    eprintln!("scan run {run_id} finished (enumerator: {enumerator})");
                    Ok(())
                })
        }
    };

    // Drained after the scan returns, so a failed scan still shows why the fast path was
    // not used. Each message is printed once, because draining empties the queue.
    for note in backend.extras.drain_notices() {
        eprintln!("note: {note}");
    }
    outcome
}

fn emit(
    report: &Report,
    format: Resolved,
    mut masking: Option<(Masker, Connection)>,
) -> Result<()> {
    let m = masking.as_mut().map(|(m, _)| m);
    let text = match format {
        Resolved::Json => format!("{}\n", report.to_json(m)),
        Resolved::Human => report.to_human(m),
    };
    // Flush first: tokens must never reach stdout unless they are in the vault.
    if let Some((m, v)) = masking.as_mut() {
        m.flush(v).context("write mask vault")?;
    }
    print!("{text}");
    Ok(())
}

/// Canonical form of each `--exclude-dir`. Fails when a path is missing or covers a scan root.
fn resolve_exclude_dirs(dirs: &[PathBuf], roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    // No -E: roots are left to the scan's own error handling, as before.
    if dirs.is_empty() {
        return Ok(Vec::new());
    }
    // Lowercased like the matcher, so a case-only match on a case-sensitive volume is refused too.
    let lower = |p: &std::path::Path| PathBuf::from(p.to_string_lossy().to_lowercase());
    let roots = roots
        .iter()
        .map(|r| std::fs::canonicalize(r).with_context(|| format!("scan root {}", r.display())))
        .collect::<Result<Vec<_>>>()?;
    dirs.iter()
        .map(|d| {
            let canon = std::fs::canonicalize(d)
                .with_context(|| format!("--exclude-dir {}", d.display()))?;
            if let Some(root) = roots.iter().find(|r| lower(r).starts_with(lower(&canon))) {
                anyhow::bail!(
                    "--exclude-dir {} contains scan root {}",
                    d.display(),
                    root.display()
                );
            }
            Ok(canon)
        })
        .collect()
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
}
