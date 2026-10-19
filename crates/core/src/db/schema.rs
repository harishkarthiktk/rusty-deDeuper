//! Schema versioning. `PRAGMA user_version` is the only version source.

use rusqlite::Connection;

use crate::error::CoreError;

pub const EXPECTED_VERSION: i64 = 2;
const SCHEMA: &str = include_str!("schema/v2.sql");

pub fn current_version(conn: &Connection) -> Result<i64, CoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

pub fn check_version(conn: &Connection) -> Result<(), CoreError> {
    match current_version(conn)? {
        v if v == EXPECTED_VERSION => Ok(()),
        v => Err(CoreError::SchemaVersion {
            found: v,
            expected: EXPECTED_VERSION,
        }),
    }
}

/// Create the schema on a fresh DB; refuse every other version.
///
/// There are no migrations: an older database must be deleted and rescanned.
pub fn apply_migrations(conn: &Connection) -> Result<(), CoreError> {
    let v = current_version(conn)?;
    if v != 0 && v != EXPECTED_VERSION {
        return Err(CoreError::SchemaVersion {
            found: v,
            expected: EXPECTED_VERSION,
        });
    }
    if v == 0 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", EXPECTED_VERSION)?;
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.db");
        let db = crate::db::Db::open(&path, &Config::default()).unwrap();
        (dir, db.conn)
    }

    #[test]
    fn apply_is_idempotent() {
        let (_dir, conn) = temp_db();
        apply_migrations(&conn).unwrap();
        apply_migrations(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), EXPECTED_VERSION);
    }

    #[test]
    fn refuse_newer_version() {
        let (_dir, conn) = temp_db();
        conn.pragma_update(None, "user_version", 999).unwrap();
        let err = check_version(&conn).unwrap_err();
        assert!(matches!(
            err,
            CoreError::SchemaVersion {
                found: 999,
                expected: EXPECTED_VERSION
            }
        ));
        assert!(apply_migrations(&conn).is_err());
    }
}
