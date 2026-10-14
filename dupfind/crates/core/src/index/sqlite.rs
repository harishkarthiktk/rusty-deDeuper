//! DB-backed fallback for over-budget passes.
//!
//! Memory is bounded by the number of *distinct keys* seen (typically a small
//! fraction of files); partner ids are read from the committed `files` rows via
//! `idx_files_prefix` / `idx_files_full`. Because the pipeline's writes are async,
//! the caller must commit the pass (via a writer `flush`) before partner ids are
//! visible here. The switch is logged at `warn`.

use std::collections::HashSet;

use rusqlite::Connection;

use crate::error::Result;
use crate::model::Hash;

use super::Index;

pub struct SqliteIndex<'a> {
    conn: &'a Connection,
    volume_id: i64,
    /// volume-agnostic when `false` (full hash spans volumes).
    per_volume: bool,
    keys: HashSet<Hash>,
}

impl<'a> SqliteIndex<'a> {
    pub fn new(conn: &'a Connection, volume_id: i64, per_volume: bool) -> Self {
        tracing::warn!(
            volume_id,
            per_volume,
            "index over budget: switching to DB-backed mode"
        );
        SqliteIndex {
            conn,
            volume_id,
            per_volume,
            keys: HashSet::new(),
        }
    }

    fn partners(&self, key: &Hash) -> Result<Vec<i64>> {
        if self.per_volume {
            crate::db::queries::prefix_partners(self.conn, self.volume_id, key)
        } else {
            crate::db::queries::full_partners(self.conn, key)
        }
    }
}

impl Index for SqliteIndex<'_> {
    fn insert(&mut self, key: Hash, id: i64) -> Result<Vec<i64>> {
        let partners = if self.keys.contains(&key) {
            self.partners(&key)?
        } else {
            Vec::new()
        };
        self.keys.insert(key);
        let _ = id;
        Ok(partners)
    }

    fn singletons(&self) -> Box<dyn Iterator<Item = i64> + '_> {
        // Not expressible without a per-key cursor; the DB-backed path classifies
        // singletons with `queries::singleton_prefixes` instead.
        Box::new(std::iter::empty())
    }

    fn approx_bytes(&self) -> usize {
        self.keys.len() * 16
    }
}
