//! The naive Phase 0 pipeline: enumerate -> size -> prefix -> full -> report.
//!
//! No DB, no resume, no parallelism. This exists to prove end-to-end correctness;
//! Phase 1 adds the batched DB pipeline alongside it.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::cancel::Token;
use crate::config::Config;
use crate::error::Result;
use crate::hash::{hash_full, hash_prefix};
use crate::model::{attrs, Hash};
use crate::path::RawPath;
use crate::report::{Group, Member, Report};
use crate::traits::{ContentReader, Enumerator, Hasher, PathCodec, WalkItem};

#[derive(Clone, Debug, Default)]
pub struct ScanStats {
    pub files_enumerated: usize,
    pub files_skipped: usize,
    pub files_errored: usize,
    pub bytes_hashed: u64,
}

struct Rec {
    path: RawPath,
    volume_id: i64,
    /// Grouping key for the by-size pass, copied from `FileMeta::size`.
    size: i64,
    /// `size` checked once at enumeration, used for hashing and stats.
    bytes: u64,
    /// `(dev, file_id)` from `identity::run_key`; `None` means a per-row key.
    file_id_key: Option<Vec<u8>>,
}

impl Rec {
    fn inode_key(&self, fallback_idx: usize) -> Vec<u8> {
        match &self.file_id_key {
            Some(b) => b.clone(),
            None => {
                let mut k = b"idx:".to_vec();
                k.extend_from_slice(&fallback_idx.to_be_bytes());
                k
            }
        }
    }
}

fn distinct_count(recs: &[Rec], idxs: &[usize]) -> usize {
    let mut set: HashSet<Vec<u8>> = HashSet::new();
    for &i in idxs {
        set.insert(recs[i].inode_key(i));
    }
    set.len()
}

/// Run the Phase 0 algorithm. `volume_id` is 0 (no volume identity in Phase 0).
pub fn scan_naive(
    roots: &[PathBuf],
    enumr: &dyn Enumerator,
    reader: &dyn ContentReader,
    hasher: &dyn Hasher,
    codec: &dyn PathCodec,
    cfg: &Config,
    cancel: &Token,
) -> Result<(Report, ScanStats)> {
    let _ = codec; // kept for signature symmetry with the DB pipeline
    let mut stats = ScanStats::default();
    let mut recs: Vec<Rec> = Vec::new();

    for root in roots {
        cancel.check()?;
        enumr.walk(root, cfg, &mut |item| {
            if matches!(item, WalkItem::Error { .. }) {
                stats.files_errored += 1;
                return Ok(());
            }
            let WalkItem::File { path, meta } = item else {
                return Ok(());
            };
            match meta.kind {
                crate::traits::EntryKind::Directory => return Ok(()),
                crate::traits::EntryKind::Symlink => {
                    stats.files_skipped += 1;
                    return Ok(());
                }
                crate::traits::EntryKind::Other => {
                    stats.files_skipped += 1;
                    return Ok(());
                }
                crate::traits::EntryKind::RegularFile => {}
            }
            if meta.size == 0 {
                stats.files_skipped += 1;
                return Ok(());
            }
            if attrs::has(meta.attrs, attrs::PLACEHOLDER) && !cfg.include_placeholders {
                stats.files_skipped += 1;
                return Ok(());
            }
            // Corrupt metadata counts as an error for this file only; the scan goes on.
            let Ok(bytes) = meta.size_bytes() else {
                stats.files_errored += 1;
                return Ok(());
            };
            stats.files_enumerated += 1;
            recs.push(Rec {
                path,
                volume_id: 0,
                size: meta.size,
                bytes,
                file_id_key: crate::identity::run_key(&meta),
            });
            Ok(())
        })?;
    }

    // Pass: size.
    let mut by_size: HashMap<i64, Vec<usize>> = HashMap::new();
    for (i, r) in recs.iter().enumerate() {
        by_size.entry(r.size).or_default().push(i);
    }
    let mut size_candidates: Vec<usize> = Vec::new();
    for idxs in by_size.values() {
        if distinct_count(&recs, idxs) > 1 {
            size_candidates.extend_from_slice(idxs);
        }
    }

    // Pass: prefix.
    let mut prefix_hashes: HashMap<usize, Hash> = HashMap::new();
    for &i in &size_candidates {
        cancel.check()?;
        let r = &recs[i];
        let native = codec.raw_to_path(&r.path);
        match hash_prefix(reader, &native, r.bytes, cfg) {
            Ok((h, _)) => {
                stats.bytes_hashed += r.bytes;
                prefix_hashes.insert(i, h);
            }
            Err(_) => stats.files_errored += 1,
        }
    }
    let mut by_prefix: HashMap<(i64, Hash), Vec<usize>> = HashMap::new();
    for (&i, &h) in &prefix_hashes {
        by_prefix.entry((recs[i].size, h)).or_default().push(i);
    }
    let mut prefix_survivors: Vec<usize> = Vec::new();
    for idxs in by_prefix.values() {
        if distinct_count(&recs, idxs) > 1 {
            prefix_survivors.extend_from_slice(idxs);
        }
    }

    // Pass: full.
    let mut full_hashes: HashMap<usize, Hash> = HashMap::new();
    for &i in &prefix_survivors {
        cancel.check()?;
        let r = &recs[i];
        let native = codec.raw_to_path(&r.path);
        match hash_full(reader, &native, r.bytes) {
            Ok(h) => {
                stats.bytes_hashed += r.bytes;
                full_hashes.insert(i, h);
            }
            Err(_) => stats.files_errored += 1,
        }
    }
    let mut by_full: HashMap<(i64, Hash), Vec<usize>> = HashMap::new();
    for (&i, &h) in &full_hashes {
        by_full.entry((recs[i].size, h)).or_default().push(i);
    }

    let mut groups = Vec::new();
    for ((size, h), idxs) in by_full {
        let distinct = distinct_count(&recs, &idxs);
        if distinct <= 1 {
            continue;
        }
        let members: Vec<Member> = idxs
            .iter()
            .map(|&i| Member {
                volume_id: recs[i].volume_id,
                path: recs[i].path.clone(),
                size,
            })
            .collect();
        groups.push(Group {
            full_hash: h,
            size,
            distinct_inodes: distinct as i64,
            wasted_logical: size * (distinct as i64 - 1),
            members,
        });
    }

    let mut report = Report {
        groups,
        files_enumerated: stats.files_enumerated,
        files_skipped: stats.files_skipped,
        files_errored: stats.files_errored,
    };
    report.normalize();
    let _ = hasher; // hasher reserved for the pluggable primitive
    Ok((report, stats))
}
