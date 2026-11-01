//! Path masking for CLI output: vault location and Masker setup shared by scan, report and lookup.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use dedupe_core::db::queries;
use dedupe_core::mask::Masker;
use rusqlite::Connection;

use crate::platform;

pub use dedupe_core::mask::VAULT_NAME;

/// Vault next to the database file.
pub fn vault_path(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(VAULT_NAME)
}

/// The DB uuid as stored in `meta`, as bytes. Shared by export and unmask so they compare equal.
pub fn db_uuid(conn: &Connection) -> Result<Vec<u8>> {
    queries::meta_get(conn, "db_uuid")
        .context("read db_uuid")?
        .map(String::into_bytes)
        .context("database has no db_uuid; scan it again")
}

/// A masker and its vault connection, or `None` with `--unmasked`.
pub fn masker_for(db_path: &Path, unmasked: bool) -> Result<Option<(Masker, Connection)>> {
    if unmasked {
        return Ok(None);
    }
    let db = dedupe_core::db::Db::open_readonly(db_path).context("open database (read-only)")?;
    open_vault(&vault_path(db_path), &db_uuid(&db.conn)?).map(Some)
}

/// `scan --no-db`: vault under `root`, with a fixed uuid that no DB can match at unmask time.
pub fn masker_no_db(root: &Path, unmasked: bool) -> Result<Option<(Masker, Connection)>> {
    if unmasked {
        return Ok(None);
    }
    open_vault(&root.join(VAULT_NAME), b"no-db").map(Some)
}

fn open_vault(path: &Path, uuid: &[u8]) -> Result<(Masker, Connection)> {
    let out =
        Masker::begin(path, uuid).with_context(|| format!("open mask vault {}", path.display()))?;
    platform::restrict_perms(path).context("restrict mask vault permissions")?;
    Ok(out)
}
