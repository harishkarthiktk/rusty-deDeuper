//! Rescan support (4.7a): the enumerate-merge itself lives in
//! [`super::pass_enumerate`]; this module owns stale-row deletion, scoped to
//! this run's roots via a recursive CTE over `folders`.

use crate::db::writer::Batch;
use crate::error::Result;

use super::Ctx;

/// Queue deletion of files/folders not seen this run, under the given root folder ids.
/// No flush: the caller sends `PhaseComplete` next, so both commit in one transaction.
pub fn delete_stale(
    ctx: &Ctx,
    volume_id: i64,
    root_folder_ids: &[i64],
    protected_folder_ids: &[i64],
) -> Result<()> {
    if root_folder_ids.is_empty() {
        return Ok(());
    }
    ctx.writer.send(Batch::DeleteStale {
        run_id: ctx.run_id,
        volume_id,
        root_folder_ids: root_folder_ids.to_vec(),
        protected_folder_ids: protected_folder_ids.to_vec(),
    })
}
