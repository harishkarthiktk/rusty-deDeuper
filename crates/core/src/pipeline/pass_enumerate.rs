//! `pass_enumerate`: walk the roots, upsert folders/files, merge with any prior
//! rows (4.7a): keep hashes for unchanged files, null them for changed files,
//! detect moves via `(volume_id, file_id)` + `size` + `mtime_ns`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::model::{attrs, now_unix_ns, FileEntry, FolderRow, State};
use crate::path::RawPath;
use crate::progress::{emit_advance, emit_finished, emit_started, PhaseProgress};
use crate::traits::{EntryKind, FileMeta, WalkItem};

use super::{rescan, Ctx, VolumeScan};

const BATCH: usize = 5_000;
/// Walk progress is reported every this many accepted rows (not per entry).
const PROGRESS_CHUNK: u64 = 1_024;

/// Returns the root folder ids for this volume (for stale deletion scoping).
pub fn run(ctx: &Ctx, v: &VolumeScan) -> Result<Vec<i64>> {
    let cfg = ctx.cfg;
    let now = now_unix_ns();

    // Skip the DB file and its WAL/SHM siblings when they live under a scanned root,
    // so a scan never indexes (or hashes) its own database.
    let skip: Option<HashSet<PathBuf>> = cfg.db_path.as_ref().map(|db| {
        let mut s = HashSet::new();
        s.insert(std::path::absolute(db).unwrap_or_else(|_| db.clone()));
        for suf in ["-wal", "-shm"] {
            let mut os = db.as_os_str().to_os_string();
            os.push(suf);
            let sibling = PathBuf::from(os);
            s.insert(std::path::absolute(&sibling).unwrap_or(sibling));
        }
        s
    });

    let mut dir_ids: HashMap<RawPath, i64> = HashMap::new();
    let mut folders_out: Vec<FolderRow> = Vec::new();
    let mut files_out: Vec<FileEntry> = Vec::new();
    let mut seen_ids: HashSet<i64> = HashSet::new();
    let mut claimed_inodes: HashSet<Vec<u8>> = HashSet::new();
    let mut root_folder_ids: Vec<i64> = Vec::new();
    // Accepted file rows so far; reported to the sink in chunks of 1024.
    let mut accepted: u64 = 0;

    emit_started(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id: v.volume_id,
            phase: crate::model::Phase::Enumerated,
            done: 0,
            total: None,
        },
    );

    // The volume mount root is folder(volume, parent=0, name='').
    let mount_root = ensure_folder(ctx, v.volume_id, 0, &RawPath::empty(), &mut folders_out)?;
    dir_ids.insert(v.mount.clone(), mount_root);

    for root in &v.roots {
        ctx.svc.cancel.check()?;
        // Build the folder chain from the mount down to this scan root.
        let mut cur = mount_root;
        let mut acc = v.mount.clone();
        for comp in relative_components(root, &v.mount) {
            acc = acc.join_component(&comp);
            let id = ensure_folder(ctx, v.volume_id, cur, &RawPath::new(comp), &mut folders_out)?;
            dir_ids.entry(acc.clone()).or_insert(id);
            cur = id;
        }
        root_folder_ids.push(cur);

        let native_root = ctx.svc.codec.raw_to_path(root);
        ctx.svc.enumr.walk(&native_root, cfg, &mut |item| {
            ctx.svc.cancel.check()?;
            match item {
                WalkItem::Dir { path } => {
                    let native = ctx.svc.codec.raw_to_path(&path);
                    let (parent_raw, name) = ctx.svc.codec.split_component(&native);
                    if let Some(&pid) = dir_ids.get(&parent_raw) {
                        let id = ensure_folder(ctx, v.volume_id, pid, &name, &mut folders_out)?;
                        dir_ids.entry(path).or_insert(id);
                    }
                    Ok(())
                }
                WalkItem::File { path, meta } => {
                    let native = ctx.svc.codec.raw_to_path(&path);
                    if skip.as_ref().is_some_and(|s| s.contains(&native)) {
                        return Ok(());
                    }
                    let (parent_raw, name) = ctx.svc.codec.split_component(&native);
                    let Some(&folder_id) = dir_ids.get(&parent_raw) else {
                        return Ok(());
                    };
                    let entry = build_entry(
                        ctx,
                        v.volume_id,
                        folder_id,
                        &name,
                        &meta,
                        now,
                        &mut seen_ids,
                        &mut claimed_inodes,
                    )?;
                    files_out.push(entry);
                    accepted += 1;
                    if accepted % PROGRESS_CHUNK == 0 {
                        emit_advance(ctx.svc.sink, ctx.run_id, v.volume_id, PROGRESS_CHUNK);
                    }
                    if files_out.len() >= BATCH || folders_out.len() >= BATCH {
                        flush_folders(ctx, &mut folders_out)?;
                        flush_files(ctx, &mut files_out)?;
                    }
                    Ok(())
                }
            }
        })?;
    }

    flush_folders(ctx, &mut folders_out)?;
    flush_files(ctx, &mut files_out)?;

    // Report the remainder that did not fill a whole chunk.
    let remainder = accepted % PROGRESS_CHUNK;
    if remainder > 0 {
        emit_advance(ctx.svc.sink, ctx.run_id, v.volume_id, remainder);
    }

    // Phase marker: shares the txn with the last rows.
    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id: v.volume_id,
        phase: crate::model::Phase::Enumerated,
    })?;
    ctx.writer.flush()?;
    emit_finished(
        ctx.svc.sink,
        &PhaseProgress {
            run_id: ctx.run_id,
            volume_id: v.volume_id,
            phase: crate::model::Phase::Enumerated,
            done: accepted,
            total: Some(accepted),
        },
    );

    // Stale deletion after enumeration is committed (4.7a step 2).
    rescan::delete_stale(ctx, v.volume_id, &root_folder_ids)?;

    Ok(root_folder_ids)
}

fn flush_folders(ctx: &Ctx, out: &mut Vec<FolderRow>) -> Result<()> {
    if !out.is_empty() {
        ctx.writer.send(Batch::Folders(std::mem::take(out)))?;
    }
    Ok(())
}

fn flush_files(ctx: &Ctx, out: &mut Vec<FileEntry>) -> Result<()> {
    if !out.is_empty() {
        ctx.writer.send(Batch::Enumerated(std::mem::take(out)))?;
    }
    Ok(())
}

/// Ensure a folder row exists. Always emits an upsert so `last_seen_run_id` is
/// bumped for reused folders (otherwise stale deletion would remove them).
fn ensure_folder(
    ctx: &Ctx,
    volume_id: i64,
    parent_id: i64,
    name: &RawPath,
    out: &mut Vec<FolderRow>,
) -> Result<i64> {
    let id = match queries::find_folder(ctx.read, volume_id, parent_id, name)? {
        Some(id) => id,
        None => ctx.ids.folder(),
    };
    out.push(FolderRow {
        id,
        volume_id,
        parent_id,
        name: name.clone(),
        last_seen_run_id: Some(ctx.run_id),
    });
    Ok(id)
}

#[allow(clippy::too_many_arguments)]
fn build_entry(
    ctx: &Ctx,
    volume_id: i64,
    folder_id: i64,
    name: &RawPath,
    meta: &FileMeta,
    now: i64,
    seen_ids: &mut HashSet<i64>,
    claimed_inodes: &mut HashSet<Vec<u8>>,
) -> Result<FileEntry> {
    let skip = skip_reason(meta, ctx.cfg.include_placeholders);

    // 1. Look up the existing row by path.
    if let Some(existing) = queries::find_by_path(ctx.read, volume_id, folder_id, name)? {
        seen_ids.insert(existing.id);
        let unchanged = skip.is_none()
            && existing.size == meta.size
            && existing.mtime_ns == meta.mtime_ns
            && existing.file_id == meta.file_id;
        let mut e = existing;
        apply_meta(&mut e, meta, now, ctx.run_id);
        e.state = match skip {
            Some(_) => State::Skipped,
            None => State::Enumerated,
        };
        if !unchanged {
            e.prefix_hash = None;
            e.full_hash = None;
            e.prefix_bytes = None;
        }
        return Ok(e);
    }

    // 2. Move detection: same inode, size and mtime, not yet seen this run.
    if skip.is_none() {
        if let Some(fid) = &meta.file_id {
            if !claimed_inodes.contains(fid) {
                for cand in queries::find_move_candidate(ctx.read, volume_id, fid, ctx.run_id)? {
                    if seen_ids.contains(&cand.id) {
                        continue;
                    }
                    if cand.size == meta.size && cand.mtime_ns == meta.mtime_ns {
                        claimed_inodes.insert(fid.clone());
                        seen_ids.insert(cand.id);
                        let mut e = cand;
                        e.folder_id = folder_id;
                        e.name = name.clone();
                        apply_meta(&mut e, meta, now, ctx.run_id);
                        e.state = State::Enumerated;
                        return Ok(e);
                    }
                }
            }
        }
    }

    // 3. New row.
    let mut e = FileEntry::enumerated(
        ctx.ids.file(),
        volume_id,
        folder_id,
        name.clone(),
        ctx.run_id,
    );
    apply_meta(&mut e, meta, now, ctx.run_id);
    e.state = match skip {
        Some(_) => State::Skipped,
        None => State::Enumerated,
    };
    Ok(e)
}

fn apply_meta(e: &mut FileEntry, meta: &FileMeta, now: i64, run_id: i64) {
    e.file_id = meta.file_id.clone();
    e.nlink = meta.nlink;
    e.size = meta.size;
    e.mtime_ns = meta.mtime_ns;
    e.ctime_ns = meta.ctime_ns;
    e.attrs = meta.attrs;
    e.scanned_at = Some(now);
    e.err = None;
    e.last_seen_run_id = run_id;
}

fn skip_reason(meta: &FileMeta, include_placeholders: bool) -> Option<()> {
    if meta.kind != EntryKind::RegularFile {
        return Some(());
    }
    if meta.size <= 0 {
        return Some(());
    }
    if attrs::has(meta.attrs, attrs::PLACEHOLDER) && !include_placeholders {
        return Some(());
    }
    None
}

/// Components of `root` relative to `mount`. Empty when root == mount.
pub fn relative_components(root: &RawPath, mount: &RawPath) -> Vec<Vec<u8>> {
    let m = mount.as_bytes();
    let r = root.as_bytes();
    let rest: &[u8] = if !m.is_empty() && r.starts_with(m) {
        &r[m.len()..]
    } else {
        r
    };
    split_components(rest)
}

fn split_components(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split(|&b| b == b'/' || b == b'\\')
        .filter(|c| !c.is_empty())
        .map(|c| c.to_vec())
        .collect()
}
