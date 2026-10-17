//! `pass_full`: full-hash every prefix survivor (state 2). Without `--verify`,
//! the file lands in state 5 directly (3 is transient). With `--verify=byte`
//! it stops in state 3 until `pass_verify`.
//!
//! With `queue_depth > 1` only the reads and stats run on worker threads. Row
//! decisions, path lookups and batches stay on the calling thread, in row order,
//! so the output does not depend on the depth.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use crate::cancel::Token;
use crate::config::VerifyMode;
use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::{CoreError, Result};
use crate::hash::hash_full_target;
use crate::model::{ErrRow, FileEntry, Hash, Phase, State};
use crate::path::RawPath;
use crate::progress::{emit_advance, emit_finished, emit_started, PhaseProgress};
use crate::scheduler::Scheduler;
use crate::traits::{AccessPattern, ContentReader, FileMeta, ReadTarget};

use super::pass_prefix::{changed_err, io_err, meta_matches, unchanged};
use super::{Ctx, PathCache};

type Hashed = Vec<(i64, Hash)>;

/// One file for a worker to hash. `idx` is the row position, so the result can
/// be put back in row order.
struct Job {
    idx: usize,
    path: PathBuf,
    size: u64,
    /// Owned copy of the row's file id. A worker borrows it for its `ReadTarget`.
    file_id: Option<Vec<u8>>,
}

/// What a worker learned about one file. The row is judged on the main thread.
struct WorkerResult {
    hash: Result<Hash>,
    meta: Result<FileMeta>,
}

/// What `plan_rows` decided for one row. Kept per row so `finish_rows` emits the
/// batches in row order without deciding again.
enum Slot {
    /// A stored hash, kept as is.
    Reused(Hash),
    /// Sent to a worker; its result is judged in `finish_rows`.
    Job,
    /// Could not be planned (corrupt size). Recorded directly, no worker involved.
    Failed(ErrRow),
}

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    let mut rows = queries::rows_in_state(ctx.read, volume_id, ctx.run_id, State::PrefixHashed)?;
    let mut cache = PathCache::new(ctx.read);
    let scheduler = Scheduler::from(ctx.cfg.profile);
    // Reorder before anything else reads `rows`. On SSD the key closure is never
    // called, so the default path keeps the database order.
    scheduler.order(&mut rows, |r| {
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
            phase: Phase::FullDone,
            done: 0,
            total: Some(total),
        },
    );

    let depth = scheduler.queue_depth();
    let (hashed, errors) = if depth > 1 {
        hash_pooled(ctx, volume_id, &rows, &mut cache, depth)?
    } else {
        hash_sequential(ctx, volume_id, &rows, &mut cache)?
    };

    let full_batch = if ctx.cfg.verify == VerifyMode::Byte {
        Batch::FullHashedPending(hashed)
    } else {
        Batch::FullHashed(hashed)
    };
    ctx.writer.send(full_batch)?;
    if !errors.is_empty() {
        ctx.writer.send(Batch::Error(errors))?;
    }
    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id,
        phase: Phase::FullDone,
    })?;
    ctx.writer.flush()?;
    emit_finished(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::FullDone,
            done: total,
            total: Some(total),
        },
    );
    Ok(())
}

/// The `queue_depth == 1` path: one row at a time on this thread, no threads spawned.
fn hash_sequential(
    ctx: &Ctx,
    volume_id: i64,
    rows: &[FileEntry],
    cache: &mut PathCache<'_>,
) -> Result<(Hashed, Vec<ErrRow>)> {
    let mut hashed: Hashed = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();
    for r in rows {
        ctx.svc.cancel.check()?;
        // One advance per row, whether its hash is reused or computed.
        emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        if let Some(f) = reusable_hash(ctx, r) {
            hashed.push((r.id, f));
            continue;
        }
        let path = cache.file_path(r.folder_id, &r.name)?;
        let size = match r.size_bytes() {
            Ok(size) => size,
            Err(e) => {
                errors.push(io_err(ctx, r, &path, e));
                continue;
            }
        };
        let native = ctx.svc.codec.raw_to_path(&path);
        let target = ReadTarget {
            path: &native,
            file_id: r.file_id.as_deref(),
            access: AccessPattern::Sequential,
        };
        match hash_full_target(ctx.svc.reader, target, size, ctx.cfg.profile.buf_size) {
            Ok(h) => match unchanged(ctx, &native, r) {
                Ok(true) => hashed.push((r.id, h)),
                Ok(false) => errors.push(changed_err(ctx, r, &path)),
                Err(e) => errors.push(io_err(ctx, r, &path, e)),
            },
            Err(e) => errors.push(io_err(ctx, r, &path, e)),
        }
    }
    Ok((hashed, errors))
}

/// The `queue_depth > 1` path: plan on this thread, hash on workers, then judge
/// on this thread in row order.
fn hash_pooled(
    ctx: &Ctx,
    volume_id: i64,
    rows: &[FileEntry],
    cache: &mut PathCache<'_>,
    depth: usize,
) -> Result<(Hashed, Vec<ErrRow>)> {
    let (slots, jobs) = plan_rows(ctx, volume_id, rows, cache)?;
    let results = hash_parallel(ctx, volume_id, jobs, rows.len(), depth)?;
    // A cancelled run may have skipped jobs. Checking here turns that into
    // `Cancelled` before `finish_rows` expects a result for every job. The flag
    // never clears, so a pass here means no worker saw it set.
    ctx.svc.cancel.check()?;
    finish_rows(ctx, rows, cache, slots, results)
}

/// Walks rows in order on this thread. Stored hashes are kept in place, a row
/// with a corrupt size is recorded as failed, and every other row becomes a job.
/// Paths are built here because `PathCache` reads through a connection that must
/// not leave this thread.
///
/// Reused and failed rows never reach a worker, so they advance progress here.
/// Jobs advance when their result comes back, so each row advances exactly once.
fn plan_rows(
    ctx: &Ctx,
    volume_id: i64,
    rows: &[FileEntry],
    cache: &mut PathCache<'_>,
) -> Result<(Vec<Slot>, Vec<Job>)> {
    let mut slots = Vec::with_capacity(rows.len());
    let mut jobs = Vec::new();
    for (idx, r) in rows.iter().enumerate() {
        ctx.svc.cancel.check()?;
        let slot = if let Some(h) = reusable_hash(ctx, r) {
            Slot::Reused(h)
        } else {
            let path = cache.file_path(r.folder_id, &r.name)?;
            match r.size_bytes() {
                Ok(size) => {
                    jobs.push(Job {
                        idx,
                        path: ctx.svc.codec.raw_to_path(&path),
                        size,
                        file_id: r.file_id.clone(),
                    });
                    Slot::Job
                }
                Err(e) => Slot::Failed(io_err(ctx, r, &path, e)),
            }
        };
        if !matches!(slot, Slot::Job) {
            emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        }
        slots.push(slot);
    }
    Ok((slots, jobs))
}

/// Hashes `jobs` on up to `depth` scoped threads. Workers claim jobs from a shared
/// counter, so one slow file does not hold up the rest. Results return over a
/// channel and are stored by row index.
fn hash_parallel(
    ctx: &Ctx,
    volume_id: i64,
    jobs: Vec<Job>,
    rows_len: usize,
    depth: usize,
) -> Result<Vec<Option<WorkerResult>>> {
    // Copy the shared references out of `ctx`. Capturing `ctx` would send the
    // non-`Sync` connection toward the workers.
    let reader = ctx.svc.reader;
    let cancel = ctx.svc.cancel;
    let chunk = ctx.cfg.profile.buf_size;
    let workers = depth.min(jobs.len());
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, WorkerResult)>();
    let mut results: Vec<Option<WorkerResult>> = Vec::with_capacity(rows_len);
    results.resize_with(rows_len, || None);

    // Set when a spawn fails, so the workers that did start stop claiming jobs.
    let abort = AtomicBool::new(false);

    thread::scope(|s| -> Result<()> {
        for _ in 0..workers {
            let tx = tx.clone();
            let (jobs, next, abort) = (&jobs, &next, &abort);
            if let Err(e) = thread::Builder::new().spawn_scoped(s, move || {
                run_worker(reader, cancel, jobs, next, abort, chunk, &tx);
            }) {
                abort.store(true, Ordering::Relaxed);
                return Err(e.into());
            }
        }
        // Drop our sender so `rx` ends once every worker has exited.
        drop(tx);
        for (idx, res) in rx {
            if let Some(slot) = results.get_mut(idx) {
                *slot = Some(res);
            }
            emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        }
        Ok(())
    })?;
    Ok(results)
}

/// Claims job indices until none are left or the run is cancelled.
fn run_worker(
    reader: &dyn ContentReader,
    cancel: &Token,
    jobs: &[Job],
    next: &AtomicUsize,
    abort: &AtomicBool,
    chunk: usize,
    tx: &mpsc::Sender<(usize, WorkerResult)>,
) {
    // Relaxed: `abort` is only a stop hint, and the channel orders the results.
    while !cancel.is_cancelled() && !abort.load(Ordering::Relaxed) {
        // Relaxed is enough: the counter only hands out distinct indices, and the
        // channel orders the results.
        let k = next.fetch_add(1, Ordering::Relaxed);
        let Some(job) = jobs.get(k) else {
            break;
        };
        // A closed channel means the receiver is gone, so nothing can be reported.
        if tx.send((job.idx, hash_job(reader, job, chunk))).is_err() {
            break;
        }
    }
}

fn hash_job(reader: &dyn ContentReader, job: &Job, chunk: usize) -> WorkerResult {
    let target = ReadTarget {
        path: &job.path,
        file_id: job.file_id.as_deref(),
        access: AccessPattern::Sequential,
    };
    WorkerResult {
        hash: hash_full_target(reader, target, job.size, chunk),
        meta: reader.metadata(&job.path),
    }
}

/// Judges every row in order on this thread. Builds `hashed` and `errors` the
/// same way the sequential path does, so batch contents do not depend on depth.
fn finish_rows(
    ctx: &Ctx,
    rows: &[FileEntry],
    cache: &mut PathCache<'_>,
    slots: Vec<Slot>,
    results: Vec<Option<WorkerResult>>,
) -> Result<(Hashed, Vec<ErrRow>)> {
    let mut hashed: Hashed = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();
    for ((r, slot), result) in rows.iter().zip(slots).zip(results) {
        match slot {
            Slot::Reused(f) => hashed.push((r.id, f)),
            Slot::Failed(row) => errors.push(row),
            Slot::Job => {
                let Some(res) = result else {
                    return Err(CoreError::Other(format!(
                        "hash worker returned no result for row {}",
                        r.id
                    )));
                };
                let path = cache.file_path(r.folder_id, &r.name)?;
                match judge(ctx, r, &path, res) {
                    Ok(h) => hashed.push((r.id, h)),
                    Err(row) => errors.push(row),
                }
            }
        }
    }
    Ok((hashed, errors))
}

/// The sequential path's rule for one hashed row: keep the hash only if a fresh
/// stat still matches the row. Otherwise return the error row to record.
fn judge(
    ctx: &Ctx,
    r: &FileEntry,
    path: &RawPath,
    res: WorkerResult,
) -> std::result::Result<Hash, ErrRow> {
    match (res.hash, res.meta) {
        (Ok(h), Ok(md)) if meta_matches(&md, r) => Ok(h),
        (Ok(_), Ok(_)) => Err(changed_err(ctx, r, path)),
        (Ok(_), Err(e)) | (Err(e), _) => Err(io_err(ctx, r, path, e)),
    }
}

/// A stored full hash is reused when it was computed over the current prefix length.
fn reusable_hash(ctx: &Ctx, r: &FileEntry) -> Option<Hash> {
    if r.prefix_bytes == Some(ctx.cfg.prefix_total() as i64) {
        r.full_hash
    } else {
        None
    }
}
