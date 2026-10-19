//! Phase 1 scan orchestrator: open the DB, guard config/volumes, drive the
//! passes, recompute `dup_groups`, and finish the run.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::config::{Config, VerifyMode};
use crate::db::ids::IdAllocator;
use crate::db::writer::{spawn_writer, Batch};
use crate::db::{self, queries};
use crate::error::{CoreError, Result};
use crate::model::{now_unix_ns, Phase};

use super::{
    pass_enumerate, pass_full, pass_prefix, pass_size, pass_verify, resume, Ctx, Services,
    VolumeScan,
};

pub struct ScanOptions {
    pub cli_args: String,
    pub resume: bool,
    pub stop_after: Option<i64>,
    /// Run without the database lock when the filesystem does not support locking.
    pub no_lock: bool,
}

pub fn run_scan(
    db_path: &Path,
    roots: &[PathBuf],
    cfg: &Config,
    svc: &Services,
    opts: &ScanOptions,
) -> Result<i64> {
    let now = now_unix_ns();
    let _lock = db::lock::acquire_with(db_path, opts.no_lock)?; // held until run_scan returns
    let db = db::Db::open(db_path, cfg)?;

    resume::check_config_guard(&db.conn, cfg)?;
    if queries::meta_get(&db.conn, "hash_algo")?.is_none() {
        resume::init_meta(&db.conn, cfg, now)?;
    }

    let (run_id, volumes, phases) = if opts.resume {
        let plan = resume::plan_resume(&db.conn, cfg, svc.volume, svc.codec)?;
        (plan.run_id, plan.volumes, plan.phases)
    } else {
        let mut by_vol: BTreeMap<i64, VolumeScan> = BTreeMap::new();
        for root in roots {
            let canon = std::fs::canonicalize(root)?;
            let info = svc.volume.identify(&canon)?;
            let vid = queries::upsert_volume(&db.conn, &info, now)?;
            let raw = svc.codec.path_to_raw(&canon);
            let entry = by_vol.entry(vid).or_insert_with(|| VolumeScan {
                volume_id: vid,
                mount: info.mount.clone(),
                roots: Vec::new(),
            });
            if !entry.roots.contains(&raw) {
                entry.roots.push(raw);
            }
        }
        let volumes: Vec<VolumeScan> = by_vol.into_values().collect();
        let vids: Vec<i64> = volumes.iter().map(|v| v.volume_id).collect();
        for old in queries::running_runs_on_volumes(&db.conn, &vids)? {
            tracing::warn!(old, "marking overlapping running run abandoned");
            queries::abandon_run(&db.conn, old)?;
        }
        let run_id = queries::insert_run(&db.conn, &opts.cli_args, "unknown", now)?;
        for v in &volumes {
            queries::upsert_run_volume(&db.conn, run_id, v.volume_id)?;
            for r in &v.roots {
                queries::insert_run_root(&db.conn, run_id, v.volume_id, r)?;
            }
        }
        (run_id, volumes, HashMap::new())
    };

    let read = db::Db::open_readonly(db_path)?.conn;
    let ids = IdAllocator::seed(&read)?;
    let writer = spawn_writer(db.conn, cfg.prefix_total() as i64)?;
    let ctx = Ctx {
        cfg,
        run_id,
        writer: &writer,
        read: &read,
        ids: &ids,
        svc,
    };

    let passes = run_passes(&ctx, &volumes, &phases, opts.stop_after);
    // `ctx` is not used after this point, so `writer` can be moved (no Drop on Ctx).
    // A failed or cancelled pass still joins the writer, so every batch already sent
    // is committed.
    let stopped = merge_writer_result(passes, writer.shutdown())?;

    if !stopped {
        let mut db2 = db::Db::open(db_path, cfg)?;
        // A fresh timestamp: `now` is the run's start, and `finished_at` orders runs.
        let finished = now_unix_ns();
        // One IMMEDIATE transaction: a concurrent `report` sees either the old groups with
        // the old latest run or the new ones with the new run, never a mix. IMMEDIATE
        // because the block starts with a read.
        let tx = db2
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for v in &volumes {
            let phase = queries::get_phase(&tx, run_id, v.volume_id)?;
            if phase < Phase::GroupsDone {
                queries::recompute_groups(&tx, v.volume_id, run_id, finished)?;
                queries::set_run_volume_phase(&tx, run_id, v.volume_id, Phase::GroupsDone)?;
            }
        }
        queries::finish_run(&tx, run_id, finished)?;
        tx.commit()?;
    }

    Ok(run_id)
}

/// Runs the passes for every volume. Returns `true` when a stop request ended the run early.
fn run_passes(
    ctx: &Ctx,
    volumes: &[VolumeScan],
    phases: &HashMap<i64, Phase>,
    stop_after_opt: Option<i64>,
) -> Result<bool> {
    let mut stopped = false;
    // With `--verify` off there is no Verified commit; treat a stop request at
    // Verified as a stop at FullDone so the hook always has a real boundary.
    let mut stop_after = stop_after_opt;
    if ctx.cfg.verify != VerifyMode::Byte && stop_after == Some(Phase::Verified.as_i64()) {
        stop_after = Some(Phase::FullDone.as_i64());
    }

    for v in volumes {
        let mut phase = *phases.get(&v.volume_id).unwrap_or(&Phase::None);
        if stop_after == Some(0) {
            stopped = true;
            break;
        }

        if phase < Phase::Enumerated {
            pass_enumerate::run(ctx, v)?;
            phase = Phase::Enumerated;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if phase < Phase::SizeDone {
            pass_size::run(ctx, v.volume_id)?;
            phase = Phase::SizeDone;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if phase < Phase::PrefixDone {
            pass_prefix::run(ctx, v.volume_id)?;
            phase = Phase::PrefixDone;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if phase < Phase::FullDone {
            pass_full::run(ctx, v.volume_id)?;
            phase = Phase::FullDone;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        // Runs even when pass_full was skipped on resume, so rows an interrupted
        // verify-on run left in state 3 do not stay there. Before groups recompute.
        if ctx.cfg.verify != VerifyMode::Byte {
            ctx.writer.send(Batch::PromotePending {
                volume_id: v.volume_id,
                run_id: ctx.run_id,
            })?;
            ctx.writer.flush()?;
        }
        if ctx.cfg.verify == VerifyMode::Byte && phase < Phase::Verified {
            pass_verify::run(ctx, v.volume_id)?;
            phase = Phase::Verified;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        // A request to stop at GroupsDone stops before the (post-shutdown) recompute.
        if stop_after.is_some_and(|n| n >= Phase::GroupsDone.as_i64()) {
            stopped = true;
            break;
        }
    }

    Ok(stopped)
}

/// Prefer the writer's real error over the `WriterDied` a pass saw when sending.
fn merge_writer_result(passes: Result<bool>, writer: Result<()>) -> Result<bool> {
    match (passes, writer) {
        (Ok(stopped), Ok(())) => Ok(stopped),
        (Ok(_), Err(w)) | (Err(CoreError::WriterDied), Err(w)) => Err(w),
        (Err(e), Err(w)) => {
            tracing::error!("db writer also failed: {w}");
            Err(e)
        }
        (Err(e), Ok(())) => Err(e),
    }
}

fn stop_hit(stop_after: Option<i64>, phase: Phase) -> bool {
    stop_after.is_some_and(|n| n <= phase.as_i64())
}
