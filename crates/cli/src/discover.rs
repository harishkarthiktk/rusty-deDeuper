//! Find the dedupe database inside a folder when `--db` is not given.

use std::fmt;
use std::path::{Path, PathBuf};

use dedupe_core::db::Db;

/// Why a single database could not be picked from a folder.
#[derive(Debug)]
pub enum DiscoverError {
    /// The folder holds no dedupe database.
    NotFound(PathBuf),
    /// The folder holds more than one dedupe database. Paths are sorted.
    Ambiguous(Vec<PathBuf>),
    /// The folder could not be listed.
    Io(std::io::Error),
}

impl fmt::Display for DiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound(dir) => write!(
                f,
                "no dedupe database in {}; run `dedupe scan <root>` or pass --db <PATH>",
                dir.display()
            ),
            Self::Ambiguous(paths) => {
                let dir = paths
                    .first()
                    .and_then(|p| p.parent())
                    .unwrap_or_else(|| Path::new("."));
                let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(
                    f,
                    "multiple dedupe databases in {}: {}; pass --db <PATH> to choose",
                    dir.display(),
                    names.join(", ")
                )
            }
            Self::Io(err) => write!(f, "list database folder: {err}"),
        }
    }
}

impl std::error::Error for DiscoverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            Self::NotFound(_) | Self::Ambiguous(_) => None,
        }
    }
}

/// Return the single dedupe database directly inside `dir` (not recursive).
pub fn discover_db(dir: &Path) -> Result<PathBuf, DiscoverError> {
    let entries = std::fs::read_dir(dir).map_err(DiscoverError::Io)?;
    let mut found = Vec::new();
    for entry in entries {
        let path = entry.map_err(DiscoverError::Io)?.path();
        let has_db_ext = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("db"));
        if has_db_ext && path.is_file() && is_dedupe_db(&path) {
            found.push(path);
        }
    }
    found.sort();
    match found.len() {
        0 => Err(DiscoverError::NotFound(dir.to_path_buf())),
        1 => Ok(found.swap_remove(0)),
        _ => Err(DiscoverError::Ambiguous(found)),
    }
}

/// True when `path` is a SQLite file with dedupe's schema version and a `meta` table.
/// `Db::open_readonly` already rejects any `user_version` other than the expected one.
/// Any open or query error (including non-SQLite files such as `Thumbs.db`) means false.
fn is_dedupe_db(path: &Path) -> bool {
    let Ok(db) = Db::open_readonly(path) else {
        return false;
    };
    db.conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(false)
}
