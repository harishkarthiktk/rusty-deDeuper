//! `pass_verify`: byte-compare hash-equal files (state 3) under `--verify=byte`.
//! Survivors move to state 4, mismatches and read failures to -2 with an
//! `errors` row. Hardlinks sharing an inode are compared once.

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::{CoreError, Result};
use crate::hash::read_full;
use crate::model::{ErrRow, ErrorKind, FileEntry, Hash, Phase, State};
use crate::progress::{emit_advance, emit_finished, emit_started, PhaseProgress};
use crate::traits::{AccessPattern, ContentReader, ReadTarget};

use super::pass_prefix::{changed_err, io_err, unchanged};
use super::{send_chunk, Ctx, PathCache, RESULT_CHUNK};

/// Which file of a compared pair a read error came from.
#[derive(Clone, Copy)]
enum Side {
    Reference,
    Candidate,
}

/// Result of byte-comparing two distinct inodes.
enum Verdict {
    Equal,
    Differ,
    Failed(ErrRow),
}

/// Outcome for one distinct inode (one hardlink collapse unit) in a group.
#[derive(Clone)]
enum RepState {
    Verified,
    /// Error kind and message recorded on every row of the inode.
    Failed(ErrorKind, String),
}

pub fn run(ctx: &Ctx, volume_id: i64) -> Result<()> {
    let rows = queries::rows_in_state(ctx.read, volume_id, ctx.run_id, State::FullHashed)?;
    if rows.is_empty() && has_done_rows(ctx, volume_id)? {
        tracing::warn!(
            volume_id,
            "no state-3 rows to byte-verify; rows already in state 5 were not byte-verified"
        );
    }
    let total = rows.len() as u64;
    emit_started(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::Verified,
            done: 0,
            total: Some(total),
        },
    );

    let mut cache = PathCache::new(ctx.read);
    let mut verified: Vec<i64> = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();

    // Group by content key. A row without a full hash cannot be compared.
    let mut by_key: HashMap<(Hash, i64), Vec<&FileEntry>> = HashMap::new();
    for r in &rows {
        match r.full_hash {
            Some(h) => by_key.entry((h, r.size)).or_default().push(r),
            None => {
                errors.push(member_err(
                    ctx,
                    &mut cache,
                    r,
                    ErrorKind::Other,
                    "missing full hash before verify",
                )?);
                emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
            }
        }
    }
    // Deterministic order: members by id, groups by their lowest id.
    let mut groups: Vec<Vec<&FileEntry>> = by_key.into_values().collect();
    for g in &mut groups {
        g.sort_unstable_by_key(|r| r.id);
    }
    groups.sort_unstable_by_key(|g| g.first().map(|r| r.id));

    let res = (|| -> Result<()> {
        for group in &groups {
            ctx.svc.cancel.check()?;
            let verdicts = verify_group(ctx, &mut cache, group)?;
            for (r, verdict) in group.iter().zip(verdicts) {
                match verdict {
                    None => verified.push(r.id),
                    Some(e) => errors.push(e),
                }
            }
            // One advance per row, so the sum equals `total`.
            emit_advance(ctx.svc.sink, ctx.run_id, volume_id, group.len() as u64);
            // Whole groups only. Errors go first: a commit that splits the two batches
            // must never leave a lone mismatched member in state 3, which a resume
            // would promote as a singleton.
            if verified.len() + errors.len() >= RESULT_CHUNK {
                send_chunk(ctx, &mut errors, true, Batch::Error)?;
                send_chunk(ctx, &mut verified, true, Batch::Verified)?;
            }
        }
        Ok(())
    })();
    // Hand over finished groups even when the loop stopped (cancel, error).
    send_chunk(ctx, &mut errors, true, Batch::Error)?;
    send_chunk(ctx, &mut verified, true, Batch::Verified)?;
    res?;

    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id,
        phase: Phase::Verified,
    })?;
    ctx.writer.flush()?;
    emit_finished(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id,
            phase: Phase::Verified,
            done: total,
            total: Some(total),
        },
    );
    Ok(())
}

/// Verdict per group member, aligned with `group`: `None` is verified, `Some`
/// is the error row to record.
fn verify_group(
    ctx: &Ctx,
    cache: &mut PathCache<'_>,
    group: &[&FileEntry],
) -> Result<Vec<Option<ErrRow>>> {
    let (reps, rep_of) = collapse_hardlinks(group);
    let states = if reps.len() == 1 {
        // Singleton or all hardlinks of one inode: nothing to compare.
        vec![RepState::Verified]
    } else {
        classify_reps(ctx, cache, &reps)?
    };
    group
        .iter()
        .zip(&rep_of)
        .map(|(r, &j)| match &states[j] {
            RepState::Verified => Ok(None),
            RepState::Failed(kind, msg) => member_err(ctx, cache, r, *kind, msg).map(Some),
        })
        .collect()
}

/// Split a group into distinct inodes. Returns the representatives in id order
/// and, for each member, the index of its representative.
fn collapse_hardlinks<'a>(group: &[&'a FileEntry]) -> (Vec<&'a FileEntry>, Vec<usize>) {
    let mut reps: Vec<&'a FileEntry> = Vec::new();
    let mut by_inode: HashMap<&[u8], usize> = HashMap::new();
    let mut rep_of = Vec::with_capacity(group.len());
    for &r in group {
        let idx = match r.file_id.as_deref() {
            Some(fid) => *by_inode.entry(fid).or_insert_with(|| {
                reps.push(r);
                reps.len() - 1
            }),
            None => {
                reps.push(r);
                reps.len() - 1
            }
        };
        rep_of.push(idx);
    }
    (reps, rep_of)
}

/// Cluster the representatives by byte equality, anchored on the lowest id of
/// each cluster. The largest cluster (ties: earliest) is verified; other
/// clusters are mismatches. If an anchor itself fails, the whole group fails.
fn classify_reps(
    ctx: &Ctx,
    cache: &mut PathCache<'_>,
    reps: &[&FileEntry],
) -> Result<Vec<RepState>> {
    let mut clusters: Vec<Vec<usize>> = Vec::new();
    let mut failed: Vec<Option<ErrRow>> = vec![None; reps.len()];
    let chunk = ctx.cfg.profile.buf_size;
    'reps: for j in 0..reps.len() {
        for c in clusters.iter_mut() {
            let anchor = reps[c[0]];
            match compare_pair(ctx, cache, anchor, reps[j], chunk)? {
                Verdict::Equal => {
                    c.push(j);
                    continue 'reps;
                }
                Verdict::Differ => {}
                Verdict::Failed(e) if e.file_row_id == Some(anchor.id) => {
                    return Ok(abort_states(reps.len(), c[0], &e));
                }
                Verdict::Failed(e) => {
                    failed[j] = Some(e);
                    continue 'reps;
                }
            }
        }
        clusters.push(vec![j]);
    }

    let best = (0..clusters.len())
        .max_by_key(|&i| (clusters[i].len(), Reverse(i)))
        .unwrap_or(0);
    let mut states = vec![RepState::Verified; reps.len()];
    for (ci, c) in clusters.iter().enumerate() {
        if ci != best {
            for &j in c {
                states[j] = RepState::Failed(
                    ErrorKind::VerifyMismatch,
                    "bytes differ from duplicate group".into(),
                );
            }
        }
    }
    for (j, e) in failed.into_iter().enumerate() {
        if let Some(e) = e {
            states[j] = RepState::Failed(e.kind, e.message.unwrap_or_default());
        }
    }
    Ok(states)
}

/// Every representative fails: the failing anchor keeps its own message, the
/// others record that the group was aborted.
fn abort_states(n: usize, failing: usize, cause: &ErrRow) -> Vec<RepState> {
    let own = cause.message.as_deref().unwrap_or_default();
    (0..n)
        .map(|j| {
            let msg = if j == failing {
                own.to_string()
            } else {
                "verify aborted: reference file failed".into()
            };
            RepState::Failed(cause.kind, msg)
        })
        .collect()
}

/// Byte-compare two distinct inodes, then re-stat both: a file changed since
/// hashing makes the comparison meaningless, so it is reported as changed.
fn compare_pair(
    ctx: &Ctx,
    cache: &mut PathCache<'_>,
    a: &FileEntry,
    b: &FileEntry,
    chunk: usize,
) -> Result<Verdict> {
    let pa = cache.file_path(a.folder_id, &a.name)?;
    let pb = cache.file_path(b.folder_id, &b.name)?;
    let na = ctx.svc.codec.raw_to_path(&pa);
    let nb = ctx.svc.codec.raw_to_path(&pb);
    let size = match a.size_bytes() {
        Ok(size) => size,
        Err(e) => return Ok(Verdict::Failed(io_err(ctx, a, &pa, e))),
    };
    let equal = match verify_pair(
        ctx.svc.reader,
        ReadTarget {
            path: &na,
            file_id: a.file_id.as_deref(),
            access: AccessPattern::Sequential,
        },
        ReadTarget {
            path: &nb,
            file_id: b.file_id.as_deref(),
            access: AccessPattern::Sequential,
        },
        size,
        chunk,
    ) {
        Ok(eq) => eq,
        Err((Side::Reference, e)) => return Ok(Verdict::Failed(io_err(ctx, a, &pa, e))),
        Err((Side::Candidate, e)) => return Ok(Verdict::Failed(io_err(ctx, b, &pb, e))),
    };
    for (r, p, n) in [(a, &pa, &na), (b, &pb, &nb)] {
        match unchanged(ctx, n, r) {
            Ok(true) => {}
            Ok(false) => return Ok(Verdict::Failed(changed_err(ctx, r, p))),
            Err(e) => return Ok(Verdict::Failed(io_err(ctx, r, p, e))),
        }
    }
    Ok(if equal {
        Verdict::Equal
    } else {
        Verdict::Differ
    })
}

/// Chunked byte compare. `Ok(false)` on the first differing chunk, on a chunk
/// still short after [`read_full`], or if either file extends past `size`.
/// Errors name the failing side. Each file is opened once, so a pair costs two
/// opens regardless of its size. `chunk` is the read size per step.
fn verify_pair(
    reader: &dyn ContentReader,
    a: ReadTarget<'_>,
    b: ReadTarget<'_>,
    size: u64,
    chunk: usize,
) -> std::result::Result<bool, (Side, CoreError)> {
    // A zero chunk would never advance `offset`, so the loop below would not end.
    let chunk = chunk.max(1);
    let mut ha = reader.open_target(a).map_err(|e| (Side::Reference, e))?;
    let mut hb = reader.open_target(b).map_err(|e| (Side::Candidate, e))?;
    let mut buf_a = vec![0u8; chunk];
    let mut buf_b = vec![0u8; chunk];
    let mut offset = 0u64;
    while offset < size {
        // Clamp in u64 before narrowing, so a large remainder cannot truncate on 32-bit targets.
        let want = (size - offset).min(chunk as u64) as usize;
        let na =
            read_full(ha.as_mut(), offset, &mut buf_a[..want]).map_err(|e| (Side::Reference, e))?;
        let nb =
            read_full(hb.as_mut(), offset, &mut buf_b[..want]).map_err(|e| (Side::Candidate, e))?;
        if na != want || nb != want || buf_a[..want] != buf_b[..want] {
            return Ok(false);
        }
        offset += want as u64;
    }
    let mut probe = [0u8; 1];
    for (side, h) in [
        (Side::Reference, ha.as_mut()),
        (Side::Candidate, hb.as_mut()),
    ] {
        let n = h.read_at(size, &mut probe).map_err(|e| (side, e))?;
        if n != 0 {
            return Ok(false);
        }
    }
    Ok(true)
}

fn member_err(
    ctx: &Ctx,
    cache: &mut PathCache<'_>,
    r: &FileEntry,
    kind: ErrorKind,
    msg: &str,
) -> Result<ErrRow> {
    let path = cache.file_path(r.folder_id, &r.name)?;
    Ok(ErrRow {
        run_id: ctx.run_id,
        file_row_id: Some(r.id),
        path: Some(path),
        kind,
        message: Some(msg.to_string()),
    })
}

/// Cheap existence check, so the warning does not load every state-5 row.
fn has_done_rows(ctx: &Ctx, volume_id: i64) -> Result<bool> {
    Ok(ctx.read.query_row(
        "SELECT EXISTS(SELECT 1 FROM files WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=?3)",
        rusqlite::params![volume_id, ctx.run_id, State::Done.as_i64()],
        |r| r.get(0),
    )?)
}
