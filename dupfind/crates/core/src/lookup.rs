//! `lookup`: check whether a new file duplicates anything already indexed (4.8).
//! Read-only by default; `--insert` persists on-demand hashes and the query row.

use std::path::Path;

use rusqlite::{params, Connection};

use crate::config::Config;
use crate::db::ids::IdAllocator;
use crate::db::queries;
use crate::error::Result;
use crate::hash::{hash_full, hash_prefix};
use crate::model::{attrs, Hash, SkipReason, State};
use crate::path::RawPath;
use crate::traits::{ContentReader, EntryKind, PathCodec, VolumeIdentity};

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

struct Candidate {
    id: i64,
    volume_id: i64,
    folder_id: i64,
    name: RawPath,
    size: i64,
    mtime_ns: i64,
    state: State,
    prefix_hash: Option<Hash>,
    full_hash: Option<Hash>,
}

pub fn lookup(
    conn: &Connection,
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
    cfg: &Config,
    insert: bool,
) -> Result<LookupOutcome> {
    // 1. stat the query file.
    let meta = reader.metadata(path)?;
    match meta.kind {
        EntryKind::RegularFile => {}
        EntryKind::Symlink => return Ok(LookupOutcome::Skipped(SkipReason::Symlink)),
        _ => return Ok(LookupOutcome::Skipped(SkipReason::NonRegular)),
    }
    if meta.size == 0 {
        return Ok(LookupOutcome::Skipped(SkipReason::ZeroLength));
    }
    if attrs::has(meta.attrs, attrs::PLACEHOLDER) && !cfg.include_placeholders {
        return Ok(LookupOutcome::Skipped(SkipReason::Placeholder));
    }

    // 2. candidates across every volume (singletons included).
    let candidates = size_candidates(conn, meta.size)?;
    if candidates.is_empty() {
        return Ok(LookupOutcome::NoMatch);
    }

    // 3. query prefix (equals full for small files).
    let total = cfg.prefix_total();
    let (q_prefix, _) = hash_prefix(reader, path, meta.size as u64, cfg)?;
    let small = (meta.size as u64) <= total;

    // 4-5. prefix filter; unconfirmable candidates stay as Unmounted.
    let mut survivors: Vec<(Candidate, RawPath)> = Vec::new();
    let mut unmounted: Vec<(Candidate, RawPath)> = Vec::new();
    let mut on_demand: Vec<(i64, Option<Hash>, Option<Hash>)> = Vec::new();

    for c in &candidates {
        let cpath = queries::reconstruct_file_path(conn, c.folder_id, &c.name)?;
        let native = codec.raw_to_path(&cpath);
        let cprefix = match c.prefix_hash {
            Some(h) => Some(h),
            None => match hash_prefix(reader, &native, c.size as u64, cfg) {
                Ok((h, _)) => {
                    on_demand.push((c.id, Some(h), None));
                    Some(h)
                }
                Err(_) => None,
            },
        };
        match cprefix {
            Some(h) if h == q_prefix => survivors.push((clone_candidate(c), cpath)),
            Some(_) => {}
            None => unmounted.push((clone_candidate(c), cpath)),
        }
    }

    if survivors.is_empty() && unmounted.is_empty() {
        return Ok(LookupOutcome::NoMatch);
    }

    // 6-7. full-hash filter + freshness.
    let q_full = if small {
        q_prefix
    } else {
        hash_full(reader, path, meta.size as u64)?
    };

    let mut matches: Vec<MatchRow> = Vec::new();
    for (c, cpath) in &survivors {
        let native = codec.raw_to_path(cpath);
        let cfull = match c.full_hash {
            Some(h) => Some(h),
            None => match hash_full(reader, &native, c.size as u64) {
                Ok(h) => {
                    on_demand.push((c.id, None, Some(h)));
                    Some(h)
                }
                Err(_) => None,
            },
        };
        match cfull {
            Some(h) if h == q_full => matches.push(MatchRow {
                volume_id: c.volume_id,
                path: cpath.clone(),
                freshness: freshness(reader, &native, c),
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

    if matches.is_empty() {
        return Ok(LookupOutcome::NoMatch);
    }

    if insert {
        persist(conn, codec, vi, path, &meta, &on_demand)?;
    }

    Ok(LookupOutcome::Matches(matches))
}

fn clone_candidate(c: &Candidate) -> Candidate {
    Candidate {
        id: c.id,
        volume_id: c.volume_id,
        folder_id: c.folder_id,
        name: c.name.clone(),
        size: c.size,
        mtime_ns: c.mtime_ns,
        state: c.state,
        prefix_hash: c.prefix_hash,
        full_hash: c.full_hash,
    }
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
        "SELECT id,volume_id,folder_id,name,size,mtime_ns,state,prefix_hash,full_hash \
         FROM files WHERE size=?1 AND state NOT IN (-1,-2)",
    )?;
    let rows = stmt
        .query_map([size], |r| {
            let p: Option<Vec<u8>> = r.get(7)?;
            let f: Option<Vec<u8>> = r.get(8)?;
            Ok(Candidate {
                id: r.get(0)?,
                volume_id: r.get(1)?,
                folder_id: r.get(2)?,
                name: RawPath::new(r.get::<_, Vec<u8>>(3)?),
                size: r.get(4)?,
                mtime_ns: r.get(5)?,
                state: State::from_i64(r.get(6)?).unwrap_or(State::Error),
                prefix_hash: p.and_then(|b| Hash::from_slice(&b)),
                full_hash: f.and_then(|b| Hash::from_slice(&b)),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Persist on-demand hashes; optionally insert the query file as a new row.
fn persist(
    conn: &Connection,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
    meta: &crate::traits::FileMeta,
    on_demand: &[(i64, Option<Hash>, Option<Hash>)],
) -> Result<()> {
    for (id, prefix, full) in on_demand {
        match (prefix, full) {
            (Some(p), Some(f)) => conn.execute(
                "UPDATE files SET prefix_hash=?1, full_hash=?2, state=5 WHERE id=?3",
                params![p.as_bytes(), f.as_bytes(), id],
            )?,
            (Some(p), None) => conn.execute(
                "UPDATE files SET prefix_hash=?1 WHERE id=?2",
                params![p.as_bytes(), id],
            )?,
            (None, Some(f)) => conn.execute(
                "UPDATE files SET full_hash=?1, state=5 WHERE id=?2",
                params![f.as_bytes(), id],
            )?,
            (None, None) => 0,
        };
    }

    // Insert the query row only if its volume is already indexed.
    let info = vi.identify(path)?;
    let Some(volume_id) = find_volume_id(conn, &info.uuid)? else {
        return Ok(());
    };
    let mount = queries::get_volume_mount(conn, volume_id)?.unwrap_or_else(RawPath::empty);
    let native = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let (parent, name) = codec.split_component(&native);
    let Some(folder_id) = find_folder_by_path(conn, volume_id, &mount, &parent)? else {
        return Ok(());
    };
    let id = IdAllocator::seed(conn)?.file();
    let mut entry = crate::model::FileEntry::enumerated(
        id,
        volume_id,
        folder_id,
        name,
        latest_run(conn, volume_id)?,
    );
    entry.file_id = meta.file_id.clone();
    entry.nlink = meta.nlink;
    entry.size = meta.size;
    entry.mtime_ns = meta.mtime_ns;
    entry.ctime_ns = meta.ctime_ns;
    entry.attrs = meta.attrs;
    entry.scanned_at = Some(crate::model::now_unix_ns());
    queries::upsert_file(conn, &entry)?;
    Ok(())
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

/// Resolve a directory path to an existing folder id, walking from the mount root.
fn find_folder_by_path(
    conn: &Connection,
    volume_id: i64,
    mount: &RawPath,
    dir: &RawPath,
) -> Result<Option<i64>> {
    let comps = crate::pipeline::pass_enumerate::relative_components(dir, mount);
    let Some(root) = queries::find_folder(conn, volume_id, 0, &RawPath::empty())? else {
        return Ok(None);
    };
    let mut cur = root;
    for comp in comps {
        match queries::find_folder(conn, volume_id, cur, &RawPath::new(comp))? {
            Some(id) => cur = id,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}
