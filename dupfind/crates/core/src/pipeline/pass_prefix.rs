//! `pass_prefix`: hash the prefix of every size candidate. Small files
//! (`size <= F + L`) get both hashes in one `Hashed` batch and skip `pass_full`.
//! Falls back to DB-backed partner lookup when the memory map exceeds budget.

use std::path::Path;

use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::hash::{hash_full, hash_prefix};
use crate::index::{Index, MemoryIndex};
use crate::model::{ErrRow, ErrorKind, FileEntry, Hash, Phase, State};
use crate::path::RawPath;

use super::{Ctx, PathCache};

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    let cfg = ctx.cfg;
    let total = cfg.prefix_total();
    let rows = queries::rows_in_state(ctx.read, volume_id, ctx.run_id, State::SizeCandidate)?;

    let mut index = MemoryIndex::new(cfg.mem_budget);
    for (id, h) in queries::rows_with_prefix(ctx.read, volume_id, ctx.run_id)? {
        index.insert(h, id)?;
    }

    let mut cache = PathCache::new(ctx.read);
    let mut small: Vec<(i64, Hash, Hash)> = Vec::new();
    let mut large: Vec<(i64, Hash)> = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();

    for r in &rows {
        ctx.svc.cancel.check()?;

        // Reuse a stored hash when `prefix_bytes` matches the current config, so
        // an unchanged rescan does no I/O (4.7a step 3).
        if (r.size as u64) <= total {
            if let (Some(p), Some(f)) = (r.prefix_hash, r.full_hash) {
                if r.prefix_bytes == Some(r.size) {
                    small.push((r.id, p, f));
                    continue;
                }
            }
        } else if let Some(p) = r.prefix_hash {
            if r.prefix_bytes == Some(total as i64) {
                large.push((r.id, p));
                continue;
            }
        }

        let path = cache.file_path(r.folder_id, &r.name)?;
        let native = ctx.svc.codec.raw_to_path(&path);

        if (r.size as u64) <= total {
            match hash_full(ctx.svc.reader, &native, r.size as u64) {
                Ok(h) => match unchanged(ctx, &native, r) {
                    Ok(true) => small.push((r.id, h, h)),
                    Ok(false) => errors.push(changed_err(ctx, r, &path)),
                    Err(e) => errors.push(io_err(ctx, r, &path, e)),
                },
                Err(e) => errors.push(io_err(ctx, r, &path, e)),
            }
        } else {
            match hash_prefix(ctx.svc.reader, &native, r.size as u64, cfg) {
                Ok((h, _)) => match unchanged(ctx, &native, r) {
                    Ok(true) => large.push((r.id, h)),
                    Ok(false) => errors.push(changed_err(ctx, r, &path)),
                    Err(e) => errors.push(io_err(ctx, r, &path, e)),
                },
                Err(e) => errors.push(io_err(ctx, r, &path, e)),
            }
        }
    }

    for (id, h) in &large {
        index.insert(*h, *id)?;
    }

    if index.over_budget() {
        tracing::warn!(
            volume_id,
            "prefix_map over budget: DB-backed classification"
        );
        ctx.writer.send(Batch::Hashed(small))?;
        ctx.writer.send(Batch::PrefixHashed(large.clone()))?;
        send_errors(ctx, &mut errors)?;
        ctx.writer.send(Batch::PhaseComplete {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::PrefixDone,
        })?;
        ctx.writer.flush()?;
        let singles: std::collections::HashSet<i64> =
            queries::singleton_prefixes(ctx.read, volume_id, ctx.run_id)?
                .into_iter()
                .collect();
        let uniques: Vec<(i64, Hash)> = large
            .into_iter()
            .filter(|(id, _)| singles.contains(id))
            .collect();
        ctx.writer.send(Batch::UniquePrefix(uniques))?;
    } else {
        let mut partnered = Vec::new();
        let mut unique = Vec::new();
        for (id, h) in &large {
            if index.count(h) > 1 {
                partnered.push((*id, *h));
            } else {
                unique.push((*id, *h));
            }
        }
        ctx.writer.send(Batch::Hashed(small))?;
        ctx.writer.send(Batch::PrefixHashed(partnered))?;
        ctx.writer.send(Batch::UniquePrefix(unique))?;
        send_errors(ctx, &mut errors)?;
        ctx.writer.send(Batch::PhaseComplete {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::PrefixDone,
        })?;
    }
    ctx.writer.flush()?;
    Ok(())
}

fn send_errors(ctx: &Ctx, errors: &mut Vec<ErrRow>) -> Result<()> {
    if !errors.is_empty() {
        ctx.writer.send(Batch::Error(std::mem::take(errors)))?;
    }
    Ok(())
}

/// Re-stat and compare. `Ok(false)` means the file changed during the read.
pub(crate) fn unchanged(ctx: &Ctx, native: &Path, r: &FileEntry) -> Result<bool> {
    let md = ctx.svc.reader.metadata(native)?;
    let ctime_ok = match (r.ctime_ns, md.ctime_ns) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    Ok(md.size == r.size && md.mtime_ns == r.mtime_ns && ctime_ok)
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
