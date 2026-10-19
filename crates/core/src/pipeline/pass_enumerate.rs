//! `pass_enumerate`: walk the roots, upsert folders/files, merge with any prior
//! rows (4.7a): keep hashes for unchanged files, null them for changed files,
//! detect moves via `(volume_id, file_id)` + `size` + `mtime_ns`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::db::queries;
use crate::db::writer::Batch;
use crate::error::Result;
use crate::model::{attrs, now_unix_ns, ErrRow, FileEntry, FolderRow, State};
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
        for suf in ["-wal", "-shm", ".lock"] {
            let mut os = db.as_os_str().to_os_string();
            os.push(suf);
            let sibling = PathBuf::from(os);
            s.insert(std::path::absolute(&sibling).unwrap_or(sibling));
        }
        s
    });

    let mut dir_ids: HashMap<RawPath, i64> = HashMap::new();
    let mut folders: FolderOverlay = HashMap::new();
    let mut folders_out: Vec<FolderRow> = Vec::new();
    let mut files_out: Vec<FileEntry> = Vec::new();
    let mut claims: HashMap<i64, Claim> = HashMap::new();
    let mut claimed_inodes: HashSet<Vec<u8>> = HashSet::new();
    let mut root_folder_ids: Vec<i64> = Vec::new();
    // Folders the walker could not read, protected from stale deletion, and the error
    // rows to record for them.
    let mut protected: Vec<i64> = Vec::new();
    let mut errors_out: Vec<ErrRow> = Vec::new();
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
    let mount_root = ensure_folder(
        ctx,
        v.volume_id,
        0,
        &RawPath::empty(),
        &mut folders,
        &mut folders_out,
    )?;
    dir_ids.insert(v.mount.clone(), mount_root);

    for root in &v.roots {
        ctx.svc.cancel.check()?;
        // Build the folder chain from the mount down to this scan root.
        let mut cur = mount_root;
        let mut acc = v.mount.clone();
        for comp in relative_components(root, &v.mount) {
            acc = acc.join_component(&comp);
            cur = ensure_folder(
                ctx,
                v.volume_id,
                cur,
                &RawPath::new(comp),
                &mut folders,
                &mut folders_out,
            )?;
            // `cur` is always the canonical id, so the cache and the chain agree.
            dir_ids.insert(acc.clone(), cur);
        }
        root_folder_ids.push(cur);

        let native_root = ctx.svc.codec.raw_to_path(root);
        let mut scope = DevScope::for_root(ctx, &native_root)?;
        ctx.svc.enumr.walk(&native_root, cfg, &mut |item| {
            ctx.svc.cancel.check()?;
            match item {
                WalkItem::Dir { path } => {
                    let native = ctx.svc.codec.raw_to_path(&path);
                    // An excluded folder is never registered, so the files below it find no
                    // parent in `dir_ids` and are dropped too. This covers the `$MFT` path,
                    // which cannot prune. The scan root itself is always kept.
                    if path != *root && cfg.exclude.is_excluded(&native) {
                        return Ok(());
                    }
                    let (parent_raw, name) = ctx.svc.codec.split_component(&native);
                    if let Some(&pid) = dir_ids.get(&parent_raw) {
                        let id = ensure_folder(
                            ctx,
                            v.volume_id,
                            pid,
                            &name,
                            &mut folders,
                            &mut folders_out,
                        )?;
                        dir_ids.entry(path).or_insert(id);
                    }
                    Ok(())
                }
                WalkItem::Error {
                    path,
                    is_dir,
                    kind,
                    message,
                } => {
                    let native = ctx.svc.codec.raw_to_path(&path);
                    let (parent_raw, name) = ctx.svc.codec.split_component(&native);
                    let mut row_id = None;
                    if is_dir {
                        // The dir entry is normally registered before it is read; if
                        // not, protect the parent (conservative: keep more rows).
                        if let Some(&id) = dir_ids.get(&path).or_else(|| dir_ids.get(&parent_raw)) {
                            protected.push(id);
                        }
                    } else if let Some(&fid) = dir_ids.get(&parent_raw) {
                        if let Some(mut e) =
                            queries::find_by_path(ctx.read, v.volume_id, fid, &name)?
                        {
                            if let std::collections::hash_map::Entry::Vacant(slot) =
                                claims.entry(e.id)
                            {
                                slot.insert(Claim::Path);
                                // Hashes kept: the row survives stale deletion and the next
                                // run re-checks it through the normal step 1 comparison.
                                e.state = State::Error;
                                e.err = Some(kind.as_i64());
                                e.last_seen_run_id = ctx.run_id;
                                row_id = Some(e.id);
                                files_out.push(e);
                            }
                        }
                    }
                    errors_out.push(ErrRow {
                        run_id: ctx.run_id,
                        file_row_id: row_id,
                        path: Some(path),
                        kind,
                        message: Some(message),
                    });
                    Ok(())
                }
                WalkItem::File { path, mut meta } => {
                    let native = ctx.svc.codec.raw_to_path(&path);
                    if skip.as_ref().is_some_and(|s| s.contains(&native))
                        || cfg.exclude.is_excluded(&native)
                    {
                        return Ok(());
                    }
                    scope.apply(ctx, &native, &mut meta);
                    let (parent_raw, name) = ctx.svc.codec.split_component(&native);
                    let Some(&folder_id) = dir_ids.get(&parent_raw) else {
                        return Ok(());
                    };
                    let Some(entry) = build_entry(
                        ctx,
                        v.volume_id,
                        folder_id,
                        &name,
                        &meta,
                        now,
                        &mut claims,
                        &mut claimed_inodes,
                    )?
                    else {
                        return Ok(());
                    };
                    files_out.push(entry);
                    accepted += 1;
                    if accepted.is_multiple_of(PROGRESS_CHUNK) {
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
    if !errors_out.is_empty() {
        ctx.writer
            .send(Batch::Error(std::mem::take(&mut errors_out)))?;
    }

    // Report the remainder that did not fill a whole chunk.
    let remainder = accepted % PROGRESS_CHUNK;
    if remainder > 0 {
        emit_advance(ctx.svc.sink, ctx.run_id, v.volume_id, remainder);
    }

    // Stale deletion rides in the same transaction as the phase marker (4.7a step 2),
    // so a crash cannot leave the phase set with the deletion skipped.
    rescan::delete_stale(ctx, v.volume_id, &root_folder_ids, &protected)?;
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

/// Per-root device and a cache of foreign-device tags (only populated with
/// `--cross-mounts`, the one way a Unix walk leaves the root's device).
struct DevScope {
    root_dev: Option<u64>,
    tags: HashMap<u64, [u8; 8]>,
}

impl DevScope {
    fn for_root(ctx: &Ctx, native_root: &std::path::Path) -> Result<Self> {
        // `dev` is `None` on Windows, which disables namespacing there (the walker cannot
        // leave the volume).
        let root_dev = ctx.svc.reader.metadata(native_root)?.dev;
        Ok(Self {
            root_dev,
            tags: HashMap::new(),
        })
    }

    /// Rewrites `meta.file_id` for an entry on a different device than its root, so ids
    /// from two filesystems cannot collide under one `volume_id`. The tag comes from the
    /// foreign volume's uuid, so it is stable across reboots and rescans keep their hashes.
    fn apply(&mut self, ctx: &Ctx, native: &std::path::Path, meta: &mut FileMeta) {
        let (Some(root), Some(dev)) = (self.root_dev, meta.dev) else {
            return;
        };
        if dev == root {
            return;
        }
        let Some(fid) = meta.file_id.as_deref() else {
            return;
        };
        let tag = *self
            .tags
            .entry(dev)
            .or_insert_with(|| match ctx.svc.volume.identify(native) {
                Ok(info) => crate::identity::volume_tag(&info.uuid),
                Err(e) => {
                    // Unstable but collision-free within this run: rescans re-hash.
                    tracing::warn!(dev, error = %e, "cannot identify nested mount; dev-based tag");
                    crate::identity::volume_tag(&format!("dev:{dev:016x}"))
                }
            });
        meta.file_id = Some(crate::identity::namespaced_file_id(tag, fid));
    }
}

/// Folders resolved this run on this volume: `(parent_id, name) -> id`.
/// Authoritative over `ctx.read`, which cannot see rows still queued for the writer.
type FolderOverlay = HashMap<(i64, RawPath), i64>;

/// Resolve `(volume, parent, name)` to a folder id: overlay first, then the committed
/// snapshot, then a new id. The upsert (which bumps `last_seen_run_id`, so stale
/// deletion keeps reused folders) is emitted once per run, on first resolution.
fn ensure_folder(
    ctx: &Ctx,
    volume_id: i64,
    parent_id: i64,
    name: &RawPath,
    overlay: &mut FolderOverlay,
    out: &mut Vec<FolderRow>,
) -> Result<i64> {
    let key = (parent_id, name.clone());
    if let Some(&id) = overlay.get(&key) {
        return Ok(id);
    }
    let id = match queries::find_folder(ctx.read, volume_id, parent_id, name)? {
        Some(id) => id,
        None => ctx.ids.folder(),
    };
    overlay.insert(key, id);
    out.push(FolderRow {
        id,
        volume_id,
        parent_id,
        name: name.clone(),
        last_seen_run_id: Some(ctx.run_id),
    });
    Ok(id)
}

/// How a pre-existing row was claimed this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// Found at the same path (step 1).
    Path,
    /// Taken by move detection for a different path (step 2).
    Move,
}

/// `None` when this exact path was already enumerated this run.
#[allow(clippy::too_many_arguments)]
fn build_entry(
    ctx: &Ctx,
    volume_id: i64,
    folder_id: i64,
    name: &RawPath,
    meta: &FileMeta,
    now: i64,
    claims: &mut HashMap<i64, Claim>,
    claimed_inodes: &mut HashSet<Vec<u8>>,
) -> Result<Option<FileEntry>> {
    let skip = skip_reason(meta, ctx.cfg.include_placeholders);

    // 1. Look up the existing row by path. The snapshot is stale for rows already
    // claimed this run, so check the claim before trusting it.
    if let Some(existing) = queries::find_by_path(ctx.read, volume_id, folder_id, name)? {
        match claims.get(&existing.id) {
            // Same path yielded twice (nested roots, `$MFT` fallback re-yield).
            Some(Claim::Path) => return Ok(None),
            // The row at this path moved away this run: this path holds a new file.
            Some(Claim::Move) => {}
            None => {
                claims.insert(existing.id, Claim::Path);
                let unchanged = skip.is_none() && unchanged_since(&existing, meta);
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
                return Ok(Some(e));
            }
        }
    }

    // 2. Move detection: same inode, size and mtime, not yet claimed this run.
    if skip.is_none() {
        if let Some(fid) = &meta.file_id {
            if !claimed_inodes.contains(fid) {
                for cand in queries::find_move_candidate(ctx.read, volume_id, fid, ctx.run_id)? {
                    if claims.contains_key(&cand.id) {
                        continue;
                    }
                    if cand.size == meta.size && cand.mtime_ns == meta.mtime_ns {
                        claimed_inodes.insert(fid.clone());
                        claims.insert(cand.id, Claim::Move);
                        let mut e = cand;
                        e.folder_id = folder_id;
                        e.name = name.clone();
                        apply_meta(&mut e, meta, now, ctx.run_id);
                        e.state = State::Enumerated;
                        return Ok(Some(e));
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
    Ok(Some(e))
}

/// P1.7.1: hashes survive a rescan only if size, mtime, ctime and file id match. ctime
/// catches a rewrite that restores mtime (`touch -r`, `cp -p`, archive extraction).
fn unchanged_since(existing: &FileEntry, meta: &FileMeta) -> bool {
    existing.size == meta.size
        && existing.mtime_ns == meta.mtime_ns
        && super::pass_prefix::ctime_matches(existing.ctime_ns, meta.ctime_ns)
        && existing.file_id == meta.file_id
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
