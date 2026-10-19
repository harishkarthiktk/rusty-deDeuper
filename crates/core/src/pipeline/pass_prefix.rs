//! `pass_prefix`: hash the prefix of every size candidate. Small files
//! (`size <= F + L`) get both hashes in one `Hashed` batch and skip `pass_full`.
//! Hardlinks sharing an inode are hashed once (representative) and copied to
//! siblings (4.4a rule 3). Results stream to the writer in bounded chunks, and unique
//! prefixes are classified by one SQL statement that commits with the phase marker.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::config::VerifyMode;
use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::hash::{hash_full_target, hash_prefix_target};
use crate::model::{ErrRow, ErrorKind, FileEntry, Hash, Phase, State};
use crate::path::RawPath;
use crate::progress::{emit_advance, emit_finished, emit_started, PhaseProgress};
use crate::scheduler::Scheduler;
use crate::traits::{AccessPattern, FileMeta, ReadTarget};

use super::{send_chunk, Ctx, PathCache};

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    let cfg = ctx.cfg;
    let mut rows = queries::rows_in_state(ctx.read, volume_id, ctx.run_id, State::SizeCandidate)?;
    let mut cache = PathCache::new(ctx.read);
    // Must run before `hardlink_reps` and the index build: both refer to rows by
    // position. On SSD the key closure is never called, so the order is unchanged.
    Scheduler::from(cfg.profile).order(&mut rows, |r| {
        let p = cache.file_path(r.folder_id, &r.name)?;
        ctx.svc
            .reader
            .first_extent_offset(&ctx.svc.codec.raw_to_path(&p))
    })?;
    let total = rows.len() as u64;
    emit_started(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::PrefixDone,
            done: 0,
            total: Some(total),
        },
    );

    // 4.4a rule 3: hash one representative per inode; copy hashes to siblings. The
    // representative always has a lower row index, so its outcome is already known when
    // a sibling is reached. Outcomes are kept only for representatives with siblings.
    let rep_of = hardlink_reps(&rows);
    let reps: HashSet<usize> = rep_of.values().copied().collect();
    let mut outcomes: HashMap<usize, HashOutcome> = HashMap::new();

    let mut small: Vec<(i64, Hash, Hash)> = Vec::new();
    let mut large: Vec<(i64, Hash)> = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();
    // Under --verify=byte, small files wait in state 3 for pass_verify.
    let small_wrap: fn(Vec<(i64, Hash, Hash)>) -> Batch = if cfg.verify == VerifyMode::Byte {
        Batch::HashedPending
    } else {
        Batch::Hashed
    };

    let res = (|| -> Result<()> {
        for (i, r) in rows.iter().enumerate() {
            ctx.svc.cancel.check()?;
            // Every row index is counted exactly once here, including hardlink
            // siblings, so the sum of advances equals `total` (= rows.len()).
            emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
            let outcome = match rep_of.get(&i) {
                Some(rep) => match outcomes.get(rep) {
                    Some(o) => sibling_outcome(&mut cache, r, o)?,
                    None => continue, // representative vanished (cannot happen)
                },
                None => hash_row(ctx, &mut cache, r)?,
            };
            match &outcome {
                HashOutcome::Small(p, f) => small.push((r.id, *p, *f)),
                HashOutcome::Large(p) => large.push((r.id, *p)),
                HashOutcome::Err(e) => errors.push(e.clone()),
            }
            if reps.contains(&i) {
                outcomes.insert(i, outcome);
            }
            send_chunk(ctx, &mut small, false, small_wrap)?;
            send_chunk(ctx, &mut large, false, Batch::PrefixHashed)?;
            send_chunk(ctx, &mut errors, false, Batch::Error)?;
        }
        Ok(())
    })();
    // Hand over finished work even when the loop stopped (cancel, error).
    send_chunk(ctx, &mut small, true, small_wrap)?;
    send_chunk(ctx, &mut large, true, Batch::PrefixHashed)?;
    send_chunk(ctx, &mut errors, true, Batch::Error)?;
    res?;

    // Unique prefixes are classified in one writer-side statement, in the same
    // transaction as the phase marker, over every state-2 row of this run (including
    // rows committed before a crash).
    ctx.writer.send(Batch::MarkPrefixSingletons {
        volume_id,
        run_id: ctx.run_id,
    })?;
    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id,
        phase: Phase::PrefixDone,
    })?;
    ctx.writer.flush()?;
    emit_finished(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::PrefixDone,
            done: total,
            total: Some(total),
        },
    );
    Ok(())
}

/// The outcome of hashing one file, or of reusing a stored hash for it.
#[derive(Clone)]
enum HashOutcome {
    /// prefix == full (small file): both hashes.
    Small(Hash, Hash),
    /// prefix only (large file).
    Large(Hash),
    Err(ErrRow),
}

fn hash_row(ctx: &Ctx, cache: &mut PathCache<'_>, r: &FileEntry) -> Result<HashOutcome> {
    let cfg = ctx.cfg;
    let total = cfg.prefix_total();
    let size = match r.size_bytes() {
        Ok(size) => size,
        Err(e) => {
            let path = cache.file_path(r.folder_id, &r.name)?;
            return Ok(HashOutcome::Err(io_err(ctx, r, &path, e)));
        }
    };
    // Reuse stored hashes for unchanged files (4.7a step 3).
    if size <= total {
        if let (Some(p), Some(f)) = (r.prefix_hash, r.full_hash) {
            if r.prefix_bytes == Some(r.size) {
                return Ok(HashOutcome::Small(p, f));
            }
        }
    } else if let Some(p) = r.prefix_hash {
        if r.prefix_bytes == Some(total as i64) {
            return Ok(HashOutcome::Large(p));
        }
    }
    let path = cache.file_path(r.folder_id, &r.name)?;
    let native = ctx.svc.codec.raw_to_path(&path);
    if size <= total {
        let target = ReadTarget {
            path: &native,
            file_id: r.file_id.as_deref(),
            access: AccessPattern::Sequential,
        };
        match hash_full_target(ctx.svc.reader, target, size, cfg.profile.buf_size) {
            Ok(h) => match unchanged(ctx, &native, r) {
                Ok(true) => Ok(HashOutcome::Small(h, h)),
                Ok(false) => Ok(HashOutcome::Err(changed_err(ctx, r, &path))),
                Err(e) => Ok(HashOutcome::Err(io_err(ctx, r, &path, e))),
            },
            Err(e) => Ok(HashOutcome::Err(io_err(ctx, r, &path, e))),
        }
    } else {
        let target = ReadTarget {
            path: &native,
            file_id: r.file_id.as_deref(),
            access: AccessPattern::Random,
        };
        match hash_prefix_target(ctx.svc.reader, target, size, cfg) {
            Ok((h, _)) => match unchanged(ctx, &native, r) {
                Ok(true) => Ok(HashOutcome::Large(h)),
                Ok(false) => Ok(HashOutcome::Err(changed_err(ctx, r, &path))),
                Err(e) => Ok(HashOutcome::Err(io_err(ctx, r, &path, e))),
            },
            Err(e) => Ok(HashOutcome::Err(io_err(ctx, r, &path, e))),
        }
    }
}

/// sibling row index -> representative row index, for rows sharing a `file_id`.
fn hardlink_reps(rows: &[FileEntry]) -> HashMap<usize, usize> {
    let mut rep_of = HashMap::new();
    let mut seen: HashMap<Vec<u8>, usize> = HashMap::new();
    for (i, r) in rows.iter().enumerate() {
        if let Some(fid) = &r.file_id {
            if let Some(&rep) = seen.get(fid) {
                rep_of.insert(i, rep);
            } else {
                seen.insert(fid.clone(), i);
            }
        }
    }
    rep_of
}

/// A hardlink sibling copies its representative's outcome (4.4a rule 3).
fn sibling_outcome(
    cache: &mut PathCache<'_>,
    r: &FileEntry,
    rep: &HashOutcome,
) -> Result<HashOutcome> {
    Ok(match rep {
        HashOutcome::Err(e) => {
            let mut e2 = e.clone();
            e2.file_row_id = Some(r.id);
            e2.path = Some(cache.file_path(r.folder_id, &r.name)?);
            HashOutcome::Err(e2)
        }
        other => other.clone(),
    })
}

/// Re-stat and compare. `Ok(false)` means the file changed during the read.
pub(crate) fn unchanged(ctx: &Ctx, native: &Path, r: &FileEntry) -> Result<bool> {
    Ok(meta_matches(&ctx.svc.reader.metadata(native)?, r))
}

/// Pure comparison of a stat against the stored row. Split out so the worker
/// pool can judge a stat that was taken on another thread.
pub(crate) fn meta_matches(md: &FileMeta, r: &FileEntry) -> bool {
    md.size == r.size && md.mtime_ns == r.mtime_ns && ctime_matches(r.ctime_ns, md.ctime_ns)
}

/// ctime is compared only when both sides have one (FAT, pre-1970 on Windows).
pub(crate) fn ctime_matches(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

pub(crate) fn changed_err(ctx: &Ctx, r: &FileEntry, path: &RawPath) -> ErrRow {
    ErrRow {
        run_id: ctx.run_id,
        file_row_id: Some(r.id),
        path: Some(path.clone()),
        kind: ErrorKind::ChangedDuringRead,
        message: Some("file changed during read".into()),
    }
}

pub(crate) fn io_err(
    ctx: &Ctx,
    r: &FileEntry,
    path: &RawPath,
    e: crate::error::CoreError,
) -> ErrRow {
    ErrRow {
        run_id: ctx.run_id,
        file_row_id: Some(r.id),
        path: Some(path.clone()),
        kind: ErrorKind::Io,
        message: Some(e.to_string()),
    }
}
