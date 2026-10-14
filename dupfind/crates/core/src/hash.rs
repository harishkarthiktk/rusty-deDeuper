//! BLAKE3-128 wrapper plus the two hashing modes from 4.4a rule 4.

use std::path::Path;

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::model::Hash;
use crate::traits::{ContentReader, Hasher};

/// `meta.hash_algo` value.
pub const HASH_ALGO: &str = "blake3-128";

/// Chunk size for streaming full-file reads.
pub const READ_CHUNK: usize = 64 * 1024;

/// Hash a byte slice to BLAKE3-128.
pub fn hash_bytes(data: &[u8]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(data);
    finish(h)
}

fn finish(h: blake3::Hasher) -> Hash {
    let out = h.finalize();
    let bytes = out.as_bytes();
    let mut a = [0u8; 16];
    a.copy_from_slice(&bytes[..16]);
    Hash(a)
}

/// The default `Hasher`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Blake3Hasher;

impl Hasher for Blake3Hasher {
    fn hash_bytes(&self, data: &[u8]) -> Hash {
        hash_bytes(data)
    }
}

/// Compute the prefix hash per 4.4a rule 4.
///
/// Returns `(hash, prefix_bytes)`. For `size <= F + L` this is the whole file and
/// `prefix_bytes == size`; larger files hash `first F || last L`.
pub fn hash_prefix(
    reader: &dyn ContentReader,
    path: &Path,
    size: u64,
    cfg: &Config,
) -> Result<(Hash, i64)> {
    let total = cfg.prefix_total();
    if size <= total {
        let whole = read_whole(reader, path, size)?;
        return Ok((hash_bytes(&whole), size as i64));
    }
    let first = reader.read_range(path, 0, cfg.prefix_first_bytes as usize)?;
    let last = reader.read_range(
        path,
        size - cfg.prefix_last_bytes,
        cfg.prefix_last_bytes as usize,
    )?;
    let mut h = blake3::Hasher::new();
    h.update(&first);
    h.update(&last);
    Ok((finish(h), total as i64))
}

/// Stream the whole file and hash it.
pub fn hash_full(reader: &dyn ContentReader, path: &Path, size: u64) -> Result<Hash> {
    let mut h = blake3::Hasher::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; READ_CHUNK];
    while offset < size {
        let want = ((size - offset) as usize).min(READ_CHUNK);
        let n = reader.read_at(path, offset, &mut buf[..want])?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        offset += n as u64;
    }
    Ok(finish(h))
}

fn read_whole(reader: &dyn ContentReader, path: &Path, size: u64) -> Result<Vec<u8>> {
    reader.read_range(path, 0, size as usize)
}

/// Verify an expected prefix length is consistent with the config.
pub fn prefix_bytes_matches(prefix_bytes: Option<i64>, cfg: &Config) -> bool {
    match prefix_bytes {
        Some(b) => {
            let b = b as u64;
            // Either the full-file (small) case, or exactly F + L.
            b == cfg.prefix_total() || b <= cfg.prefix_total()
        }
        None => false,
    }
}

/// Guard helper used by resume: string form of the current hash config.
pub fn config_fingerprint(cfg: &Config) -> String {
    format!(
        "{}:first={}:last={}",
        cfg.hash_algo, cfg.prefix_first_bytes, cfg.prefix_last_bytes
    )
}

/// Unused-import guard for `CoreError` in this module's public signatures.
#[allow(dead_code)]
fn _assert_error_type(e: CoreError) -> CoreError {
    e
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{EntryKind, FileMeta};
    use std::path::Path;

    struct Mem(Vec<u8>);
    impl ContentReader for Mem {
        fn read_at(&self, _p: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
            let o = offset as usize;
            if o >= self.0.len() {
                return Ok(0);
            }
            let n = (self.0.len() - o).min(buf.len());
            buf[..n].copy_from_slice(&self.0[o..o + n]);
            Ok(n)
        }
        fn metadata(&self, _p: &Path) -> Result<FileMeta> {
            Ok(FileMeta {
                file_id: None,
                nlink: 1,
                size: self.0.len() as i64,
                mtime_ns: 0,
                ctime_ns: None,
                attrs: 0,
                kind: EntryKind::RegularFile,
            })
        }
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(hash_bytes(b"hello"), hash_bytes(b"hello"));
        assert_ne!(hash_bytes(b"hello"), hash_bytes(b"world"));
    }

    #[test]
    fn small_file_boundaries() {
        let cfg = Config::default();
        let total = cfg.prefix_total();
        for size in [1u64, total, total + 1] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let r = Mem(data);
            let p = Path::new("x");
            let (ph, pb) = hash_prefix(&r, p, size, &cfg).unwrap();
            let fh = hash_full(&r, p, size).unwrap();
            if size <= total {
                assert_eq!(ph, fh, "size {size} should hash whole file");
                assert_eq!(pb, size as i64);
            } else {
                assert_ne!(ph, fh, "size {size} prefix must differ from full");
                assert_eq!(pb, total as i64);
            }
        }
    }
}
