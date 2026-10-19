//! Inter-process exclusion for read-write commands: an OS advisory lock on
//! `<db>.lock`, released automatically when the process exits or is killed.
//!
//! A sidecar file, not the database itself: on Windows the lock is mandatory, so locking
//! the `.db` would make SQLite's own reads on other handles fail.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// Held for the lifetime of a read-write command. Dropping it releases the lock.
#[must_use = "the lock is released when this guard is dropped"]
pub struct DbLock {
    _file: File,
    locked: bool,
}

impl DbLock {
    /// False only when the filesystem cannot lock and the caller allowed running unlocked.
    pub fn is_locked(&self) -> bool {
        self.locked
    }
}

pub fn lock_path(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// Take the exclusive lock or fail fast with `DbBusy`. Fails with `Unsupported` when the
/// filesystem cannot lock, unless `allow_unlocked` is set.
pub fn acquire(db: &Path) -> Result<DbLock> {
    acquire_with(db, false)
}

/// As `acquire`, but with `allow_unlocked` the command runs without the lock on filesystems
/// that refuse file locking. The lock file is never deleted while a command runs: removing
/// it would let a second process lock a new inode.
pub fn acquire_with(db: &Path, allow_unlocked: bool) -> Result<DbLock> {
    let path = lock_path(db);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(DbLock {
            _file: file,
            locked: true,
        }),
        Err(TryLockError::WouldBlock) => Err(CoreError::DbBusy(path.display().to_string())),
        Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => {
            if !allow_unlocked {
                return Err(CoreError::Unsupported(format!(
                    "{}: file locking is unsupported here; pass --no-lock to run without the database lock",
                    path.display()
                )));
            }
            tracing::warn!(path = %path.display(), "file locking unsupported; continuing unlocked");
            Ok(DbLock {
                _file: file,
                locked: false,
            })
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}
