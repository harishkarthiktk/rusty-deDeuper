//! Per-OS volume identity. See `.overall_plan.md` 4.10.
//!
//! Prefer the filesystem UUID / volume GUID; fall back to an fsid. `UuidKind::None`
//! synthesizes `"none:<fs_type>:<mount>"` and warns (weak resume guard).

use std::path::Path;

use dedupe_core::error::Result;
use dedupe_core::traits::{VolumeIdentity, VolumeInfo};

use crate::sys::{Os, Platform};

#[derive(Clone, Copy, Debug, Default)]
pub struct GenericVolumeIdentity;

impl VolumeIdentity for GenericVolumeIdentity {
    fn identify(&self, path: &Path) -> Result<VolumeInfo> {
        Os::identify(path)
    }
}
