//! Directory traversal via `walkdir`. Regular files only at the pipeline level;
//! the walker reports every entry and lets the pipeline apply 4.4a rules.

use std::path::{Path, PathBuf};

use crate::codec::GenericPathCodec;
use crate::sys::{Os, Platform};
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
fn file_meta_for(entry: &walkdir::DirEntry, ft: std::fs::FileType) -> std::io::Result<FileMeta> {
    Os::walk_file_meta(entry, ft)
}

/// Last directory yielded at each depth. walkdir reports a failed entry inside an open
/// directory without a path, only with its depth; this maps it back to that directory.
#[derive(Default)]
struct DirTrail(Vec<PathBuf>);

impl DirTrail {
    /// Record a yielded directory at `depth` and forget deeper ones.
    fn enter(&mut self, depth: usize, path: &Path) {
        self.0.truncate(depth);
        self.0.push(path.to_path_buf());
    }

    /// Directory whose listing produced an error at `err_depth`, if known.
    fn parent_of(&self, err_depth: usize) -> Option<&Path> {
        err_depth
            .checked_sub(1)
            .and_then(|d| self.0.get(d))
            .map(PathBuf::as_path)
    }
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
        let mut trail = DirTrail::default();
        for entry in walker {
            let entry = match entry {
                Ok(e) => e,
                Err(err) => {
                    let io = err
                        .io_error()
                        .map(|e| std::io::Error::new(e.kind(), e.to_string()))
                        .unwrap_or_else(|| std::io::Error::other(err.to_string()));
                    if let Some(path) = err.path() {
                        // Unknown type => treat as a directory so its old rows are kept.
                        let is_dir = path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(true);
                        visit(walk_error(&codec, path, &io, is_dir))?;
                    } else if let Some(dir) = trail.parent_of(err.depth()) {
                        // A failed entry inside an open listing: attribute it to that directory
                        // so core protects the subtree's old rows.
                        visit(walk_error(&codec, dir, &io, true))?;
                    } else {
                        tracing::warn!(
                            depth = err.depth(),
                            "walk error with no path and no known parent"
                        );
                    }
                    continue;
                }
            };
            let path = entry.path();
            let ft = entry.file_type();
            let raw = codec.path_to_raw(path);
            if ft.is_dir() {
                trail.enter(entry.depth(), path);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_of_should_return_dir_one_level_up_when_error_depth_given() {
        let mut t = DirTrail::default();
        t.enter(0, Path::new("/r"));
        t.enter(1, Path::new("/r/a"));
        assert_eq!(t.parent_of(2), Some(Path::new("/r/a")));
        assert_eq!(t.parent_of(1), Some(Path::new("/r")));
    }

    #[test]
    fn enter_should_forget_deeper_dirs_when_sibling_entered() {
        let mut t = DirTrail::default();
        t.enter(0, Path::new("/r"));
        t.enter(1, Path::new("/r/a"));
        t.enter(2, Path::new("/r/a/b"));
        t.enter(1, Path::new("/r/c"));
        assert_eq!(t.parent_of(3), None);
        assert_eq!(t.parent_of(2), Some(Path::new("/r/c")));
        assert_eq!(t.parent_of(0), None);
    }
}
