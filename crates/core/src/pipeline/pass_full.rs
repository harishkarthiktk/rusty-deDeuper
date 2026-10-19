//! `pass_full`: full-hash every prefix survivor (state 2). Without `--verify`,
//! the file lands in state 5 directly (3 is transient). With `--verify=byte`
//! it stops in state 3 until `pass_verify`.
//!
//! With `queue_depth > 1` only the reads and stats run on worker threads. Row
//! decisions and batches stay on the calling thread. Results stream to the writer in
//! bounded chunks as they arrive, so a crash or Ctrl-C keeps finished work. The final
//! DB state does not depend on the depth (each update is by row id), but batches reach
//! the writer in completion order, not row order.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;

use crate::cancel::Token;
use crate::config::VerifyMode;
use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::hash::hash_full_target;
use crate::model::{ErrRow, FileEntry, Hash, Phase, State};
use crate::path::RawPath;
use crate::progress::{emit_advance, emit_finished, emit_started, PhaseProgress};
use crate::scheduler::Scheduler;
use crate::traits::{AccessPattern, ContentReader, FileMeta, ReadTarget};

use super::pass_prefix::{changed_err, io_err, meta_matches, unchanged};
use super::{send_chunk, Ctx, PathCache};

type Hashed = Vec<(i64, Hash)>;

/// One file for a worker to hash. `idx` is the row position, so the result can
/// be matched back to its row.
struct Job {
    idx: usize,
    /// The path as stored, for the error row. Built on the planning thread because
    /// `PathCache` reads through a connection that must not leave it.
    raw: RawPath,
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

/// Result buffers for the pass, flushed to the writer in bounded chunks.
struct Out {
    hashed: Hashed,
    errors: Vec<ErrRow>,
    /// `FullHashed`, or `FullHashedPending` under `--verify=byte`.
    wrap: fn(Hashed) -> Batch,
}

fn flush_out(ctx: &Ctx, out: &mut Out, force: bool) -> Result<()> {
    send_chunk(ctx, &mut out.hashed, force, out.wrap)?;
    send_chunk(ctx, &mut out.errors, force, Batch::Error)
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

    let mut out = Out {
        hashed: Vec::new(),
        errors: Vec::new(),
        wrap: if ctx.cfg.verify == VerifyMode::Byte {
            Batch::FullHashedPending
        } else {
            Batch::FullHashed
        },
    };
    let depth = scheduler.queue_depth();
    let res = if depth > 1 {
        hash_pooled(ctx, volume_id, &rows, &mut cache, depth, &mut out)
    } else {
        hash_sequential(ctx, volume_id, &rows, &mut cache, &mut out)
    };
    // Hand over finished work even when the pass stopped (cancel, error).
    flush_out(ctx, &mut out, true)?;
    res?;

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
    out: &mut Out,
) -> Result<()> {
    for r in rows {
        ctx.svc.cancel.check()?;
        // One advance per row, whether its hash is reused or computed.
        emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        if let Some(f) = reusable_hash(ctx, r) {
            out.hashed.push((r.id, f));
            flush_out(ctx, out, false)?;
            continue;
        }
        let path = cache.file_path(r.folder_id, &r.name)?;
        let size = match r.size_bytes() {
            Ok(size) => size,
            Err(e) => {
                out.errors.push(io_err(ctx, r, &path, e));
                flush_out(ctx, out, false)?;
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
                Ok(true) => out.hashed.push((r.id, h)),
                Ok(false) => out.errors.push(changed_err(ctx, r, &path)),
                Err(e) => out.errors.push(io_err(ctx, r, &path, e)),
            },
            Err(e) => out.errors.push(io_err(ctx, r, &path, e)),
        }
        flush_out(ctx, out, false)?;
    }
    Ok(())
}

/// The `queue_depth > 1` path: plan on this thread, hash on workers, judge each
/// result on this thread as it arrives.
fn hash_pooled(
    ctx: &Ctx,
    volume_id: i64,
    rows: &[FileEntry],
    cache: &mut PathCache<'_>,
    depth: usize,
    out: &mut Out,
) -> Result<()> {
    let jobs = plan_rows(ctx, volume_id, rows, cache, out)?;
    hash_parallel(ctx, &jobs, depth, &mut |job, res| {
        emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        let r = &rows[job.idx];
        match judge(ctx, r, &job.raw, res) {
            Ok(h) => out.hashed.push((r.id, h)),
            Err(row) => out.errors.push(row),
        }
        flush_out(ctx, out, false)
    })?;
    // A cancelled run may have skipped jobs. The flag never clears, so a pass here
    // means no worker saw it set.
    ctx.svc.cancel.check()
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
    out: &mut Out,
) -> Result<Vec<Job>> {
    let mut jobs = Vec::new();
    for (idx, r) in rows.iter().enumerate() {
        ctx.svc.cancel.check()?;
        if let Some(h) = reusable_hash(ctx, r) {
            out.hashed.push((r.id, h));
        } else {
            let raw = cache.file_path(r.folder_id, &r.name)?;
            match r.size_bytes() {
                Ok(size) => {
                    jobs.push(Job {
                        idx,
                        path: ctx.svc.codec.raw_to_path(&raw),
                        raw,
                        size,
                        file_id: r.file_id.clone(),
                    });
                    continue;
                }
                Err(e) => out.errors.push(io_err(ctx, r, &raw, e)),
            }
        }
        emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        flush_out(ctx, out, false)?;
    }
    Ok(jobs)
}

/// Hashes `jobs` on up to `depth` scoped threads. Workers claim jobs from a shared
/// counter, so one slow file does not hold up the rest. Results return over a
/// channel and `on_result` runs on this thread in completion order.
fn hash_parallel(
    ctx: &Ctx,
    jobs: &[Job],
    depth: usize,
    on_result: &mut dyn FnMut(&Job, WorkerResult) -> Result<()>,
) -> Result<()> {
    // Copy the shared references out of `ctx`. Capturing `ctx` would send the
    // non-`Sync` connection toward the workers.
    let reader = ctx.svc.reader;
    let cancel = ctx.svc.cancel;
    let chunk = ctx.cfg.profile.buf_size;
    let workers = depth.min(jobs.len());
    let next = AtomicUsize::new(0);
    let (tx, rx) = mpsc::channel::<(usize, WorkerResult)>();

    // Set when a spawn fails or a result callback errs, so workers stop claiming jobs.
    let abort = AtomicBool::new(false);

    thread::scope(|s| -> Result<()> {
        for _ in 0..workers {
            let tx = tx.clone();
            let (next, abort) = (&next, &abort);
            if let Err(e) = thread::Builder::new().spawn_scoped(s, move || {
                run_worker(reader, cancel, jobs, next, abort, chunk, &tx);
            }) {
                abort.store(true, Ordering::Relaxed);
                return Err(e.into());
            }
        }
        // Drop our sender so `rx` ends once every worker has exited.
        drop(tx);
        for (k, res) in rx {
            if let Err(e) = on_result(&jobs[k], res) {
                abort.store(true, Ordering::Relaxed);
                // Returning drops `rx`, so a worker's next send fails and it exits.
                return Err(e);
            }
        }
        Ok(())
    })
}

/// Claims job indices until none are left or the run is cancelled. Sends the job
/// index, not the row index.
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
        if tx.send((k, hash_job(reader, job, chunk))).is_err() {
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
