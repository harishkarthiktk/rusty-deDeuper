//! Identity keys for hardlink collapse. `file_id` is unique per volume; these helpers
//! make it unique per run when entries from several devices are mixed.

use crate::traits::FileMeta;

/// Run-local hardlink key for the naive pipeline: `(dev, file_id)`. `None` when the
/// file has no id (callers fall back to a per-row key).
pub fn run_key(meta: &FileMeta) -> Option<Vec<u8>> {
    let fid = meta.file_id.as_ref()?;
    let mut k = Vec::with_capacity(9 + fid.len());
    match meta.dev {
        Some(d) => {
            k.push(1);
            k.extend_from_slice(&d.to_le_bytes());
        }
        None => k.push(0),
    }
    k.extend_from_slice(fid);
    Some(k)
}

/// Stable 8-byte tag for a foreign volume, from its identity string.
pub fn volume_tag(uuid: &str) -> [u8; 8] {
    let h = blake3::hash(uuid.as_bytes());
    let mut t = [0u8; 8];
    t.copy_from_slice(&h.as_bytes()[..8]);
    t
}

/// `tag || native id`: a file_id that cannot collide with ids native to the root volume
/// (those are 8 bytes on Unix) and that open-by-id never accepts.
///
/// Invariant: only Unix walks cross devices (`--cross-mounts`), and Unix ids are 8 bytes,
/// so a 16-byte namespaced id never equals a native one. The Windows walker never leaves
/// its volume.
pub fn namespaced_file_id(tag: [u8; 8], native: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + native.len());
    v.extend_from_slice(&tag);
    v.extend_from_slice(native);
    v
}
