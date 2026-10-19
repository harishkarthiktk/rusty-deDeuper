//! Run-scoped DB-backed fallback for over-budget passes (4.6). Partner and
//! singleton classification is query-driven: the pipeline commits the pass via a
//! writer flush, then this cursor reads `idx_files_prefix` / `idx_files_full`.

use rusqlite::Connection;

use crate::error::Result;
use crate::model::Hash;

use super::Index;

pub struct SqliteIndex<'a> {
    conn: &'a Connection,
    volume_id: i64,
    run_id: i64,
}

impl<'a> SqliteIndex<'a> {
    pub fn new(conn: &'a Connection, volume_id: i64, run_id: i64) -> Self {
        tracing::warn!(
            volume_id,
            run_id,
            "index over budget: DB-backed classification"
        );
        SqliteIndex {
            conn,
            volume_id,
            run_id,
        }
    }

    /// Ids (state 2, this run) whose prefix hash has no partner (singletons).
    pub fn prefix_singletons(&self) -> Result<Vec<i64>> {
        crate::db::queries::singleton_prefixes(self.conn, self.volume_id, self.run_id)
    }

    /// Streamed grouped cursor: `(prefix_hash, ids)` for committed state-2 rows,
    /// ordered by prefix_hash via `idx_files_prefix`. Bounded by group count.
    pub fn prefix_groups(&self) -> Result<Vec<(Hash, Vec<i64>)>> {
        crate::db::queries::prefix_groups(self.conn, self.volume_id, self.run_id)
    }
}

impl Index for SqliteIndex<'_> {
    fn insert(&mut self, key: Hash, id: i64) -> Result<Vec<i64>> {
        let mut partners =
            crate::db::queries::prefix_partners_run(self.conn, self.volume_id, self.run_id, &key)?;
        partners.retain(|p| *p != id);
        Ok(partners)
    }

    fn singletons(&self) -> Box<dyn Iterator<Item = i64> + '_> {
        match self.prefix_singletons() {
            Ok(v) => Box::new(v.into_iter()),
            Err(_) => Box::new(std::iter::empty()),
        }
    }

    fn approx_bytes(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::{queries, Db};
    use crate::model::{FileEntry, FolderRow, FsType, Hash, State, UuidKind};
    use crate::path::RawPath;
    use crate::traits::VolumeInfo;

    fn fixture() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("i.db"), &Config::default()).unwrap();
        let info = VolumeInfo {
            uuid: "v".into(),
            uuid_kind: UuidKind::FsUuid,
            mount: RawPath::new(b"/".to_vec()),
            fs_type: FsType::Ext4,
            label: None,
        };
        let vid = queries::upsert_volume(&db.conn, &info, 0).unwrap();
        queries::insert_folder(
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
        (dir, db.conn)
    }

    fn put(conn: &rusqlite::Connection, id: i64, h: Hash) {
        let mut f = FileEntry::enumerated(id, 1, 1, RawPath::new(format!("f{id}").into_bytes()), 1);
        f.state = State::PrefixHashed;
        f.prefix_hash = Some(h);
        queries::upsert_file(conn, &f).unwrap();
    }

    #[test]
    fn prefix_singletons_and_partners_are_run_scoped() {
        let (_d, conn) = fixture();
        let h = Hash([3; 16]);
        put(&conn, 10, h); // partner
        put(&conn, 11, h); // partner
        put(&conn, 12, Hash([4; 16])); // singleton
        let mut ix = SqliteIndex::new(&conn, 1, 1);
        let singles = ix.prefix_singletons().unwrap();
        assert_eq!(singles, vec![12]);
        let mut partners = ix.insert(h, 10).unwrap();
        partners.sort();
        assert_eq!(partners, vec![11]);
        let groups = ix.prefix_groups().unwrap();
        assert_eq!(groups.len(), 2);
    }
}
