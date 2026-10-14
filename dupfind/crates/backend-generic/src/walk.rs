//! Directory traversal via `walkdir`. Regular files only at the pipeline level;
//! the walker reports every entry and lets the pipeline apply 4.4a rules.

use std::path::Path;

use crate::codec::GenericPathCodec;
use crate::meta::file_meta_from;
use dupfind_core::config::Config;
use dupfind_core::error::Result;
use dupfind_core::traits::{Enumerator, PathCodec, WalkItem};
use walkdir::WalkDir;

#[derive(Clone, Copy, Debug, Default)]
pub struct WalkdirEnumerator;

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
        for entry in builder.into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            let ft = entry.file_type();
            let raw = codec.path_to_raw(path);
            if ft.is_dir() {
                visit(WalkItem::Dir { path: raw })?;
                continue;
            }
            // Never follow: use symlink metadata so a symlink is never opened.
            match path.symlink_metadata() {
                Ok(md) => {
                    let meta = file_meta_from(&md, ft);
                    visit(WalkItem::File { path: raw, meta })?;
                }
                Err(_) => continue,
            }
        }
        Ok(())
    }
}
