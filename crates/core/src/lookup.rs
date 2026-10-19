//! `lookup`: check whether a new file duplicates anything already indexed (4.8).
//! Read-only by default; `--insert` persists on-demand hashes and the query row in one
//! transaction. The caller must hold `db::lock::acquire` while inserting.

use std::path::Path;

use rusqlite::{params, Connection, TransactionBehavior};

use crate::config::Config;
use crate::db::queries;
use crate::error::{CoreError, Result};
use crate::hash::{hash_full, hash_prefix};
use crate::model::{attrs, FileEntry, Hash, SkipReason, State};
use crate::path::{separator_for, strip_mount, RawPath};
use crate::traits::{ContentReader, EntryKind, FileMeta, PathCodec, VolumeIdentity};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Missing,
    Unmounted,
}

#[derive(Clone, Debug)]
pub struct MatchRow {
    pub volume_id: i64,
    pub path: RawPath,
    pub freshness: Freshness,
}

#[derive(Clone, Debug)]
pub enum LookupOutcome {
    Skipped(SkipReason),
    NoMatch,
    Matches(Vec<MatchRow>),
}

/// What `--insert` did with the query file's own row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertStatus {
    NotRequested,
    Inserted {
        id: i64,
    },
    Updated {
        id: i64,
    },
    /// Row already indexed, fresh and fully hashed: left alone.
    Unchanged {
        id: i64,
    },
    SkippedVolumeNotIndexed,
    SkippedFolderNotIndexed,
}

#[derive(Clone, Debug)]
pub struct LookupReport {
    pub outcome: LookupOutcome,
    pub insert: InsertStatus,
}

struct Candidate {
    id: i64,
    volume_id: i64,
    folder_id: i64,
    name: RawPath,
    file_id: Option<Vec<u8>>,
    size: i64,
    mtime_ns: i64,
    prefix_hash: Option<Hash>,
    full_hash: Option<Hash>,
}

impl Candidate {
    /// `None` for a corrupt negative size. The caller skips the row: one bad
    /// row must not fail a lookup.
    fn size_bytes(&self) -> Option<u64> {
        u64::try_from(self.size).ok()
    }
}

/// Where the query file sits in the index. `folder_id`/`existing` are `None`
/// when the parent folder was never scanned.
struct QueryKey {
    volume_id: i64,
    folder_id: Option<i64>,
    name: RawPath,
    existing: Option<FileEntry>,
}

/// One on-demand hash to persist, with the metadata it was computed against.
struct OnDemand {
    id: i64,
    size: i64,
    mtime_ns: i64,
    prefix: Option<Hash>,
    full: Option<Hash>,
}

/// Result of the read half: the outcome plus what `--insert` may write.
struct Probe {
    outcome: LookupOutcome,
    q_prefix: Option<Hash>,
    q_full: Option<Hash>,
    on_demand: Vec<OnDemand>,
}

/// Read-only lookup, or lookup plus `--insert` when `insert` is set.
/// Callers that need the insert result use [`lookup_report`].
pub fn lookup(
    conn: &Connection,
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
    cfg: &Config,
    insert: bool,
) -> Result<LookupOutcome> {
    lookup_report(conn, reader, codec, vi, path, cfg, insert).map(|r| r.outcome)
}

pub fn lookup_report(
    conn: &Connection,
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
    cfg: &Config,
    insert: bool,
) -> Result<LookupReport> {
    // 1. stat the query file.
    let meta = reader.metadata(path)?;
    let skipped = |reason| {
        Ok(LookupReport {
            outcome: LookupOutcome::Skipped(reason),
            insert: InsertStatus::NotRequested,
        })
    };
    match meta.kind {
        EntryKind::RegularFile => {}
        EntryKind::Symlink => return skipped(SkipReason::Symlink),
        _ => return skipped(SkipReason::NonRegular),
    }
    if meta.size == 0 {
        return skipped(SkipReason::ZeroLength);
    }
    if attrs::has(meta.attrs, attrs::PLACEHOLDER) && !cfg.include_placeholders {
        return skipped(SkipReason::Placeholder);
    }
    // A fresh stat cannot be negative, so a failure here is a real error for the query.
    let size = meta.size_bytes()?;

    // Where the query lives in the index, so it never matches itself. `None` only when the
    // query's volume is not indexed: no candidate can share that volume, so nothing needs
    // excluding. An identity failure is an error in both modes.
    let key = resolve_query_key(conn, codec, vi, path).map_err(|e| {
        CoreError::Other(format!(
            "cannot identify query file's volume; needed to exclude the file itself: {e}"
        ))
    })?;

    let probe = probe_index(reader, codec, conn, path, &meta, size, key.as_ref(), cfg)?;

    let status = if insert {
        apply_insert(conn, key.as_ref(), &meta, &probe, cfg.prefix_total() as i64)?
    } else {
        InsertStatus::NotRequested
    };
    Ok(LookupReport {
        outcome: probe.outcome,
        insert: status,
    })
}

#[allow(clippy::too_many_arguments)]
fn probe_index(
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    conn: &Connection,
    path: &Path,
    meta: &FileMeta,
    size: u64,
    key: Option<&QueryKey>,
    cfg: &Config,
) -> Result<Probe> {
    let no_match = |q_prefix, q_full, on_demand| Probe {
        outcome: LookupOutcome::NoMatch,
        q_prefix,
        q_full,
        on_demand,
    };

    // 2. candidates across every volume (singletons included), minus the query itself
    // and other links to the same inode.
    let candidates: Vec<Candidate> = size_candidates(conn, meta.size)?
        .into_iter()
        .filter(|c| !is_self_or_link(c, key, meta))
        .collect();
    if candidates.is_empty() {
        return Ok(no_match(None, None, Vec::new()));
    }

    // 3. query prefix (equals full for small files).
    let total = cfg.prefix_total();
    let (q_prefix, _) = hash_prefix(reader, path, size, cfg)?;
    let small = size <= total;

    // 4-5. prefix filter; unconfirmable candidates stay as Unmounted.
    let mut survivors: Vec<(&Candidate, RawPath)> = Vec::new();
    let mut unmounted: Vec<(&Candidate, RawPath)> = Vec::new();
    let mut on_demand: Vec<OnDemand> = Vec::new();

    for c in &candidates {
        let Some(csize) = c.size_bytes() else {
            continue;
        };
        let cpath = queries::reconstruct_file_path(conn, c.folder_id, &c.name)?;
        let native = codec.raw_to_path(&cpath);
        let cprefix = match c.prefix_hash {
            Some(h) => Some(h),
            None => match hash_prefix(reader, &native, csize, cfg) {
                Ok((h, _)) => {
                    on_demand.push(OnDemand {
                        id: c.id,
                        size: c.size,
                        mtime_ns: c.mtime_ns,
                        prefix: Some(h),
                        full: None,
                    });
                    Some(h)
                }
                Err(_) => None,
            },
        };
        match cprefix {
            Some(h) if h == q_prefix => survivors.push((c, cpath)),
            Some(_) => {}
            None => unmounted.push((c, cpath)),
        }
    }

    if survivors.is_empty() && unmounted.is_empty() {
        return Ok(no_match(Some(q_prefix), None, on_demand));
    }

    // 6-7. full-hash filter + freshness.
    let q_full = if small {
        q_prefix
    } else {
        hash_full(reader, path, size)?
    };

    let mut matches: Vec<MatchRow> = Vec::new();
    for (c, cpath) in &survivors {
        // Survivors passed the same check above, so this cannot skip a row that was kept.
        let Some(csize) = c.size_bytes() else {
            continue;
        };
        let native = codec.raw_to_path(cpath);
        let fresh = freshness(reader, &native, c);
        let cfull = match c.full_hash {
            Some(h) => Some(h),
            None => match hash_full(reader, &native, csize) {
                Ok(h) => {
                    // A hash of content that changed since the scan must not be stored
                    // against the old metadata.
                    if fresh == Freshness::Fresh {
                        on_demand.push(OnDemand {
                            id: c.id,
                            size: c.size,
                            mtime_ns: c.mtime_ns,
                            prefix: None,
                            full: Some(h),
                        });
                    }
                    Some(h)
                }
                Err(_) => None,
            },
        };
        match cfull {
            Some(h) if h == q_full => matches.push(MatchRow {
                volume_id: c.volume_id,
                path: cpath.clone(),
                freshness: fresh,
            }),
            Some(_) => {}
            None => matches.push(MatchRow {
                volume_id: c.volume_id,
                path: cpath.clone(),
                freshness: Freshness::Unmounted,
            }),
        }
    }
    for (c, cpath) in &unmounted {
        matches.push(MatchRow {
            volume_id: c.volume_id,
            path: cpath.clone(),
            freshness: Freshness::Unmounted,
        });
    }

    let outcome = if matches.is_empty() {
        LookupOutcome::NoMatch
    } else {
        LookupOutcome::Matches(matches)
    };
    Ok(Probe {
        outcome,
        q_prefix: Some(q_prefix),
        q_full: Some(q_full),
        on_demand: merge_on_demand(on_demand),
    })
}

/// One entry per row: a prefix hash and a later full hash for the same candidate merge.
fn merge_on_demand(mut v: Vec<OnDemand>) -> Vec<OnDemand> {
    v.sort_by_key(|d| d.id);
    let mut out: Vec<OnDemand> = Vec::with_capacity(v.len());
    for d in v {
        match out.last_mut() {
            Some(last) if last.id == d.id => {
                last.prefix = last.prefix.or(d.prefix);
                last.full = last.full.or(d.full);
            }
            _ => out.push(d),
        }
    }
    out
}

/// True for the query's own row or another link to the same inode.
fn is_self_or_link(c: &Candidate, key: Option<&QueryKey>, meta: &FileMeta) -> bool {
    let Some(key) = key else { return false };
    if key.existing.as_ref().is_some_and(|e| e.id == c.id) {
        return true;
    }
    c.volume_id == key.volume_id
        && meta.file_id.is_some()
        && c.file_id.as_deref() == meta.file_id.as_deref()
}

fn freshness(reader: &dyn ContentReader, native: &Path, c: &Candidate) -> Freshness {
    match reader.metadata(native) {
        Err(_) => Freshness::Missing,
        Ok(md) if md.size == c.size && md.mtime_ns == c.mtime_ns => Freshness::Fresh,
        Ok(_) => Freshness::Stale,
    }
}

fn size_candidates(conn: &Connection, size: i64) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT id,volume_id,folder_id,name,size,mtime_ns,prefix_hash,full_hash,file_id \
         FROM files WHERE size=?1 AND state NOT IN (-1,-2)",
    )?;
    let rows = stmt
        .query_map([size], |r| {
            let p: Option<Vec<u8>> = r.get(6)?;
            let f: Option<Vec<u8>> = r.get(7)?;
            Ok(Candidate {
                id: r.get(0)?,
                volume_id: r.get(1)?,
                folder_id: r.get(2)?,
                name: RawPath::new(r.get::<_, Vec<u8>>(3)?),
                file_id: r.get(8)?,
                size: r.get(4)?,
                mtime_ns: r.get(5)?,
                prefix_hash: p.and_then(|b| Hash::from_slice(&b)),
                full_hash: f.and_then(|b| Hash::from_slice(&b)),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `Ok(None)` when the query's volume is not indexed. The path is canonicalized before
/// the volume is identified, so every spelling of a root resolves to the same volume.
fn resolve_query_key(
    conn: &Connection,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
) -> Result<Option<QueryKey>> {
    let native = std::fs::canonicalize(path)?;
    let info = vi.identify(&native)?;
    let Some(volume_id) = find_volume_id(conn, &info.uuid)? else {
        return Ok(None);
    };
    let mount = queries::get_volume_mount(conn, volume_id)?.unwrap_or_else(RawPath::empty);
    let (parent, name) = codec.split_component(&native);
    let folder_id = match strip_mount(parent.as_bytes(), mount.as_bytes()) {
        Some(rel) => find_folder_by_rel(conn, volume_id, mount.as_bytes(), rel)?,
        None => {
            tracing::debug!(path = %native.display(), "query path is not under the stored mount");
            None
        }
    };
    let existing = match folder_id {
        Some(fid) => queries::find_by_path(conn, volume_id, fid, &name)?,
        None => None,
    };
    Ok(Some(QueryKey {
        volume_id,
        folder_id,
        name,
        existing,
    }))
}

/// Walks the folder rows below the volume root along the mount-stripped bytes `rel`.
fn find_folder_by_rel(
    conn: &Connection,
    volume_id: i64,
    mount: &[u8],
    rel: &[u8],
) -> Result<Option<i64>> {
    let Some(mut cur) = queries::find_folder(conn, volume_id, 0, &RawPath::empty())? else {
        return Ok(None);
    };
    // Windows-style mounts accept both separators; Unix names may legally hold `\`.
    let sep = separator_for(mount);
    let is_sep = |b: &u8| *b == sep || (sep == b'\\' && *b == b'/');
    for comp in rel.split(is_sep).filter(|c| !c.is_empty()) {
        match queries::find_folder(conn, volume_id, cur, &RawPath::new(comp.to_vec()))? {
            Some(id) => cur = id,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

fn find_volume_id(conn: &Connection, uuid: &str) -> Result<Option<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM volumes WHERE uuid=?1")?;
    let mut rows = stmt.query_map([uuid], |r| r.get::<_, i64>(0))?;
    let id = rows.next().transpose()?;
    Ok(id)
}

fn latest_run(conn: &Connection, volume_id: i64) -> Result<i64> {
    Ok(queries::latest_finished_run(conn, volume_id)?.unwrap_or(0))
}

/// State a scan would have given the query row.
fn query_state(q_prefix: Option<Hash>, q_full: Option<Hash>) -> State {
    match (q_prefix, q_full) {
        (_, Some(_)) => State::Done,
        (Some(_), None) => State::UniquePrefix,
        (None, None) => State::UniqueSize,
    }
}

fn is_fresh(e: &FileEntry, m: &FileMeta) -> bool {
    e.size == m.size
        && e.mtime_ns == m.mtime_ns
        && e.ctime_ns == m.ctime_ns
        && e.file_id == m.file_id
}

/// Writes everything `--insert` needs in one IMMEDIATE transaction, so a failure part
/// way leaves nothing. The caller holds the database lock, so no scan can hand out ids
/// concurrently. Rows are touched by id, never by a blind upsert (CORE-02).
fn apply_insert(
    conn: &Connection,
    key: Option<&QueryKey>,
    meta: &FileMeta,
    probe: &Probe,
    prefix_total: i64,
) -> Result<InsertStatus> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let mut touched: Vec<(i64, Hash)> = Vec::new();

    // Candidate hashes computed on demand. The guard keeps a row that changed since the
    // scan, or that is no longer a singleton, from taking a stale hash.
    for d in &probe.on_demand {
        let n = match d.full {
            Some(_) => tx.execute(
                "UPDATE files SET prefix_hash=COALESCE(?1,prefix_hash), full_hash=?2, \
                 prefix_bytes=CASE WHEN size<=?6 THEN size ELSE ?6 END, state=5 \
                 WHERE id=?3 AND state IN (-3,-4) AND size=?4 AND mtime_ns=?5",
                params![
                    d.prefix.as_ref().map(|h| h.as_bytes().as_slice()),
                    d.full.as_ref().map(|h| h.as_bytes().as_slice()),
                    d.id,
                    d.size,
                    d.mtime_ns,
                    prefix_total
                ],
            )?,
            // Prefix only: keep the state. Promoting -3 to -4 would claim a per-volume
            // fact that lookup's global candidate set does not establish.
            None => tx.execute(
                "UPDATE files SET prefix_hash=?1, \
                 prefix_bytes=CASE WHEN size<=?6 THEN size ELSE ?6 END \
                 WHERE id=?3 AND state=-3 AND prefix_hash IS NULL AND size=?4 AND mtime_ns=?5",
                params![
                    d.prefix.as_ref().map(|h| h.as_bytes().as_slice()),
                    Option::<&[u8]>::None,
                    d.id,
                    d.size,
                    d.mtime_ns,
                    prefix_total
                ],
            )?,
        };
        if let (1, Some(f)) = (n, d.full) {
            let vol: i64 =
                tx.query_row("SELECT volume_id FROM files WHERE id=?1", [d.id], |r| {
                    r.get(0)
                })?;
            touched.push((vol, f));
        }
    }

    let status = match key {
        None => InsertStatus::SkippedVolumeNotIndexed,
        Some(QueryKey {
            folder_id: None, ..
        }) => InsertStatus::SkippedFolderNotIndexed,
        Some(
            k @ QueryKey {
                folder_id: Some(fid),
                ..
            },
        ) => {
            let state = query_state(probe.q_prefix, probe.q_full);
            let run = latest_run(&tx, k.volume_id)?;
            let status = match &k.existing {
                Some(e) if is_fresh(e, meta) && e.state.is_hashed() => {
                    InsertStatus::Unchanged { id: e.id }
                }
                Some(e) => {
                    update_query_row(&tx, e.id, meta, probe, state, run, prefix_total)?;
                    InsertStatus::Updated { id: e.id }
                }
                None => {
                    let id = insert_query_row(&tx, k, *fid, meta, probe, state, run, prefix_total)?;
                    InsertStatus::Inserted { id }
                }
            };
            if let (State::Done, Some(f)) = (state, probe.q_full) {
                if !matches!(status, InsertStatus::Unchanged { .. }) {
                    touched.push((k.volume_id, f));
                }
            }
            status
        }
    };

    touched.sort_unstable_by_key(|(v, h)| (*v, *h.as_bytes()));
    touched.dedup();
    let now = crate::model::now_unix_ns();
    for (vol, h) in &touched {
        let run = latest_run(&tx, *vol)?;
        queries::recompute_group_for_hash(&tx, *vol, run, h, now)?;
    }
    tx.commit()?;
    Ok(status)
}

/// Hashes stored on the query row: only what the chosen state implies.
fn query_hashes(probe: &Probe, state: State) -> (Option<Hash>, Option<Hash>) {
    match state {
        State::Done => (probe.q_prefix, probe.q_full),
        State::UniquePrefix => (probe.q_prefix, None),
        _ => (None, None),
    }
}

fn prefix_bytes_for(meta: &FileMeta, prefix: Option<Hash>, prefix_total: i64) -> Option<i64> {
    prefix.map(|_| meta.size.min(prefix_total))
}

#[allow(clippy::too_many_arguments)]
fn insert_query_row(
    tx: &rusqlite::Transaction<'_>,
    key: &QueryKey,
    folder_id: i64,
    meta: &FileMeta,
    probe: &Probe,
    state: State,
    run: i64,
    prefix_total: i64,
) -> Result<i64> {
    let (prefix, full) = query_hashes(probe, state);
    // No `id` and no ON CONFLICT: under the lock with `existing == None`, SQLite assigns
    // the next rowid and a conflict is a real error.
    let id = tx.query_row(
        "INSERT INTO files(volume_id,folder_id,name,file_id,nlink,size,mtime_ns,ctime_ns,attrs,\
         prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,last_seen_run_id) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,NULL,?14,?15) RETURNING id",
        params![
            key.volume_id,
            folder_id,
            key.name.as_bytes(),
            meta.file_id,
            meta.nlink,
            meta.size,
            meta.mtime_ns,
            meta.ctime_ns,
            meta.attrs,
            prefix.as_ref().map(|h| h.as_bytes().as_slice()),
            full.as_ref().map(|h| h.as_bytes().as_slice()),
            prefix_bytes_for(meta, prefix, prefix_total),
            state.as_i64(),
            crate::model::now_unix_ns(),
            run,
        ],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(id)
}

fn update_query_row(
    tx: &rusqlite::Transaction<'_>,
    id: i64,
    meta: &FileMeta,
    probe: &Probe,
    state: State,
    run: i64,
    prefix_total: i64,
) -> Result<()> {
    let (prefix, full) = query_hashes(probe, state);
    tx.execute(
        "UPDATE files SET size=?1, mtime_ns=?2, ctime_ns=?3, file_id=?4, nlink=?5, attrs=?6, \
         prefix_hash=?7, full_hash=?8, prefix_bytes=?9, state=?10, err=NULL, scanned_at=?11, \
         last_seen_run_id=MAX(last_seen_run_id, ?12) WHERE id=?13",
        params![
            meta.size,
            meta.mtime_ns,
            meta.ctime_ns,
            meta.file_id,
            meta.nlink,
            meta.attrs,
            prefix.as_ref().map(|h| h.as_bytes().as_slice()),
            full.as_ref().map(|h| h.as_bytes().as_slice()),
            prefix_bytes_for(meta, prefix, prefix_total),
            state.as_i64(),
            crate::model::now_unix_ns(),
            run,
            id,
        ],
    )?;
    Ok(())
}
