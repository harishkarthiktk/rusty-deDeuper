//! Pre-assigned row ids so the pipeline knows `files.id` before the writer commits.

use std::sync::atomic::{AtomicI64, Ordering};

use rusqlite::Connection;

use crate::error::CoreError;

pub struct IdAllocator {
    next_file: AtomicI64,
    next_folder: AtomicI64,
}

impl IdAllocator {
    /// Seed from `MAX(id) + 1`, separately for files and folders.
    pub fn seed(conn: &Connection) -> Result<Self, CoreError> {
        let max = |table: &str| -> Result<i64, CoreError> {
            Ok(conn.query_row(
                &format!("SELECT COALESCE(MAX(id), 0) FROM {table}"),
                [],
                |r| r.get(0),
            )?)
        };
        Ok(Self {
            next_file: AtomicI64::new(max("files")? + 1),
            next_folder: AtomicI64::new(max("folders")? + 1),
        })
    }

    pub fn file(&self) -> i64 {
        self.next_file.fetch_add(1, Ordering::Relaxed)
    }

    pub fn folder(&self) -> i64 {
        self.next_folder.fetch_add(1, Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn seeds_on_empty_and_existing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("i.db");
        let db = crate::db::Db::open(&path, &Config::default()).unwrap();

        let a = IdAllocator::seed(&db.conn).unwrap();
        assert_eq!(a.file(), 1);
        assert_eq!(a.folder(), 1);

        db.conn
            .execute(
                "INSERT INTO volumes(id,uuid,uuid_kind,mount,fs_type,profile,first_seen) \
                 VALUES (1,'u','fs_uuid',x'2f',?1,'unknown',0)",
                ["ext4"],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO folders(id,volume_id,parent_id,name) VALUES (7,1,0,x'')",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO files(id,volume_id,folder_id,name,size,mtime_ns,attrs,state,last_seen_run_id) \
                 VALUES (42,1,7,x'66',0,0,0,0,0)",
                [],
            )
            .unwrap();

        let b = IdAllocator::seed(&db.conn).unwrap();
        assert_eq!(b.file(), 43);
        assert_eq!(b.folder(), 8);
    }
}
