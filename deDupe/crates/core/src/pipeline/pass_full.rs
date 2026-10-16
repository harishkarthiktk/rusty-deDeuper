//! `pass_full`: full-hash every prefix survivor (state 2). Without `--verify`,
//! the file lands in state 5 directly (3 is transient).

use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::hash::hash_full;
use crate::model::{ErrRow, Phase, State};
use crate::progress::{emit_advance, emit_finished, emit_started, PhaseProgress};

use super::pass_prefix::{changed_err, io_err, unchanged};
use super::{Ctx, PathCache};

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    let rows = queries::rows_in_state(ctx.read, volume_id, ctx.run_id, State::PrefixHashed)?;
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
    let mut cache = PathCache::new(ctx.read);
    let mut hashed: Vec<(i64, crate::model::Hash)> = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();

    for r in &rows {
        ctx.svc.cancel.check()?;
        // One advance per row, whether its hash is reused or computed.
        emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        // Reuse the stored full hash for unchanged files.
        if let Some(f) = r.full_hash {
            if r.prefix_bytes == Some(ctx.cfg.prefix_total() as i64) {
                hashed.push((r.id, f));
                continue;
            }
        }
        let path = cache.file_path(r.folder_id, &r.name)?;
        let native = ctx.svc.codec.raw_to_path(&path);
        match hash_full(ctx.svc.reader, &native, r.size as u64) {
            Ok(h) => match unchanged(ctx, &native, r) {
                Ok(true) => hashed.push((r.id, h)),
                Ok(false) => errors.push(changed_err(ctx, r, &path)),
                Err(e) => errors.push(io_err(ctx, r, &path, e)),
            },
            Err(e) => errors.push(io_err(ctx, r, &path, e)),
        }
    }

    ctx.writer.send(Batch::FullHashed(hashed))?;
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
