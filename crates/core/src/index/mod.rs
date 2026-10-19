//! Partner lookup for one pass (`prefix_map` / `full_map`).
//!
//! The memory impl lives in [`memory`]; the over-budget DB-backed fallback in
//! [`sqlite`]. See `.overall_plan.md` 4.6.

use crate::error::Result;
use crate::model::Hash;

pub mod memory;
pub mod sqlite;

pub use memory::MemoryIndex;
pub use sqlite::SqliteIndex;

/// Partner lookup for one pass.
pub trait Index {
    /// Record that `id` has `key`. Returns ids already recorded under `key`
    /// (its partners so far).
    fn insert(&mut self, key: Hash, id: i64) -> Result<Vec<i64>>;

    /// Ids whose key had no partner by the end of the pass.
    fn singletons(&self) -> Box<dyn Iterator<Item = i64> + '_>;

    /// Approximate bytes used; the memory impl compares against `cfg.mem_budget`.
    fn approx_bytes(&self) -> usize;
}
