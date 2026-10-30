//! Storage-kind probe. Linux reads the block-queue `rotational` flag from sysfs.
//! macOS asks `diskutil info -plist` for the `SolidState` flag of the device behind
//! the mount. Windows asks the volume's storage device for its seek-penalty flag.
//! Other targets report unknown. An unknown answer makes the core fall back to SSD
//! policy, so a missing probe never fails a scan.

use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::traits::StorageProbe;

use crate::sys::{Os, Platform};

/// Reports whether the device holding a path is rotational (HDD) or not (SSD).
#[derive(Clone, Copy, Debug, Default)]
pub struct GenericStorageProbe;

impl StorageProbe for GenericStorageProbe {
    fn is_rotational(&self, path: &Path) -> Result<Option<bool>> {
        Os::is_rotational(path)
    }
}

/// Remote-ness of the filesystem holding a path. SQLite WAL needs shared memory and
/// working file locks, which network filesystems do not provide reliably.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsLocality {
    Local,
    /// NFS, SMB/CIFS, FUSE, Ceph, or a Windows network drive.
    Remote,
    /// WSL's 9p/drvfs mounts: local disk behind a network-style protocol. Usually works.
    Virtualized,
    Unknown,
}

/// Classifies the filesystem holding `path` (an existing file or directory). Any failure
/// is `Unknown`, so a missing probe never blocks a scan.
pub fn fs_locality(path: &Path) -> FsLocality {
    Os::fs_locality(path)
}
