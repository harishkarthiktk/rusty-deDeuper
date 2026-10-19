//! Connection handling and pragma policy. See `.plan.md` 4.16.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use crate::config::Config;
use crate::error::CoreError;

pub mod ids;
pub mod lock;
pub mod queries;
pub mod schema;
pub mod writer;

pub const WAL_AUTOCHECKPOINT_PAGES: i64 = 10_000;

pub struct Db {
    pub conn: Connection,
}

impl Db {
    /// Read-write open: pragmas, then migrations.
    pub fn open(path: &Path, cfg: &Config) -> Result<Self, CoreError> {
        let conn = Connection::open(path)?;
        apply_pragmas(&conn, cfg)?;
        schema::apply_migrations(&conn)?;
        Ok(Self { conn })
    }

    /// Read-only open for `report`, `lookup` and the pipeline's read connection.
    pub fn open_readonly(path: &Path) -> Result<Self, CoreError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        schema::check_version(&conn)?;
        Ok(Self { conn })
    }
}

/// Pragma order matters: `busy_timeout` first (the WAL switch needs a lock).
///
/// `journal_mode = WAL` returns the *resulting* mode. SQLite silently keeps the
/// old mode on filesystems that cannot do WAL, so we check and fail loudly.
pub fn apply_pragmas(conn: &Connection, _cfg: &Config) -> Result<(), CoreError> {
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(CoreError::WalUnavailable { got: mode });
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "wal_autocheckpoint", WAL_AUTOCHECKPOINT_PAGES)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_db_reports_non_wal() {
        // `:memory:` cannot use WAL; it returns "memory".
        let conn = Connection::open_in_memory().unwrap();
        let cfg = Config::default();
        let err = apply_pragmas(&conn, &cfg).unwrap_err();
        match err {
            CoreError::WalUnavailable { got } => assert_eq!(got.to_ascii_lowercase(), "memory"),
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn file_db_enables_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.db");
        let db = Db::open(&path, &Config::default()).unwrap();
        let mode: String = db
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_ascii_lowercase(), "wal");
        assert_eq!(
            schema::current_version(&db.conn).unwrap(),
            schema::EXPECTED_VERSION
        );
    }
}
