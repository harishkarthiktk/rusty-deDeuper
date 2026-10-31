//! `--exclude` globs and `--exclude-dir` directories. Matching is on the whole path with `/` as
//! the only separator, so one rule works on every platform: `**/node_modules`, `**/*.pdb`,
//! `C:/Users/me/tmp`, or a directory `c:/users/me/tmp` that also drops its whole subtree.
//!
//! Matching ignores case for every rule on every OS. An excluded directory drops everything under
//! it, because its folder is never registered and the pipeline discards files without a known folder.

use std::path::{Path, PathBuf};

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::error::{CoreError, Result};

/// Compiled `--exclude` globs and `--exclude-dir` directories. The default set matches nothing and
/// costs nothing to ask.
#[derive(Clone, Debug, Default)]
pub struct ExcludeSet {
    globs: Option<GlobSet>,
    /// Normalized, lowercased, no trailing `/`.
    dirs: Vec<String>,
}

impl ExcludeSet {
    /// `dirs` must be absolute (the CLI canonicalizes them).
    pub fn new(patterns: &[String], dirs: &[PathBuf]) -> Result<Self> {
        let globs = if patterns.is_empty() {
            None
        } else {
            let mut builder = GlobSetBuilder::new();
            for pattern in patterns {
                // `*` stays within one path component; `**` crosses components.
                let glob = GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .case_insensitive(true)
                    .build()
                    .map_err(|e| {
                        CoreError::Other(format!("bad --exclude pattern {pattern:?}: {e}"))
                    })?;
                builder.add(glob);
            }
            let set = builder
                .build()
                .map_err(|e| CoreError::Other(format!("build --exclude patterns: {e}")))?;
            Some(set)
        };
        let dirs = dirs
            .iter()
            .map(|d| normalize(d).to_lowercase().trim_end_matches('/').to_owned())
            .collect();
        Ok(Self { globs, dirs })
    }

    pub fn is_excluded(&self, path: &Path) -> bool {
        if self.globs.is_none() && self.dirs.is_empty() {
            return false;
        }
        let normalized = normalize(path);
        if self
            .globs
            .as_ref()
            .is_some_and(|set| set.is_match(&normalized))
        {
            return true;
        }
        if self.dirs.is_empty() {
            return false;
        }
        let lower = normalized.to_lowercase();
        self.dirs.iter().any(|dir| under(&lower, dir))
    }
}

/// `path` equals `dir` or lies below it. Whole components only: `/a/build` does not cover `/a/buildx`.
fn under(path: &str, dir: &str) -> bool {
    path == dir
        || path
            .strip_prefix(dir)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// `/` separators, and no `\\?\` before a drive letter, so patterns need no Windows forms.
fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    match text.strip_prefix("//?/") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_excluded_should_match_dir_and_subtree_when_dir_rule_given() {
        let set = ExcludeSet::new(&[], &[PathBuf::from("/a/build")]).expect("dir rule");
        assert!(set.is_excluded(Path::new("/a/build")));
        assert!(set.is_excluded(Path::new("/a/build/x/y.txt")));
        assert!(!set.is_excluded(Path::new("/a/src/main.rs")));
    }

    #[test]
    fn is_excluded_should_not_match_sibling_with_same_prefix() {
        let set = ExcludeSet::new(&[], &[PathBuf::from("/a/build")]).expect("dir rule");
        assert!(!set.is_excluded(Path::new("/a/buildx")));
        assert!(!set.is_excluded(Path::new("/a/buildx/file")));
    }

    #[test]
    fn is_excluded_should_ignore_case_and_verbatim_prefix() {
        let set = ExcludeSet::new(&[], &[PathBuf::from("c:/x/y")]).expect("dir rule");
        assert!(set.is_excluded(Path::new(r"\\?\C:\X\Y\z")));
    }
}
