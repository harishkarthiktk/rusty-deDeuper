//! Batched writer thread. One thread owns the `rusqlite::Connection`; hash workers
//! send [`Batch`] messages over a bounded channel. See `.overall_plan.md` 4.5.

use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, Transaction};

use crate::db::queries;
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
    /// Like `FullHashed` but lands in state 3 (awaiting byte verify). Only sent when verify == Byte.
    FullHashedPending(Vec<(i64, Hash)>),
    /// Like `Hashed` (small files, prefix+full together) but lands in state 3.
    /// Only sent when verify == Byte.
    HashedPending(Vec<(i64, Hash, Hash)>),
    Verified(Vec<i64>),
    /// Move this run's leftover state-3 rows to state 5. Sent only when verify is
    /// off: a verify-on run interrupted after full hash leaves them in 3.
    PromotePending {
        volume_id: i64,
        run_id: i64,
    },
    Error(Vec<ErrRow>),
    /// Move this run's state-2 rows whose prefix hash is unique to -4. Sent right before
    /// `PhaseComplete{PrefixDone}` so both commit in one transaction.
    MarkPrefixSingletons {
        volume_id: i64,
        run_id: i64,
    },
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
        /// Folders the walker could not read. Their subtrees keep their rows until a run
        /// that can read them.
        protected_folder_ids: Vec<i64>,
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
            Batch::Hashed(v) | Batch::HashedPending(v) => v.len(),
            Batch::FullHashedPending(v) => v.len(),
            Batch::Error(v) => v.len(),
            Batch::PhaseComplete { .. }
            | Batch::DeleteStale { .. }
            | Batch::MarkPrefixSingletons { .. }
            | Batch::PromotePending { .. }
            | Batch::Checkpoint
            | Batch::Flush(_)
            | Batch::Shutdown => 0,
        }
    }
}

pub struct WriterHandle {
    tx: SyncSender<Batch>,
    join: Option<JoinHandle<Result<(), CoreError>>>,
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

    /// Commit everything sent so far and join the thread. Returns the writer's real
    /// error, not the `WriterDied` a sender saw.
    pub fn shutdown(mut self) -> Result<(), CoreError> {
        self.join_inner()
    }

    fn join_inner(&mut self) -> Result<(), CoreError> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        // Ignored: a send error means the writer already exited; join reports why.
        let _ = self.tx.send(Batch::Shutdown);
        match join.join() {
            Ok(res) => res,
            Err(payload) => Err(CoreError::Other(format!(
                "db writer panicked: {}",
                panic_message(payload.as_ref())
            ))),
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        // Safety net for unwinding and early returns: commit what was sent and join.
        if let Err(e) = self.join_inner() {
            tracing::error!("db writer failed during drop: {e}");
        }
    }
}

/// Text of a panic payload (`&str` or `String`), for error messages.
pub(crate) fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

/// Spawn the writer thread. `prefix_total` is `F + L` from the config; the writer
/// needs it to store `prefix_bytes` from the `(id, hash)` batches defined in 4.5.
pub fn spawn_writer(conn: Connection, prefix_total: i64) -> Result<WriterHandle, CoreError> {
    spawn_writer_with(conn, prefix_total, BATCH_ROWS, BATCH_MS)
}

/// Spawn the writer thread with explicit flush thresholds. Production callers use
/// [`spawn_writer`]; tests lower the thresholds to observe a flush deterministically.
pub fn spawn_writer_with(
    conn: Connection,
    prefix_total: i64,
    max_rows: usize,
    max_ms: u64,
) -> Result<WriterHandle, CoreError> {
    let (tx, rx) = sync_channel::<Batch>(4);
    let join = std::thread::Builder::new()
        .name("db-writer".into())
        .spawn(move || {
            run(conn, rx, prefix_total, max_rows, max_ms)
                .inspect_err(|e| tracing::error!("db-writer failed: {e}"))
        })?;
    Ok(WriterHandle {
        tx,
        join: Some(join),
    })
}

fn run(
    mut conn: Connection,
    rx: Receiver<Batch>,
    prefix_total: i64,
    max_rows: usize,
    max_ms: u64,
) -> Result<(), CoreError> {
    let mut pending: Vec<Batch> = Vec::new();
    let mut rows = 0usize;
    let mut since = Instant::now();
    let max_wait = Duration::from_millis(max_ms);
    loop {
        let timeout = max_wait.saturating_sub(since.elapsed());
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
                // `recv_timeout` returns queued messages even past the deadline, so the
                // time-based commit must also be checked here, not only on Timeout.
                if rows >= max_rows || since.elapsed() >= max_wait {
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
            for f in rows {
                queries::upsert_file(tx, &f)?;
            }
        }
        Batch::MarkPrefixSingletons { volume_id, run_id } => {
            // Counts -4 rows of this run too, so a resumed pass cannot orphan a partner.
            tx.prepare_cached(
                "UPDATE files SET state=-4 \
                 WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2 \
                 AND prefix_hash IN ( \
                   SELECT prefix_hash FROM files \
                   WHERE volume_id=?1 AND last_seen_run_id=?2 AND state IN (2,-4) \
                     AND prefix_hash IS NOT NULL \
                   GROUP BY prefix_hash HAVING COUNT(*)=1)",
            )?
            .execute(params![volume_id, run_id])?;
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
        Batch::HashedPending(items) => {
            let mut stmt = tx.prepare_cached(
                "UPDATE files SET prefix_hash=?1, full_hash=?2, \
                 prefix_bytes=CASE WHEN size<=?4 THEN size ELSE ?4 END, state=3 WHERE id=?3",
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
        Batch::FullHashedPending(items) => {
            let mut stmt =
                tx.prepare_cached("UPDATE files SET full_hash=?1, state=3 WHERE id=?2")?;
            for (id, h) in items {
                stmt.execute(params![h.as_bytes(), id])?;
            }
        }
        Batch::Verified(ids) => {
            let mut stmt = tx.prepare_cached("UPDATE files SET state=4 WHERE id=?1 AND state=3")?;
            for id in ids {
                stmt.execute([id])?;
            }
        }
        Batch::PromotePending { volume_id, run_id } => {
            tx.prepare_cached(
                "UPDATE files SET state=5 WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=3",
            )?
            .execute(params![volume_id, run_id])?;
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
            protected_folder_ids,
        } => {
            if root_folder_ids.is_empty() {
                return Ok(());
            }
            let join = |ids: &[i64]| {
                ids.iter()
                    .map(|i| i.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let list = join(&root_folder_ids);
            // The clause is built only for a non-empty list, so `NOT IN (NULL)` never occurs.
            let (root_guard, step_guard) = if protected_folder_ids.is_empty() {
                (String::new(), String::new())
            } else {
                let prot = join(&protected_folder_ids);
                (
                    format!(" AND id NOT IN ({prot})"),
                    format!(" WHERE f.id NOT IN ({prot})"),
                )
            };
            let cte = format!(
                "WITH RECURSIVE sub(id) AS ( \
                   SELECT id FROM folders WHERE id IN ({list}){root_guard} \
                   UNION ALL \
                   SELECT f.id FROM folders f JOIN sub s ON f.parent_id = s.id{step_guard} \
                 ) "
            );
            tx.execute(
                &format!(
                    "{cte}DELETE FROM files WHERE volume_id=?1 AND last_seen_run_id < ?2 \
                     AND folder_id IN (SELECT id FROM sub)"
                ),
                params![volume_id, run_id],
            )?;
            tx.execute(
                &format!(
                    "{cte}DELETE FROM folders WHERE volume_id=?1 AND last_seen_run_id IS NOT NULL \
                     AND last_seen_run_id < ?2 AND id IN (SELECT id FROM sub)"
                ),
                params![volume_id, run_id],
            )?;
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
        // files.folder_id has a FK to folders(id); the fixture needs the folder row.
        conn.execute(
            "INSERT INTO folders(id,volume_id,parent_id,name,last_seen_run_id) \
             VALUES (1,2,0,x'',NULL)",
            [],
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
        // Replay must be byte-identical, not merely the same row count.
        let got = crate::db::queries::find_by_path(&conn, 2, 1, &RawPath::new(b"a".to_vec()))
            .unwrap()
            .expect("replayed row present");
        assert_eq!(got, entry);
    }

    #[test]
    fn writer_flushes_at_row_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        {
            let db = crate::db::Db::open(&path, &Config::default()).unwrap();
            db.conn
                .execute(
                    "INSERT INTO volumes(id,uuid,uuid_kind,mount,fs_type,profile,first_seen) \
                     VALUES (1,'u','fs_uuid',x'2f','ext4','unknown',0)",
                    [],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO folders(id,volume_id,parent_id,name,last_seen_run_id) \
                     VALUES (1,1,0,x'',NULL)",
                    [],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO scan_runs(id,started_at,cli_args,profile,status) \
                     VALUES (1,0,'','ssd','running')",
                    [],
                )
                .unwrap();
            for i in 1..=4 {
                db.conn
                    .execute(
                        "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,\
                         ctime_ns,attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,\
                         last_seen_run_id) VALUES(?1,1,1,?2,NULL,1,1,0,NULL,0,NULL,NULL,NULL,0,NULL,NULL,1)",
                        params![i, format!("f{i}")],
                    )
                    .unwrap();
            }
        }

        let writer_conn = crate::db::Db::open(&path, &Config::default()).unwrap().conn;
        let handle = spawn_writer_with(writer_conn, 20 * 1024, 2, 60_000).unwrap();
        // 2 rows in one batch reaches the threshold; auto-commit, no explicit Flush.
        handle.send(Batch::SizeCandidate(vec![1, 2])).unwrap();

        let reader = crate::db::Db::open_readonly(&path).unwrap().conn;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let n: i64 = reader
                .query_row(
                    "SELECT COUNT(*) FROM files WHERE id IN (1,2) AND state=1",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if n == 2 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "writer did not flush at threshold"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        handle.shutdown().unwrap();
    }

    #[test]
    fn writer_flushes_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        {
            let db = crate::db::Db::open(&path, &Config::default()).unwrap();
            db.conn
                .execute(
                    "INSERT INTO volumes(id,uuid,uuid_kind,mount,fs_type,profile,first_seen) \
                     VALUES (1,'u','fs_uuid',x'2f','ext4','unknown',0)",
                    [],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO folders(id,volume_id,parent_id,name,last_seen_run_id) \
                     VALUES (1,1,0,x'',NULL)",
                    [],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,\
                     ctime_ns,attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,\
                     last_seen_run_id) VALUES(1,1,1,'a',NULL,1,1,0,NULL,0,NULL,NULL,NULL,0,NULL,NULL,1)",
                    [],
                )
                .unwrap();
        }

        let writer_conn = crate::db::Db::open(&path, &Config::default()).unwrap().conn;
        // High row threshold and no time flush: only Shutdown can commit this.
        let handle = spawn_writer_with(writer_conn, 20 * 1024, 1_000_000, 600_000).unwrap();
        handle.send(Batch::SizeCandidate(vec![1])).unwrap();
        handle.shutdown().unwrap();

        let reader = crate::db::Db::open_readonly(&path).unwrap().conn;
        let n: i64 = reader
            .query_row(
                "SELECT COUNT(*) FROM files WHERE id=1 AND state=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "Shutdown must flush pending rows");
    }
}
