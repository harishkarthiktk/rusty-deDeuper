//! Directory traversal via `walkdir`. Regular files only at the pipeline level;
//! the walker reports every entry and lets the pipeline apply 4.4a rules.

use std::path::Path;

use crate::codec::GenericPathCodec;
use crate::meta::file_meta_from;
use dedupe_core::config::Config;
use dedupe_core::error::Result;
use dedupe_core::model::ErrorKind;
use dedupe_core::traits::{Enumerator, FileMeta, PathCodec, WalkItem};
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, Default)]
pub struct WalkdirEnumerator;

/// Metadata for a non-directory entry without following links.
///
/// Unix: `lstat` (the same cost as before). Windows: one handle open for a regular file, which
/// yields identity and info together (T8.6). If that open fails, the walker's cached find data
/// is used with `file_id` `None` and `nlink` 1, and non-regular entries use it directly.
#[cfg(not(windows))]
fn file_meta_for(entry: &walkdir::DirEntry, ft: std::fs::FileType) -> std::io::Result<FileMeta> {
    let md = entry.path().symlink_metadata()?;
    Ok(file_meta_from(&md, ft))
}

#[cfg(windows)]
fn file_meta_for(entry: &walkdir::DirEntry, ft: std::fs::FileType) -> std::io::Result<FileMeta> {
    if ft.is_file() {
        if let Some(info) = crate::win_sys::file_info(entry.path()) {
            return Ok(crate::meta::file_meta_from_info(info));
        }
    }
    let md = entry.metadata().map_err(|e| {
        e.into_io_error()
            .unwrap_or_else(|| std::io::Error::other("walk metadata"))
    })?;
    Ok(file_meta_from(&md, ft))
}

fn walk_error(
    codec: &GenericPathCodec,
    path: &Path,
    err: &std::io::Error,
    is_dir: bool,
) -> WalkItem {
    let kind = match err.kind() {
        std::io::ErrorKind::PermissionDenied => ErrorKind::Permission,
        _ => ErrorKind::Io,
    };
    WalkItem::Error {
        path: codec.path_to_raw(path),
        is_dir,
        kind,
        message: err.to_string(),
    }
}

impl Enumerator for WalkdirEnumerator {
    fn walk(
        &self,
        root: &Path,
        cfg: &Config,
        visit: &mut dyn FnMut(WalkItem) -> Result<()>,
    ) -> Result<()> {
        let codec = GenericPathCodec;
        let mut builder = WalkDir::new(root).follow_links(false);
        if cfg.one_file_system {
            builder = builder.same_file_system(true);
        }
        // `contents_first(false)` default yields parents before children, so the
        // pipeline always sees a directory before the files inside it.
        // `filter_entry` prunes an excluded directory before it is read, which is where the
        // time goes for `node_modules` and `target`. The root (depth 0) is never excluded.
        let walker = builder
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !cfg.exclude.is_excluded(e.path()));
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    // walkdir errors carry the path being read (a directory for read_dir
                    // failures). No path (rare) means nothing to attribute: skip.
                    let Some(path) = err.path() else { continue };
                    // Unknown type => treat as a directory so its old rows are kept.
                    let is_dir = path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(true);
                    let io = err
                        .io_error()
                        .map(|e| std::io::Error::new(e.kind(), e.to_string()))
                        .unwrap_or_else(|| std::io::Error::other(err.to_string()));
                    visit(walk_error(&codec, path, &io, is_dir))?;
                    continue;
                }
            };
            let path = entry.path();
            let ft = entry.file_type();
            let raw = codec.path_to_raw(path);
            if ft.is_dir() {
                visit(WalkItem::Dir { path: raw })?;
                continue;
            }
            // Never follow: metadata is read without following links, so a symlink is never opened.
            match file_meta_for(&entry, ft) {
                Ok(meta) => visit(WalkItem::File { path: raw, meta })?,
                Err(e) => visit(walk_error(&codec, path, &e, false))?,
            }
        }
        Ok(())
    }
}
