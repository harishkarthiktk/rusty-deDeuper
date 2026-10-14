//! Batched writer thread. One thread owns the `rusqlite::Connection`; hash workers
//! send [`Batch`] messages over a bounded channel. See `.plan.md` 4.5.

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, Transaction};

use crate::error::CoreError;
use crate::model::{ErrRow, FileEntry, FolderRow, Hash, Phase};

pub const BATCH_ROWS: usize = 50_000;
pub const BATCH_MS: u64 = 5_000;

pub enum Batch {
    Folders(Vec<FolderRow>),
    Enumerated(Vec<FileEntry>),
    SizeCandidate(Vec<i64>),
    UniqueSize(Vec<i64>),
    PrefixHashed(Vec<(i64, Hash)>),
    UniquePrefix(Vec<(i64, Hash)>),
    Hashed(Vec<(i64, Hash, Hash)>),
    FullHashed(Vec<(i64, Hash)>),
    Verified(Vec<i64>),
    Error(Vec<ErrRow>),
    PhaseComplete {
        run_id: i64,
        volume_id: i64,
        phase: Phase,
    },
    /// Extension beyond 4.5 (surfaced to the user): delete stale rows scoped to
    /// this run's roots. Deletion must run on the writer thread because it owns
    /// the single SQLite write connection.
    DeleteStale {
        run_id: i64,
        volume_id: i64,
        root_folder_ids: Vec<i64>,
    },
    Checkpoint,
    /// Commit everything pending, then ack. Needed so the pipeline's read
    /// connection observes a committed phase before selecting the next pass.
    /// Extension beyond 4.5 (surfaced to the user).
    Flush(std::sync::mpsc::SyncSender<Result<(), CoreError>>),
    Shutdown,
}

impl Batch {
    pub fn row_count(&self) -> usize {
        match self {
            Batch::Folders(v) => v.len(),
            Batch::Enumerated(v) => v.len(),
            Batch::SizeCandidate(v) | Batch::UniqueSize(v) | Batch::Verified(v) => v.len(),
            Batch::PrefixHashed(v) | Batch::UniquePrefix(v) | Batch::FullHashed(v) => v.len(),
            Batch::Hashed(v) => v.len(),
            Batch::Error(v) => v.len(),
            Batch::PhaseComplete { .. }
            | Batch::DeleteStale { .. }
            | Batch::Checkpoint
            | Batch::Flush(_)
            | Batch::Shutdown => 0,
        }
    }
}

pub struct WriterHandle {
    tx: SyncSender<Batch>,
    join: JoinHandle<Result<(), CoreError>>,
}

impl WriterHandle {
    pub fn send(&self, b: Batch) -> Result<(), CoreError> {
        self.tx.send(b).map_err(|_| CoreError::WriterDied)
    }

    /// Send a batch and block until the writer has committed (or failed).
    pub fn flush(&self) -> Result<(), CoreError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.send(Batch::Flush(tx))?;
        rx.recv().map_err(|_| CoreError::WriterDied)?
    }

    pub fn shutdown(self) -> Result<(), CoreError> {
        let _ = self.tx.send(Batch::Shutdown);
        self.join.join().map_err(|_| CoreError::WriterDied)?
    }
}

/// Spawn the writer thread. `prefix_total` is `F + L` from the config; the writer
/// needs it to store `prefix_bytes` from the `(id, hash)` batches defined in 4.5.
pub fn spawn_writer(conn: Connection, prefix_total: i64) -> WriterHandle {
    let (tx, rx) = sync_channel::<Batch>(4);
    let join = std::thread::Builder::new()
        .name("db-writer".into())
        .spawn(move || {
            run(conn, rx, prefix_total).inspect_err(|e| tracing::error!("db-writer failed: {e}"))
        })
        .expect("spawn db-writer");
    WriterHandle { tx, join }
}

fn run(mut conn: Connection, rx: Receiver<Batch>, prefix_total: i64) -> Result<(), CoreError> {
    let mut pending: Vec<Batch> = Vec::new();
    let mut rows = 0usize;
    let mut since = Instant::now();
    loop {
        let timeout = Duration::from_millis(BATCH_MS).saturating_sub(since.elapsed());
        match rx.recv_timeout(timeout) {
            Ok(Batch::Shutdown) => {
                commit(&mut conn, &mut pending, prefix_total)?;
                conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
                return Ok(());
            }
            Ok(Batch::Checkpoint) => {
                commit(&mut conn, &mut pending, prefix_total)?;
                rows = 0;
                since = Instant::now();
                conn.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()))?;
            }
            Ok(Batch::Flush(ack)) => {
                let res = commit(&mut conn, &mut pending, prefix_total);
                rows = 0;
                since = Instant::now();
                match res {
                    Ok(()) => {
                        let _ = ack.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = ack.send(Err(CoreError::WriterDied));
                        return Err(e);
                    }
                }
            }
            Ok(b @ Batch::PhaseComplete { .. }) => {
                pending.push(b);
                commit(&mut conn, &mut pending, prefix_total)?;
                rows = 0;
                since = Instant::now();
            }
            Ok(b) => {
                rows += b.row_count();
                pending.push(b);
                if rows >= BATCH_ROWS {
                    commit(&mut conn, &mut pending, prefix_total)?;
                    rows = 0;
                    since = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                commit(&mut conn, &mut pending, prefix_total)?;
                rows = 0;
                since = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => {
                commit(&mut conn, &mut pending, prefix_total)?;
                return Ok(());
            }
        }
    }
}

fn commit(
    conn: &mut Connection,
    pending: &mut Vec<Batch>,
    prefix_total: i64,
) -> Result<(), CoreError> {
    if pending.is_empty() {
        return Ok(());
    }
    let tx = conn.transaction()?;
    for b in pending.drain(..) {
        apply(&tx, b, prefix_total)?;
    }
    tx.commit()?;
    Ok(())
}

fn apply(tx: &Transaction<'_>, b: Batch, prefix_total: i64) -> Result<(), CoreError> {
    match b {
        Batch::Folders(rows) => {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO folders(id,volume_id,parent_id,name,last_seen_run_id) \
                 VALUES(?1,?2,?3,?4,?5) \
                 ON CONFLICT(volume_id,parent_id,name) DO UPDATE SET \
                 last_seen_run_id=excluded.last_seen_run_id",
            )?;
            for f in rows {
                stmt.execute(params![
                    f.id,
                    f.volume_id,
                    f.parent_id,
                    f.name.as_bytes(),
                    f.last_seen_run_id
                ])?;
            }
        }
        Batch::Enumerated(rows) => {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,\
                 ctime_ns,attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,\
                 last_seen_run_id) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) \
                 ON CONFLICT(volume_id,folder_id,name) DO UPDATE SET \
                 size=excluded.size, mtime_ns=excluded.mtime_ns, ctime_ns=excluded.ctime_ns, \
                 file_id=excluded.file_id, nlink=excluded.nlink, attrs=excluded.attrs, \
                 prefix_hash=excluded.prefix_hash, full_hash=excluded.full_hash, \
                 prefix_bytes=excluded.prefix_bytes, state=excluded.state, err=excluded.err, \
                 scanned_at=excluded.scanned_at, last_seen_run_id=excluded.last_seen_run_id \
                 ON CONFLICT(id) DO UPDATE SET \
                 folder_id=excluded.folder_id, name=excluded.name, size=excluded.size, \
                 mtime_ns=excluded.mtime_ns, ctime_ns=excluded.ctime_ns, \
                 file_id=excluded.file_id, nlink=excluded.nlink, attrs=excluded.attrs, \
                 prefix_hash=excluded.prefix_hash, full_hash=excluded.full_hash, \
                 prefix_bytes=excluded.prefix_bytes, state=excluded.state, err=excluded.err, \
                 scanned_at=excluded.scanned_at, last_seen_run_id=excluded.last_seen_run_id",
            )?;
            for f in rows {
                stmt.execute(params![
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
                ])?;
            }
        }
        Batch::SizeCandidate(ids) => {
            let mut stmt = tx.prepare_cached("UPDATE files SET state=1 WHERE id=?1")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        Batch::UniqueSize(ids) => {
            let mut stmt = tx.prepare_cached("UPDATE files SET state=-3 WHERE id=?1")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        Batch::PrefixHashed(items) => {
            let mut stmt = tx.prepare_cached(
                "UPDATE files SET prefix_hash=?1, \
                 prefix_bytes=CASE WHEN size<=?3 THEN size ELSE ?3 END, state=2 WHERE id=?2",
            )?;
            for (id, h) in items {
                stmt.execute(params![h.as_bytes(), id, prefix_total])?;
            }
        }
        Batch::UniquePrefix(items) => {
            let mut stmt = tx.prepare_cached(
                "UPDATE files SET prefix_hash=?1, \
                 prefix_bytes=CASE WHEN size<=?3 THEN size ELSE ?3 END, state=-4 WHERE id=?2",
            )?;
            for (id, h) in items {
                stmt.execute(params![h.as_bytes(), id, prefix_total])?;
            }
        }
        Batch::Hashed(items) => {
            let mut stmt = tx.prepare_cached(
                "UPDATE files SET prefix_hash=?1, full_hash=?2, \
                 prefix_bytes=CASE WHEN size<=?4 THEN size ELSE ?4 END, state=5 WHERE id=?3",
            )?;
            for (id, p, f) in items {
                stmt.execute(params![p.as_bytes(), f.as_bytes(), id, prefix_total])?;
            }
        }
        Batch::FullHashed(items) => {
            let mut stmt =
                tx.prepare_cached("UPDATE files SET full_hash=?1, state=5 WHERE id=?2")?;
            for (id, h) in items {
                stmt.execute(params![h.as_bytes(), id])?;
            }
        }
        Batch::Verified(ids) => {
            let mut stmt = tx.prepare_cached("UPDATE files SET state=5 WHERE id=?1 AND state=3")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        Batch::Error(rows) => {
            let mut set_err = tx.prepare_cached("UPDATE files SET state=-2, err=?1 WHERE id=?2")?;
            let mut ins = tx.prepare_cached(
                "INSERT INTO errors(run_id,file_row_id,path,kind,message) VALUES(?1,?2,?3,?4,?5)",
            )?;
            for e in rows {
                if let Some(id) = e.file_row_id {
                    set_err.execute(params![e.kind.as_i64(), id])?;
                }
                ins.execute(params![
                    e.run_id,
                    e.file_row_id,
                    e.path.as_ref().map(|p| p.as_bytes()),
                    e.kind.as_i64(),
                    e.message,
                ])?;
            }
        }
        Batch::PhaseComplete {
            run_id,
            volume_id,
            phase,
        } => {
            tx.prepare_cached(
                "INSERT INTO run_volumes(run_id,volume_id,phase_reached) VALUES(?1,?2,?3) \
                 ON CONFLICT(run_id,volume_id) DO UPDATE SET phase_reached=excluded.phase_reached",
            )?
            .execute(params![run_id, volume_id, phase.as_i64()])?;
        }
        Batch::DeleteStale {
            run_id,
            volume_id,
            root_folder_ids,
        } => {
            if root_folder_ids.is_empty() {
                return Ok(());
            }
            let list = root_folder_ids
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let sql_files = format!(
                "WITH RECURSIVE sub(id) AS ( \
                   SELECT id FROM folders WHERE id IN ({list}) \
                   UNION ALL \
                   SELECT f.id FROM folders f JOIN sub s ON f.parent_id = s.id \
                 ) \
                 DELETE FROM files WHERE volume_id=?1 AND last_seen_run_id < ?2 \
                 AND folder_id IN (SELECT id FROM sub)"
            );
            tx.execute(&sql_files, params![volume_id, run_id])?;
            let sql_folders = format!(
                "WITH RECURSIVE sub(id) AS ( \
                   SELECT id FROM folders WHERE id IN ({list}) \
                   UNION ALL \
                   SELECT f.id FROM folders f JOIN sub s ON f.parent_id = s.id \
                 ) \
                 DELETE FROM folders WHERE volume_id=?1 AND last_seen_run_id IS NOT NULL \
                 AND last_seen_run_id < ?2 AND id IN (SELECT id FROM sub)"
            );
            tx.execute(&sql_folders, params![volume_id, run_id])?;
        }
        Batch::Checkpoint | Batch::Flush(_) | Batch::Shutdown => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::path::RawPath;

    fn test_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        let db = crate::db::Db::open(&path, &Config::default()).unwrap();
        db.conn
            .execute(
                "INSERT INTO volumes(id,uuid,uuid_kind,mount,fs_type,profile,first_seen) \
                 VALUES (1,'u','fs_uuid',x'2f',?1,'unknown',0)",
                ["ext4"],
            )
            .unwrap();
        (dir, db.conn)
    }

    #[test]
    fn phase_complete_never_leads_rows() {
        let (_d, mut conn) = test_conn();
        let mut pending = vec![
            Batch::Hashed(vec![(1, Hash([1; 16]), Hash([2; 16]))]),
            Batch::PhaseComplete {
                run_id: 1,
                volume_id: 1,
                phase: Phase::Enumerated,
            },
        ];
        // Force a commit in the same txn as the phase marker.
        conn.execute(
            "INSERT INTO scan_runs(id,started_at,cli_args,profile,status) VALUES(1,0,'','ssd','running')",
            [],
        )
        .unwrap();
        commit(&mut conn, &mut pending, 20 * 1024).unwrap();
        let phase: i64 = conn
            .query_row(
                "SELECT phase_reached FROM run_volumes WHERE run_id=1 AND volume_id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, Phase::Enumerated.as_i64());
    }

    #[test]
    fn replay_enumerated_is_idempotent() {
        let (_d, mut conn) = test_conn();
        conn.execute(
            "INSERT INTO volumes(id,uuid,uuid_kind,mount,fs_type,profile,first_seen) VALUES (2,'v','fs_uuid',x'2f',?1,'unknown',0)",
            ["ext4"],
        )
        .unwrap();
        let entry = FileEntry::enumerated(1, 2, 1, RawPath::new(b"a".to_vec()), 1);
        for _ in 0..2 {
            let mut pending = vec![Batch::Enumerated(vec![entry.clone()])];
            commit(&mut conn, &mut pending, 20 * 1024).unwrap();
        }
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }
}
