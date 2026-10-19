//! `--exclude` globs. Matching is on the whole path with `/` as the only separator, so one
//! pattern works on every platform: `**/node_modules`, `**/*.pdb`, `C:/Users/me/tmp`.
//!
//! Matching is case-sensitive. An excluded directory drops everything under it, because
//! its folder is never registered and the pipeline discards files without a known folder.

use std::path::Path;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{CoreError, Result};

/// Compiled `--exclude` patterns. The default set matches nothing and costs nothing to ask.
#[derive(Clone, Debug, Default)]
pub struct ExcludeSet(Option<GlobSet>);

impl ExcludeSet {
    pub fn new(patterns: &[String]) -> Result<Self> {
        if patterns.is_empty() {
            return Ok(Self(None));
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in patterns {
            // `*` stays within one path component; `**` crosses components.
            let glob = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| CoreError::Other(format!("bad --exclude pattern {pattern:?}: {e}")))?;
            builder.add(glob);
        }
        let set = builder
            .build()
            .map_err(|e| CoreError::Other(format!("build --exclude patterns: {e}")))?;
        Ok(Self(Some(set)))
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        match &self.0 {
            Some(set) => set.is_match(normalize(path)),
            None => false,
        }
    }
}

/// `/` separators, and no `\\?\` before a drive letter, so patterns need no Windows forms.
fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    match text.strip_prefix("//?/") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => text,
    }
}
