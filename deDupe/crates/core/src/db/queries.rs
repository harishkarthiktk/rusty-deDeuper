//! Typed query helpers. All take a `&Connection` so they work on the writer thread,
//! the pipeline's read connection, or a test fixture.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::Result;
use crate::model::{now_unix_ns, ErrRow, FileEntry, FolderRow, Hash, Phase, State};
use crate::path::RawPath;
use crate::traits::VolumeInfo;

const FILE_COLS: &str = "id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,ctime_ns,\
                         attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,\
                         last_seen_run_id";

fn row_to_file(row: &Row<'_>) -> rusqlite::Result<FileEntry> {
    let state_i: i64 = row.get(13)?;
    let prefix: Option<Vec<u8>> = row.get(10)?;
    let full: Option<Vec<u8>> = row.get(11)?;
    Ok(FileEntry {
        id: row.get(0)?,
        volume_id: row.get(1)?,
        folder_id: row.get(2)?,
        name: RawPath::new(row.get::<_, Vec<u8>>(3)?),
        file_id: row.get(4)?,
        nlink: row.get(5)?,
        size: row.get(6)?,
        mtime_ns: row.get(7)?,
        ctime_ns: row.get(8)?,
        attrs: row.get(9)?,
        prefix_hash: prefix.and_then(|b| Hash::from_slice(&b)),
        full_hash: full.and_then(|b| Hash::from_slice(&b)),
        prefix_bytes: row.get(12)?,
        state: State::from_i64(state_i).unwrap_or(State::Error),
        err: row.get(14)?,
        scanned_at: row.get(15)?,
        last_seen_run_id: row.get(16)?,
    })
}

// ---------------------------------------------------------------------------
// meta
// ---------------------------------------------------------------------------

pub fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key=?1", [key], |r| r.get(0))
        .optional()?)
}

pub fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key,value) VALUES(?1,?2) \
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// volumes
// ---------------------------------------------------------------------------

pub fn upsert_volume(conn: &Connection, info: &VolumeInfo, now: i64) -> Result<i64> {
    let existing: Option<i64> = conn
        .query_row("SELECT id FROM volumes WHERE uuid=?1", [&info.uuid], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(id) = existing {
        conn.execute(
            "UPDATE volumes SET mount=?1, fs_type=?2, label=?3, last_scan=?4 WHERE id=?5",
            params![
                info.mount.as_bytes(),
                info.fs_type.as_str(),
                info.label,
                now,
                id
            ],
        )?;
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO volumes(uuid,uuid_kind,label,mount,fs_type,profile,first_seen,last_scan) \
         VALUES(?1,?2,?3,?4,?5,'unknown',?6,?6)",
        params![
            info.uuid,
            info.uuid_kind.as_str(),
            info.label,
            info.mount.as_bytes(),
            info.fs_type.as_str(),
            now
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn get_volume_uuid(conn: &Connection, volume_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT uuid FROM volumes WHERE id=?1", [volume_id], |r| {
            r.get(0)
        })
        .optional()?)
}

pub fn get_volume_mount(conn: &Connection, volume_id: i64) -> Result<Option<RawPath>> {
    Ok(conn
        .query_row("SELECT mount FROM volumes WHERE id=?1", [volume_id], |r| {
            r.get::<_, Vec<u8>>(0)
        })
        .optional()?
        .map(RawPath::new))
}

pub fn update_volume_mount(conn: &Connection, volume_id: i64, mount: &RawPath) -> Result<()> {
    conn.execute(
        "UPDATE volumes SET mount=?1 WHERE id=?2",
        params![mount.as_bytes(), volume_id],
    )?;
    Ok(())
}

/// `(id, uuid, uuid_kind, mount, fs_type, profile, last_scan)`
pub struct VolumeRow {
    pub id: i64,
    pub uuid: String,
    pub uuid_kind: String,
    pub mount: RawPath,
    pub fs_type: String,
    pub profile: String,
    pub last_scan: Option<i64>,
}

pub fn list_volumes(conn: &Connection) -> Result<Vec<VolumeRow>> {
    let mut stmt = conn.prepare(
        "SELECT id,uuid,uuid_kind,mount,fs_type,profile,last_scan FROM volumes ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(VolumeRow {
                id: r.get(0)?,
                uuid: r.get(1)?,
                uuid_kind: r.get(2)?,
                mount: RawPath::new(r.get::<_, Vec<u8>>(3)?),
                fs_type: r.get(4)?,
                profile: r.get(5)?,
                last_scan: r.get(6)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------------------
// folders
// ---------------------------------------------------------------------------

pub fn find_folder(
    conn: &Connection,
    volume_id: i64,
    parent_id: i64,
    name: &RawPath,
) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM folders WHERE volume_id=?1 AND parent_id=?2 AND name=?3",
            params![volume_id, parent_id, name.as_bytes()],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn insert_folder(conn: &Connection, f: &FolderRow) -> Result<()> {
    conn.execute(
        "INSERT INTO folders(id,volume_id,parent_id,name,last_seen_run_id) VALUES(?1,?2,?3,?4,?5) \
         ON CONFLICT(volume_id,parent_id,name) DO UPDATE SET last_seen_run_id=excluded.last_seen_run_id",
        params![f.id, f.volume_id, f.parent_id, f.name.as_bytes(), f.last_seen_run_id],
    )?;
    Ok(())
}

pub fn folder_path(conn: &Connection, folder_id: i64) -> Result<Option<RawPath>> {
    let exists: Option<i64> = conn
        .query_row("SELECT id FROM folders WHERE id=?1", [folder_id], |r| {
            r.get(0)
        })
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    Ok(Some(reconstruct_path(conn, folder_id)?))
}

// ---------------------------------------------------------------------------
// files
// ---------------------------------------------------------------------------

pub fn upsert_file(conn: &Connection, f: &FileEntry) -> Result<()> {
    conn.execute(
        "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,ctime_ns,\
         attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,last_seen_run_id) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) \
         ON CONFLICT(volume_id,folder_id,name) DO UPDATE SET \
         size=excluded.size, mtime_ns=excluded.mtime_ns, ctime_ns=excluded.ctime_ns, \
         file_id=excluded.file_id, nlink=excluded.nlink, attrs=excluded.attrs, \
         prefix_hash=excluded.prefix_hash, full_hash=excluded.full_hash, \
         prefix_bytes=excluded.prefix_bytes, state=excluded.state, err=excluded.err, \
         scanned_at=excluded.scanned_at, last_seen_run_id=excluded.last_seen_run_id",
        params![
            f.id,
            f.volume_id,
            f.folder_id,
            f.name.as_bytes(),
            f.file_id,
            f.nlink,
            f.size,
            f.mtime_ns,
            f.ctime_ns,
            f.attrs,
            f.prefix_hash.as_ref().map(|h| h.as_bytes().as_slice()),
            f.full_hash.as_ref().map(|h| h.as_bytes().as_slice()),
            f.prefix_bytes,
            f.state.as_i64(),
            f.err,
            f.scanned_at,
            f.last_seen_run_id,
        ],
    )?;
    Ok(())
}

pub fn find_by_path(
    conn: &Connection,
    volume_id: i64,
    folder_id: i64,
    name: &RawPath,
) -> Result<Option<FileEntry>> {
    let sql =
        format!("SELECT {FILE_COLS} FROM files WHERE volume_id=?1 AND folder_id=?2 AND name=?3");
    Ok(conn
        .query_row(
            &sql,
            params![volume_id, folder_id, name.as_bytes()],
            row_to_file,
        )
        .optional()?)
}

/// A row with this `(volume_id, file_id)` that has **not** been seen this run.
/// The caller must also confirm `size` and `mtime_ns` (guards inode reuse).
pub fn find_move_candidate(
    conn: &Connection,
    volume_id: i64,
    file_id: &[u8],
    run_id: i64,
) -> Result<Vec<FileEntry>> {
    let sql = format!(
        "SELECT {FILE_COLS} FROM files \
         WHERE volume_id=?1 AND file_id=?2 AND last_seen_run_id<>?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![volume_id, file_id, run_id], row_to_file)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn set_state(conn: &Connection, id: i64, state: State) -> Result<()> {
    conn.execute(
        "UPDATE files SET state=?1 WHERE id=?2",
        params![state.as_i64(), id],
    )?;
    Ok(())
}

pub fn rows_in_state(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
    state: State,
) -> Result<Vec<FileEntry>> {
    let sql = format!(
        "SELECT {FILE_COLS} FROM files \
         WHERE volume_id=?1 AND state=?3 AND last_seen_run_id=?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![volume_id, run_id, state.as_i64()], row_to_file)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn count_rows(conn: &Connection, volume_id: i64, run_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM files WHERE volume_id=?1 AND last_seen_run_id=?2",
        params![volume_id, run_id],
        |r| r.get(0),
    )?)
}

/// The 4.6 grouping query: sizes whose state-0 rows cover more than one inode.
pub fn size_candidates(conn: &Connection, volume_id: i64, run_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT size FROM files \
         WHERE volume_id=?1 AND state=0 AND last_seen_run_id=?2 \
         GROUP BY size \
         HAVING COUNT(DISTINCT COALESCE(file_id, id)) > 1",
    )?;
    let rows = stmt
        .query_map(params![volume_id, run_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// All rows in a given state for a run, unordered.
pub fn all_rows_in_state(
    conn: &Connection,
    volume_id: i64,
    state: State,
) -> Result<Vec<FileEntry>> {
    let sql = format!("SELECT {FILE_COLS} FROM files WHERE volume_id=?1 AND state=?2");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![volume_id, state.as_i64()], row_to_file)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// `(id, prefix_hash)` for rows that already hold a prefix hash this run.
pub fn rows_with_prefix(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
) -> Result<Vec<(i64, Hash)>> {
    let mut stmt = conn.prepare(
        "SELECT id, prefix_hash FROM files \
         WHERE volume_id=?1 AND last_seen_run_id=?2 AND state IN (2,-4) AND prefix_hash IS NOT NULL",
    )?;
    let rows = stmt
        .query_map(params![volume_id, run_id], |r| {
            let id: i64 = r.get(0)?;
            let h: Vec<u8> = r.get(1)?;
            Ok(Hash::from_slice(&h).map(|h| (id, h)))
        })?
        .filter_map(std::result::Result::transpose)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn prefix_partners(conn: &Connection, volume_id: i64, hash: &Hash) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM files WHERE volume_id=?1 AND prefix_hash=?2")?;
    let rows = stmt
        .query_map(params![volume_id, hash.as_bytes()], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn full_partners(conn: &Connection, hash: &Hash) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM files WHERE full_hash=?1")?;
    let rows = stmt
        .query_map(params![hash.as_bytes()], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Prefix hashes held by exactly one row this run (candidates for `-4`).
pub fn singleton_prefixes(conn: &Connection, volume_id: i64, run_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM files \
         WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2 AND prefix_hash IS NOT NULL \
         GROUP BY prefix_hash HAVING COUNT(*)=1",
    )?;
    let rows = stmt
        .query_map(params![volume_id, run_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Run-scoped partners for a prefix hash (state 2 / -4, this run only).
pub fn prefix_partners_run(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
    hash: &Hash,
) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM files WHERE volume_id=?1 AND last_seen_run_id=?2 \
         AND prefix_hash=?3 AND state IN (2,-4)",
    )?;
    let rows = stmt
        .query_map(params![volume_id, run_id, hash.as_bytes()], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Grouped prefix cursor: `(prefix_hash, [ids])` ordered by prefix_hash.
pub fn prefix_groups(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
) -> Result<Vec<(Hash, Vec<i64>)>> {
    let mut stmt = conn.prepare(
        "SELECT prefix_hash, id FROM files \
         WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2 AND prefix_hash IS NOT NULL \
         ORDER BY prefix_hash",
    )?;
    let rows = stmt.query_map(params![volume_id, run_id], |r| {
        let h: Vec<u8> = r.get(0)?;
        Ok((
            Hash::from_slice(&h).unwrap_or(Hash([0; 16])),
            r.get::<_, i64>(1)?,
        ))
    })?;
    let mut groups: Vec<(Hash, Vec<i64>)> = Vec::new();
    for row in rows {
        let (h, id) = row?;
        match groups.last_mut() {
            Some((last, ids)) if *last == h => ids.push(id),
            _ => groups.push((h, vec![id])),
        }
    }
    Ok(groups)
}

/// Delete rows not seen this run whose folder descends from one of `root_folder_ids`.
pub fn delete_stale_under_roots(
    conn: &Connection,
    run_id: i64,
    volume_id: i64,
    root_folder_ids: &[i64],
) -> Result<u64> {
    if root_folder_ids.is_empty() {
        return Ok(0);
    }
    let list = root_folder_ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "WITH RECURSIVE sub(id) AS ( \
           SELECT id FROM folders WHERE id IN ({list}) \
           UNION ALL \
           SELECT f.id FROM folders f JOIN sub s ON f.parent_id = s.id \
         ) \
         DELETE FROM files WHERE volume_id=?1 AND last_seen_run_id < ?2 \
         AND folder_id IN (SELECT id FROM sub)"
    );
    let n = conn.execute(&sql, params![volume_id, run_id])?;
    Ok(n as u64)
}

/// Full path for a file: reconstruct the folder chain, then append `name`.
pub fn reconstruct_file_path(conn: &Connection, folder_id: i64, name: &RawPath) -> Result<RawPath> {
    let dir = reconstruct_path(conn, folder_id)?;
    Ok(dir.join_component(name.as_bytes()))
}

/// Reconstruct a folder's absolute path: `volumes.mount` + chain of names.
pub fn reconstruct_path(conn: &Connection, folder_id: i64) -> Result<RawPath> {
    let (mount, _volume_id): (Vec<u8>, i64) = conn.query_row(
        "SELECT v.mount, f.volume_id FROM folders f JOIN volumes v ON v.id=f.volume_id WHERE f.id=?1",
        [folder_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;

    let mut stmt = conn.prepare(
        "WITH RECURSIVE chain(id,parent_id,name,depth) AS ( \
           SELECT id,parent_id,name,0 FROM folders WHERE id=?1 \
           UNION ALL \
           SELECT p.id,p.parent_id,p.name,c.depth+1 FROM folders p JOIN chain c ON p.id=c.parent_id \
         ) SELECT name FROM chain ORDER BY depth DESC",
    )?;
    let names = stmt
        .query_map([folder_id], |r| r.get::<_, Vec<u8>>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let sep = separator_for(&mount);
    let mut out = mount;
    for n in names {
        if n.is_empty() {
            continue;
        }
        if !out.is_empty() && !out.ends_with(&[sep]) {
            out.push(sep);
        }
        out.extend_from_slice(&n);
    }
    Ok(RawPath::new(out))
}

fn separator_for(mount: &[u8]) -> u8 {
    if mount.contains(&b'\\') && !mount.contains(&b'/') {
        b'\\'
    } else {
        b'/'
    }
}

// ---------------------------------------------------------------------------
// dup_groups
// ---------------------------------------------------------------------------

/// Idempotent recompute: delete this volume's groups, then insert groups with
/// more than one distinct inode. See 4.7 step 5.
pub fn recompute_groups(conn: &Connection, volume_id: i64, now: i64) -> Result<u64> {
    conn.execute("DELETE FROM dup_groups WHERE volume_id=?1", [volume_id])?;
    let n = conn.execute(
        "INSERT INTO dup_groups(volume_id,full_hash,size,member_count,distinct_inodes,\
         wasted_logical,wasted_physical,sharing_kind,computed_at) \
         SELECT volume_id, full_hash, size, COUNT(*), \
                COUNT(DISTINCT COALESCE(file_id, id)), \
                size*(COUNT(DISTINCT COALESCE(file_id, id))-1), NULL, 0, ?2 \
         FROM files \
         WHERE volume_id=?1 AND state IN (3,4,5) AND full_hash IS NOT NULL \
         GROUP BY full_hash, size \
         HAVING COUNT(DISTINCT COALESCE(file_id, id)) > 1",
        params![volume_id, now],
    )?;
    Ok(n as u64)
}

pub struct GroupRow {
    pub full_hash: Hash,
    pub size: i64,
    pub member_count: i64,
    pub distinct_inodes: i64,
    pub wasted_logical: i64,
}

pub fn list_groups(conn: &Connection, volume_id: i64, min_wasted: i64) -> Result<Vec<GroupRow>> {
    let mut stmt = conn.prepare(
        "SELECT full_hash,size,member_count,distinct_inodes,wasted_logical FROM dup_groups \
         WHERE volume_id=?1 AND wasted_logical>=?2 ORDER BY full_hash",
    )?;
    let rows = stmt
        .query_map(params![volume_id, min_wasted], |r| {
            let h: Vec<u8> = r.get(0)?;
            Ok(GroupRow {
                full_hash: Hash::from_slice(&h).unwrap_or(Hash([0; 16])),
                size: r.get(1)?,
                member_count: r.get(2)?,
                distinct_inodes: r.get(3)?,
                wasted_logical: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Members of a group (paths), restricted to the latest run for the volume.
pub fn group_members(
    conn: &Connection,
    volume_id: i64,
    full_hash: &Hash,
    run_id: i64,
) -> Result<Vec<(i64, i64, RawPath, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.folder_id, f.name, f.size FROM files f \
         WHERE f.volume_id=?1 AND f.full_hash=?2 AND f.state IN (3,4,5) \
         AND f.last_seen_run_id=?3",
    )?;
    let rows = stmt
        .query_map(params![volume_id, full_hash.as_bytes(), run_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                RawPath::new(r.get::<_, Vec<u8>>(2)?),
                r.get::<_, i64>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn group_count(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM dup_groups", [], |r| r.get(0))?)
}

// ---------------------------------------------------------------------------
// scan runs
// ---------------------------------------------------------------------------

pub fn insert_run(conn: &Connection, cli_args: &str, profile: &str, now: i64) -> Result<i64> {
    conn.execute(
        "INSERT INTO scan_runs(started_at,cli_args,profile,status) VALUES(?1,?2,?3,'running')",
        params![now, cli_args, profile],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_run(conn: &Connection, run_id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE scan_runs SET status='finished', finished_at=?1 WHERE id=?2",
        params![now, run_id],
    )?;
    Ok(())
}

pub fn abandon_run(conn: &Connection, run_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE scan_runs SET status='abandoned', finished_at=?1 WHERE id=?2",
        params![now_unix_ns(), run_id],
    )?;
    Ok(())
}

/// The most recent run still marked `running`.
pub fn latest_running_run(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM scan_runs WHERE status='running' ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn running_runs_on_volumes(conn: &Connection, volume_ids: &[i64]) -> Result<Vec<i64>> {
    if volume_ids.is_empty() {
        return Ok(Vec::new());
    }
    let list = volume_ids
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT DISTINCT rv.run_id FROM run_volumes rv JOIN scan_runs r ON r.id=rv.run_id \
         WHERE r.status='running' AND rv.volume_id IN ({list})"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn upsert_run_volume(conn: &Connection, run_id: i64, volume_id: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO run_volumes(run_id,volume_id,phase_reached) VALUES(?1,?2,0) \
         ON CONFLICT(run_id,volume_id) DO NOTHING",
        params![run_id, volume_id],
    )?;
    Ok(())
}

pub fn get_phase(conn: &Connection, run_id: i64, volume_id: i64) -> Result<Phase> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT phase_reached FROM run_volumes WHERE run_id=?1 AND volume_id=?2",
            params![run_id, volume_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(Phase::from_i64).unwrap_or(Phase::None))
}

pub fn volumes_for_run(conn: &Connection, run_id: i64) -> Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT volume_id FROM run_volumes WHERE run_id=?1 ORDER BY volume_id")?;
    let rows = stmt
        .query_map([run_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn insert_run_root(
    conn: &Connection,
    run_id: i64,
    volume_id: i64,
    root: &RawPath,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_roots(run_id,volume_id,root) VALUES(?1,?2,?3) \
         ON CONFLICT(run_id,volume_id,root) DO NOTHING",
        params![run_id, volume_id, root.as_bytes()],
    )?;
    Ok(())
}

pub fn roots_for_run(conn: &Connection, run_id: i64) -> Result<Vec<(i64, RawPath)>> {
    let mut stmt = conn.prepare("SELECT volume_id, root FROM run_roots WHERE run_id=?1")?;
    let rows = stmt
        .query_map([run_id], |r| {
            Ok((r.get::<_, i64>(0)?, RawPath::new(r.get::<_, Vec<u8>>(1)?)))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Latest finished run that touched a volume (for report freshness filtering).
pub fn latest_finished_run(conn: &Connection, volume_id: i64) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT rv.run_id FROM run_volumes rv JOIN scan_runs r ON r.id=rv.run_id \
             WHERE rv.volume_id=?1 AND r.status='finished' \
             ORDER BY r.finished_at DESC, rv.run_id DESC LIMIT 1",
            [volume_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn set_run_volume_phase(
    conn: &Connection,
    run_id: i64,
    volume_id: i64,
    phase: Phase,
) -> Result<()> {
    conn.execute(
        "INSERT INTO run_volumes(run_id,volume_id,phase_reached) VALUES(?1,?2,?3) \
         ON CONFLICT(run_id,volume_id) DO UPDATE SET phase_reached=excluded.phase_reached",
        params![run_id, volume_id, phase.as_i64()],
    )?;
    Ok(())
}

pub fn insert_error(conn: &Connection, e: &ErrRow) -> Result<()> {
    conn.execute(
        "INSERT INTO errors(run_id,file_row_id,path,kind,message) VALUES(?1,?2,?3,?4,?5)",
        params![
            e.run_id,
            e.file_row_id,
            e.path.as_ref().map(|p| p.as_bytes()),
            e.kind.as_i64(),
            e.message
        ],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// db info
// ---------------------------------------------------------------------------

pub fn counts_by_state(conn: &Connection, volume_id: i64) -> Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT state, COUNT(*) FROM files WHERE volume_id=?1 GROUP BY state ORDER BY state",
    )?;
    let rows = stmt
        .query_map([volume_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

pub fn stale_count(conn: &Connection, volume_id: i64, run_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM files WHERE volume_id=?1 AND last_seen_run_id<>?2",
        params![volume_id, run_id],
        |r| r.get(0),
    )?)
}

pub fn total_files(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::model::attrs;
    use crate::traits::VolumeInfo;

    fn fixture() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("q.db");
        let db = crate::db::Db::open(&path, &Config::default()).unwrap();

        let info = VolumeInfo {
            uuid: "vol-1".into(),
            uuid_kind: crate::model::UuidKind::FsUuid,
            mount: RawPath::new(b"/mnt".to_vec()),
            fs_type: crate::model::FsType::Ext4,
            label: None,
        };
        let vid = upsert_volume(&db.conn, &info, 0).unwrap();
        assert_eq!(vid, 1);
        insert_folder(
            &db.conn,
            &FolderRow {
                id: 1,
                volume_id: vid,
                parent_id: 0,
                name: RawPath::empty(),
                last_seen_run_id: Some(1),
            },
        )
        .unwrap();
        insert_folder(
            &db.conn,
            &FolderRow {
                id: 2,
                volume_id: vid,
                parent_id: 1,
                name: RawPath::new(b"sub".to_vec()),
                last_seen_run_id: Some(1),
            },
        )
        .unwrap();
        (dir, db.conn)
    }

    fn insert_file(
        conn: &Connection,
        id: i64,
        folder: i64,
        name: &[u8],
        size: i64,
        ino: Option<Vec<u8>>,
        run: i64,
    ) {
        let mut f = FileEntry::enumerated(id, 1, folder, RawPath::new(name.to_vec()), run);
        f.size = size;
        f.file_id = ino;
        f.mtime_ns = 5;
        upsert_file(conn, &f).unwrap();
    }

    #[test]
    fn find_by_path_and_reconstruct() {
        let (_d, conn) = fixture();
        insert_file(&conn, 10, 2, b"a.txt", 100, Some(vec![1]), 1);
        let got = find_by_path(&conn, 1, 2, &RawPath::new(b"a.txt".to_vec()))
            .unwrap()
            .unwrap();
        assert_eq!(got.id, 10);
        let p = reconstruct_file_path(&conn, 2, &RawPath::new(b"a.txt".to_vec())).unwrap();
        assert_eq!(p, RawPath::new(b"/mnt/sub/a.txt".to_vec()));
        assert_eq!(
            reconstruct_path(&conn, 1).unwrap(),
            RawPath::new(b"/mnt".to_vec())
        );
    }

    #[test]
    fn size_candidates_ignores_hardlink_only_pairs() {
        let (_d, conn) = fixture();
        // Two links to the same inode: not a candidate.
        insert_file(&conn, 10, 2, b"l1", 100, Some(vec![7]), 1);
        insert_file(&conn, 11, 2, b"l2", 100, Some(vec![7]), 1);
        // A distinct inode at the same size makes it a candidate.
        insert_file(&conn, 12, 2, b"c", 100, Some(vec![8]), 1);
        let sizes = size_candidates(&conn, 1, 1).unwrap();
        assert_eq!(sizes, vec![100]);

        conn.execute(
            "UPDATE files SET attrs=?1 WHERE id IN (10,11)",
            [attrs::HARDLINK],
        )
        .unwrap();
    }

    #[test]
    fn move_candidate_excludes_seen_rows() {
        let (_d, conn) = fixture();
        insert_file(&conn, 20, 2, b"m", 50, Some(vec![3]), 1);
        let seen = find_move_candidate(&conn, 1, &[3], 1).unwrap();
        assert!(seen.is_empty());
        let other = find_move_candidate(&conn, 1, &[3], 2).unwrap();
        assert_eq!(other.len(), 1);
    }

    #[test]
    fn stale_deletion_scoped_to_roots() {
        let (_d, conn) = fixture();
        insert_file(&conn, 30, 2, b"old", 10, Some(vec![1]), 1);
        // New run sees the root but not this file.
        delete_stale_under_roots(&conn, 2, 1, &[1]).unwrap();
        assert!(find_by_path(&conn, 1, 2, &RawPath::new(b"old".to_vec()))
            .unwrap()
            .is_none());
    }

    #[test]
    fn recompute_groups_collapses_hardlinks() {
        let (_d, conn) = fixture();
        let h = Hash([9; 16]);
        for (id, name, ino) in [
            (40, b"a".as_slice(), 1i64),
            (41, b"b".as_slice(), 1),
            (42, b"c".as_slice(), 2),
        ] {
            insert_file(&conn, id, 2, name, 10, Some(ino.to_le_bytes().to_vec()), 1);
            conn.execute(
                "UPDATE files SET state=5, full_hash=?1 WHERE id=?2",
                params![h.as_bytes(), id],
            )
            .unwrap();
        }
        recompute_groups(&conn, 1, 0).unwrap();
        let g = list_groups(&conn, 1, 0).unwrap();
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].distinct_inodes, 2);
        assert_eq!(g[0].member_count, 3);
        assert_eq!(g[0].wasted_logical, 10);
    }

    #[test]
    fn upsert_file_updates_existing_row() {
        let (_d, conn) = fixture();
        let mut f = FileEntry::enumerated(10, 1, 2, RawPath::new(b"u".to_vec()), 1);
        f.size = 100;
        upsert_file(&conn, &f).unwrap();
        let mut f2 = f.clone();
        f2.size = 200;
        f2.file_id = Some(vec![9]);
        upsert_file(&conn, &f2).unwrap();
        let got = find_by_path(&conn, 1, 2, &RawPath::new(b"u".to_vec()))
            .unwrap()
            .unwrap();
        assert_eq!(got.size, 200);
        assert_eq!(got.file_id, Some(vec![9]));
    }

    #[test]
    fn set_state_and_rows_in_state() {
        let (_d, conn) = fixture();
        insert_file(&conn, 10, 2, b"s", 100, Some(vec![1]), 1);
        set_state(&conn, 10, State::SizeCandidate).unwrap();
        assert_eq!(
            rows_in_state(&conn, 1, 1, State::SizeCandidate)
                .unwrap()
                .len(),
            1
        );
        assert!(rows_in_state(&conn, 1, 1, State::Done).unwrap().is_empty());
        set_state(&conn, 10, State::UniqueSize).unwrap();
        assert!(rows_in_state(&conn, 1, 1, State::SizeCandidate)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn prefix_and_full_partners() {
        let (_d, conn) = fixture();
        let h = Hash([7; 16]);
        insert_file(&conn, 10, 2, b"p1", 100, Some(vec![1]), 1);
        insert_file(&conn, 11, 2, b"p2", 100, Some(vec![2]), 1);
        conn.execute(
            "UPDATE files SET prefix_hash=?1, full_hash=?1, state=2 WHERE id IN (10,11)",
            params![h.as_bytes()],
        )
        .unwrap();
        let mut p = prefix_partners(&conn, 1, &h).unwrap();
        p.sort();
        assert_eq!(p, vec![10, 11]);
        let mut f = full_partners(&conn, &h).unwrap();
        f.sort();
        assert_eq!(f, vec![10, 11]);
    }
}
