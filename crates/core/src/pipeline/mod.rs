//! Pipeline orchestration.
//!
//! Phase 0: [`naive`]. Phase 1 adds the persistent passes, resume and rescan.

pub mod naive;
pub mod pass_enumerate;
pub mod pass_full;
pub mod pass_prefix;
pub mod pass_size;
pub mod pass_verify;
pub mod rescan;
pub mod resume;
pub mod scan;

use std::collections::HashMap;

use rusqlite::Connection;

use crate::cancel::Token;
use crate::config::Config;
use crate::db::ids::IdAllocator;
use crate::db::queries;
use crate::db::writer::{Batch, WriterHandle};
use crate::error::Result;
use crate::path::RawPath;
use crate::traits::{ContentReader, Enumerator, Hasher, PathCodec, VolumeIdentity};

pub use naive::{scan_naive, ScanStats};

/// Borrowed backend services. One bundle threaded through every pass.
pub struct Services<'a> {
    pub enumr: &'a dyn Enumerator,
    pub reader: &'a dyn ContentReader,
    pub codec: &'a dyn PathCodec,
    pub volume: &'a dyn VolumeIdentity,
    pub hasher: &'a dyn Hasher,
    pub cancel: &'a Token,
    pub sink: &'a crate::progress::SharedSink,
}

/// Everything a pass needs: config, the run, the writer, the read connection,
/// the id allocator and the backend services.
pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub run_id: i64,
    pub writer: &'a WriterHandle,
    pub read: &'a Connection,
    pub ids: &'a IdAllocator,
    pub svc: &'a Services<'a>,
}

/// Rows per streamed result chunk. A crash loses at most one writer window plus this
/// many unsent rows (plan: "kill -9 and restart must lose at most one batch window").
pub(crate) const RESULT_CHUNK: usize = 5_000;

/// Send `buf` as one batch once it holds `RESULT_CHUNK` rows (or always, with `force`).
/// Per-id transitions are idempotent and resume selects by state, so a partial commit
/// is safe.
pub(crate) fn send_chunk<T>(
    ctx: &Ctx,
    buf: &mut Vec<T>,
    force: bool,
    wrap: fn(Vec<T>) -> Batch,
) -> Result<()> {
    if !buf.is_empty() && (force || buf.len() >= RESULT_CHUNK) {
        ctx.writer.send(wrap(std::mem::take(buf)))?;
    }
    Ok(())
}

/// One volume and the roots this run scans on it.
#[derive(Clone, Debug)]
pub struct VolumeScan {
    pub volume_id: i64,
    pub mount: RawPath,
    pub roots: Vec<RawPath>,
}

/// Folder-id -> directory path cache. Reconstructing a path is a recursive CTE,
/// so passes keep a per-pass cache instead of re-querying per file.
pub struct PathCache<'a> {
    conn: &'a Connection,
    dirs: HashMap<i64, RawPath>,
}

impl<'a> PathCache<'a> {
    pub fn new(conn: &'a Connection) -> Self {
        PathCache {
            conn,
            dirs: HashMap::new(),
        }
    }

    pub fn dir(&mut self, folder_id: i64) -> Result<&RawPath> {
        if !self.dirs.contains_key(&folder_id) {
            let p = queries::reconstruct_path(self.conn, folder_id)?;
            self.dirs.insert(folder_id, p);
        }
        Ok(self.dirs.get(&folder_id).expect("just inserted"))
    }

    pub fn file_path(&mut self, folder_id: i64, name: &RawPath) -> Result<RawPath> {
        let dir = self.dir(folder_id)?.clone();
        Ok(dir.join_component(name.as_bytes()))
    }
}
