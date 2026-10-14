//! Phase 1 scan orchestrator: open the DB, guard config/volumes, drive the
//! passes, recompute `dup_groups`, and finish the run.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::config::{Config, VerifyMode};
use crate::db::ids::IdAllocator;
use crate::db::writer::spawn_writer;
use crate::db::{self, queries};
use crate::error::Result;
use crate::model::{now_unix_ns, Phase};

use super::{
    pass_enumerate, pass_full, pass_prefix, pass_size, pass_verify, resume, Ctx, Services,
    VolumeScan,
};

pub struct ScanOptions {
    pub cli_args: String,
    pub resume: bool,
    pub stop_after: Option<i64>,
}

pub fn run_scan(
    db_path: &Path,
    roots: &[PathBuf],
    cfg: &Config,
    svc: &Services,
    opts: &ScanOptions,
) -> Result<i64> {
    let now = now_unix_ns();
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
    let writer = spawn_writer(db.conn, cfg.prefix_total() as i64);
    let ctx = Ctx {
        cfg,
        run_id,
        writer: &writer,
        read: &read,
        ids: &ids,
        svc,
    };

    let mut stopped = false;
    // With `--verify` off there is no Verified commit; treat a stop request at
    // Verified as a stop at FullDone so the hook always has a real boundary.
    let mut stop_after = opts.stop_after;
    if cfg.verify != VerifyMode::Byte && stop_after == Some(Phase::Verified.as_i64()) {
        stop_after = Some(Phase::FullDone.as_i64());
    }

    for v in &volumes {
        let mut phase = *phases.get(&v.volume_id).unwrap_or(&Phase::None);
        if stop_after == Some(0) {
            stopped = true;
            break;
        }

        if phase < Phase::Enumerated {
            pass_enumerate::run(&ctx, v)?;
            phase = Phase::Enumerated;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if phase < Phase::SizeDone {
            pass_size::run(&ctx, v.volume_id)?;
            phase = Phase::SizeDone;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if phase < Phase::PrefixDone {
            pass_prefix::run(&ctx, v.volume_id)?;
            phase = Phase::PrefixDone;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if phase < Phase::FullDone {
            pass_full::run(&ctx, v.volume_id)?;
            phase = Phase::FullDone;
            if stop_hit(stop_after, phase) {
                stopped = true;
                break;
            }
        }
        if cfg.verify == VerifyMode::Byte && phase < Phase::Verified {
            pass_verify::run(&ctx, v.volume_id)?;
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

    writer.shutdown()?;

    if !stopped {
        let db2 = db::Db::open(db_path, cfg)?;
        for v in &volumes {
            let phase = queries::get_phase(&db2.conn, run_id, v.volume_id)?;
            if phase < Phase::GroupsDone {
                queries::recompute_groups(&db2.conn, v.volume_id, now)?;
                queries::set_run_volume_phase(&db2.conn, run_id, v.volume_id, Phase::GroupsDone)?;
            }
        }
        queries::finish_run(&db2.conn, run_id, now)?;
    }

    Ok(run_id)
}

fn stop_hit(stop_after: Option<i64>, phase: Phase) -> bool {
    stop_after.is_some_and(|n| n <= phase.as_i64())
}
