//! `pass_verify`: byte-compare group members. `--verify=byte` lands in Phase 2;
//! in Phase 1 this is a no-op that still advances the phase when requested.

use crate::db::writer::Batch;
use crate::error::Result;
use crate::model::Phase;

use super::Ctx;

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    // Phase 1 never requests byte verification (the CLI has no --verify flag).
    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id,
        phase: Phase::Verified,
    })?;
    ctx.writer.flush()?;
    Ok(())
}
