//! In-memory working set: `HashMap<Hash, SmallVec<[i64; 2]>>` with budget accounting.

use std::collections::HashMap;

use smallvec::SmallVec;

use crate::error::Result;
use crate::model::Hash;

use super::Index;

/// Rough per-entry overhead: hash (16) + SmallVec inline capacity (2 * 8) plus
/// map bucket slack. Deliberately conservative.
const ENTRY_OVERHEAD: usize = 48;
const ID_BYTES: usize = 8;

pub struct MemoryIndex {
    map: HashMap<Hash, SmallVec<[i64; 2]>>,
    ids: usize,
    budget: usize,
}

impl MemoryIndex {
    pub fn new(budget: usize) -> Self {
        MemoryIndex {
            map: HashMap::new(),
            ids: 0,
            budget,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// True once the map exceeds its budget; the caller switches to DB-backed mode.
    pub fn over_budget(&self) -> bool {
        self.approx_bytes() > self.budget
    }

    /// Number of ids recorded under `key` (used to classify partners).
    pub fn count(&self, key: &Hash) -> usize {
        self.map.get(key).map(|v| v.len()).unwrap_or(0)
    }

    /// Highest `count` seen; useful when deciding whether a new pass fits.
    pub fn distinct_keys(&self) -> usize {
        self.map.len()
    }
}

impl Index for MemoryIndex {
    fn insert(&mut self, key: Hash, id: i64) -> Result<Vec<i64>> {
        self.ids += 1;
        match self.map.get_mut(&key) {
            Some(v) => {
                let partners = v.iter().copied().collect();
                v.push(id);
                Ok(partners)
            }
            None => {
                let mut v = SmallVec::new();
                v.push(id);
                self.map.insert(key, v);
                Ok(Vec::new())
            }
        }
    }

    fn singletons(&self) -> Box<dyn Iterator<Item = i64> + '_> {
        Box::new(self.map.values().filter(|v| v.len() == 1).map(|v| v[0]))
    }

    fn approx_bytes(&self) -> usize {
        self.map.len() * ENTRY_OVERHEAD + self.ids * ID_BYTES
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partners_and_singletons() {
        let mut ix = MemoryIndex::new(1 << 20);
        assert!(ix.insert(Hash([1; 16]), 10).unwrap().is_empty());
        assert_eq!(ix.insert(Hash([1; 16]), 11).unwrap(), vec![10]);
        ix.insert(Hash([2; 16]), 12).unwrap();
        assert_eq!(ix.count(&Hash([1; 16])), 2);
        let singles: Vec<i64> = ix.singletons().collect();
        assert_eq!(singles, vec![12]);
    }

    #[test]
    fn budget_trips() {
        let mut ix = MemoryIndex::new(64);
        for i in 0..100 {
            ix.insert(Hash([(i % 7) as u8; 16]), i).unwrap();
        }
        assert!(ix.over_budget());
    }
}
