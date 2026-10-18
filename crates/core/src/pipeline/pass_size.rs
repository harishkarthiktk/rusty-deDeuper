//! `pass_size`: classify state-0 rows into size candidates vs unique sizes.

use std::collections::HashSet;

use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::model::{Phase, State};

use super::Ctx;

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    ctx.svc.cancel.check()?;
    let candidate_sizes: HashSet<i64> = queries::size_candidates(ctx.read, volume_id, ctx.run_id)?
        .into_iter()
        .collect();

    let rows = queries::rows_in_state(ctx.read, volume_id, ctx.run_id, State::Enumerated)?;
    let mut candidates = Vec::new();
    let mut unique = Vec::new();
    for r in rows {
        if r.size > 0 && candidate_sizes.contains(&r.size) {
            candidates.push(r.id);
        } else {
            unique.push(r.id);
        }
    }

    ctx.writer.send(Batch::SizeCandidate(candidates))?;
    ctx.writer.send(Batch::UniqueSize(unique))?;
    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id,
        phase: Phase::SizeDone,
    })?;
    ctx.writer.flush()?;
    Ok(())
}
