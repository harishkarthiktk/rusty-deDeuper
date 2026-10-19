//! BLAKE3-128 wrapper plus the two hashing modes from 4.4a rule 4.

use std::path::Path;

use crate::config::Config;
use crate::error::{CoreError, Result};
use crate::model::Hash;
use crate::traits::{AccessPattern, ContentReader, FileHandle, Hasher, ReadTarget};

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
    // Path-only callers have no stored id. The prefix reads two separated ranges,
    // so Random is the honest hint.
    hash_prefix_target(
        reader,
        ReadTarget {
            path,
            file_id: None,
            access: AccessPattern::Random,
        },
        size,
        cfg,
    )
}

/// [`hash_prefix`] over a [`ReadTarget`], so a backend can open by file id.
pub fn hash_prefix_target(
    reader: &dyn ContentReader,
    target: ReadTarget<'_>,
    size: u64,
    cfg: &Config,
) -> Result<(Hash, i64)> {
    // Check every length before opening: a config that does not fit must fail
    // without touching the file, and an `as` cast would wrap it silently.
    let total = cfg.prefix_total();
    let total_i64 = checked_i64(total, "prefix total")?;
    let first_len = checked_usize(cfg.prefix_first_bytes, "prefix_first_bytes")?;
    let last_len = checked_usize(cfg.prefix_last_bytes, "prefix_last_bytes")?;
    // One open serves both ranges; on a backend that opens per read this was two opens.
    let mut handle = reader.open_target(target)?;
    if size <= total {
        let whole_len = checked_usize(size, "file size")?;
        let whole = read_span(handle.as_mut(), 0, whole_len)?;
        return Ok((hash_bytes(&whole), checked_i64(size, "file size")?));
    }
    let first = read_span(handle.as_mut(), 0, first_len)?;
    let last = read_span(handle.as_mut(), size - cfg.prefix_last_bytes, last_len)?;
    let mut h = blake3::Hasher::new();
    h.update(&first);
    h.update(&last);
    Ok((finish(h), total_i64))
}

fn checked_usize(value: u64, what: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| CoreError::InvalidArg(format!("{what} {value} does not fit in usize")))
}

fn checked_i64(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| CoreError::InvalidArg(format!("{what} {value} exceeds i64::MAX")))
}

/// Stream the whole file and hash it, in `READ_CHUNK` reads.
pub fn hash_full(reader: &dyn ContentReader, path: &Path, size: u64) -> Result<Hash> {
    hash_full_with(reader, path, size, READ_CHUNK)
}

/// Stream the whole file in `chunk`-byte reads over one handle.
///
/// Stops early on a zero-byte read, so a file that shrank hashes what it has.
/// Verify must not use this path: it needs to see the EOF probe itself.
pub fn hash_full_with(
    reader: &dyn ContentReader,
    path: &Path,
    size: u64,
    chunk: usize,
) -> Result<Hash> {
    // Path-only callers have no stored id, and a whole-file stream is sequential.
    hash_full_target(
        reader,
        ReadTarget {
            path,
            file_id: None,
            access: AccessPattern::Sequential,
        },
        size,
        chunk,
    )
}

/// [`hash_full_with`] over a [`ReadTarget`], so a backend can open by file id.
pub fn hash_full_target(
    reader: &dyn ContentReader,
    target: ReadTarget<'_>,
    size: u64,
    chunk: usize,
) -> Result<Hash> {
    // A zero chunk would read nothing and hash an empty stream without any error.
    let chunk = chunk.max(1);
    let mut handle = reader.open_target(target)?;
    let mut h = blake3::Hasher::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; chunk];
    while offset < size {
        // A remainder beyond `usize::MAX` is necessarily larger than `chunk`.
        let want = usize::try_from(size - offset).map_or(chunk, |w| w.min(chunk));
        let n = handle.read_at(offset, &mut buf[..want])?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        offset += n as u64;
    }
    Ok(finish(h))
}

/// Fills `buf` from `offset`, retrying short reads. Stops early only at EOF (a
/// `read_at` returning 0), so the result is below `buf.len()` only at true EOF.
pub(crate) fn read_full(handle: &mut dyn FileHandle, offset: u64, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let n = handle.read_at(offset + filled as u64, &mut buf[filled..])?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Reads up to `len` bytes at `offset` over an open handle. A short filesystem
/// read must not shorten a prefix hash, so this loops via [`read_full`].
fn read_span(handle: &mut dyn FileHandle, offset: u64, len: usize) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let n = read_full(handle, offset, &mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Verify an expected prefix length is consistent with the config.
pub fn prefix_bytes_matches(prefix_bytes: Option<i64>, cfg: &Config) -> bool {
    match prefix_bytes {
        // A negative stored length never matches; `as u64` would wrap it to a huge value.
        // Either the full-file (small) case, or exactly F + L.
        Some(b) => u64::try_from(b).is_ok_and(|b| b <= cfg.prefix_total()),
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
    use std::sync::Mutex;

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
                dev: None,
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

    #[test]
    fn hash_prefix_should_return_invalid_arg_when_prefix_total_exceeds_i64() {
        // F + L = 2^63 fits in u64 but not i64; the old `as i64` would wrap it negative.
        let cfg = Config {
            prefix_first_bytes: 1 << 62,
            prefix_last_bytes: 1 << 62,
            ..Config::default()
        };
        let reader = Mem(vec![0u8; 16]);
        let got = hash_prefix(&reader, Path::new("x"), 16, &cfg);
        assert!(
            matches!(got, Err(CoreError::InvalidArg(ref msg)) if msg.contains("prefix total")),
            "a prefix total of 2^63 should be InvalidArg naming the total, got {got:?}"
        );
    }

    /// Largest read `ShortRead` returns. Not a divisor of the buffer sizes used
    /// below, so each read loop ends with a partial read.
    const MAX_READ: usize = 7;

    /// `Mem` that returns at most `MAX_READ` bytes per `read_at`, the way a
    /// filesystem can under pressure. It relies on the default `open`, so every
    /// positional read reaches `read_at` and hits the cap.
    struct ShortRead(Mem);

    impl ContentReader for ShortRead {
        fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
            let cap = buf.len().min(MAX_READ);
            self.0.read_at(path, offset, &mut buf[..cap])
        }

        fn metadata(&self, path: &Path) -> Result<FileMeta> {
            self.0.metadata(path)
        }
    }

    #[test]
    fn read_full_should_fill_buffer_across_short_reads() {
        let data: Vec<u8> = (0..50u8).collect();
        let reader = ShortRead(Mem(data.clone()));
        let mut handle = reader
            .open(Path::new("x"))
            .expect("opening a short-read double should succeed");
        let mut buf = [0u8; 20];
        let filled = read_full(handle.as_mut(), 0, &mut buf)
            .expect("read_full over short reads should succeed");
        assert_eq!(
            (filled, buf.to_vec()),
            (20, data[..20].to_vec()),
            "read_full should fill all 20 bytes across {MAX_READ}-byte reads, got {filled} bytes"
        );
    }

    #[test]
    fn read_full_should_stop_at_end_of_file() {
        let reader = ShortRead(Mem(vec![0xab; 10]));
        let mut handle = reader
            .open(Path::new("x"))
            .expect("opening a short-read double should succeed");
        let mut buf = [0u8; 32];
        let filled =
            read_full(handle.as_mut(), 0, &mut buf).expect("read_full past EOF should succeed");
        assert_eq!(
            filled, 10,
            "read_full of a 10-byte file into a 32-byte buffer should stop at EOF, got {filled}"
        );
    }

    #[test]
    fn hash_prefix_should_match_full_reads_when_reader_returns_short_reads() {
        let cfg = Config::default();
        // Larger than first + last, so both prefix ranges are read.
        let size = cfg.prefix_total() + 4096;
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let p = Path::new("x");
        let plain = hash_prefix(&Mem(data.clone()), p, size, &cfg)
            .expect("prefix hash over full reads should succeed");
        let short = hash_prefix(&ShortRead(Mem(data)), p, size, &cfg)
            .expect("prefix hash over short reads should succeed");
        assert_eq!(
            short, plain,
            "prefix over {MAX_READ}-byte reads should equal full reads for a {size}-byte file"
        );
    }

    /// `Mem` that records the `file_id` and `access` it receives in `open_target`,
    /// then forwards to `Mem` so the hash still reads real bytes.
    struct Recording {
        inner: Mem,
        seen: Mutex<Option<(Option<Vec<u8>>, AccessPattern)>>,
    }

    impl Recording {
        fn new(data: Vec<u8>) -> Self {
            Recording {
                inner: Mem(data),
                seen: Mutex::new(None),
            }
        }

        fn recorded(&self) -> Option<(Option<Vec<u8>>, AccessPattern)> {
            self.seen
                .lock()
                .expect("recording lock should not be poisoned")
                .clone()
        }
    }

    impl ContentReader for Recording {
        fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
            self.inner.read_at(path, offset, buf)
        }

        fn metadata(&self, path: &Path) -> Result<FileMeta> {
            self.inner.metadata(path)
        }

        fn open_target<'a>(&'a self, t: ReadTarget<'a>) -> Result<Box<dyn FileHandle + 'a>> {
            *self
                .seen
                .lock()
                .expect("recording lock should not be poisoned") =
                Some((t.file_id.map(<[u8]>::to_vec), t.access));
            self.inner.open_target(t)
        }
    }

    #[test]
    fn hash_full_target_should_equal_hash_full_with_when_file_id_is_none() {
        let reader = Mem((0..37u8).collect());
        let path = Path::new("x");
        let target = ReadTarget {
            path,
            file_id: None,
            access: AccessPattern::Sequential,
        };
        let via_target = hash_full_target(&reader, target, 37, 8)
            .expect("hash_full_target over an in-memory reader should succeed");
        let via_with = hash_full_with(&reader, path, 37, 8)
            .expect("hash_full_with over an in-memory reader should succeed");
        assert_eq!(
            via_target, via_with,
            "hash_full_target gave {via_target:?}, hash_full_with gave {via_with:?}"
        );
    }

    #[test]
    fn hash_prefix_target_should_equal_hash_prefix() {
        let cfg = Config::default();
        let size = cfg.prefix_total() + 4096;
        let reader = Mem((0..size).map(|i| (i % 251) as u8).collect());
        let path = Path::new("x");
        let target = ReadTarget {
            path,
            file_id: None,
            access: AccessPattern::Random,
        };
        let via_target = hash_prefix_target(&reader, target, size, &cfg)
            .expect("hash_prefix_target over an in-memory reader should succeed");
        let via_prefix = hash_prefix(&reader, path, size, &cfg)
            .expect("hash_prefix over an in-memory reader should succeed");
        assert_eq!(
            via_target, via_prefix,
            "hash_prefix_target gave {via_target:?}, hash_prefix gave {via_prefix:?}"
        );
    }

    #[test]
    fn hash_full_target_should_pass_file_id_and_sequential_access_to_reader() {
        let reader = Recording::new(vec![0x5a; 16]);
        let id = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let target = ReadTarget {
            path: Path::new("x"),
            file_id: Some(id.as_slice()),
            access: AccessPattern::Sequential,
        };
        hash_full_target(&reader, target, 16, 4)
            .expect("hash_full_target over a recording reader should succeed");
        assert_eq!(
            reader.recorded(),
            Some((Some(id.to_vec()), AccessPattern::Sequential)),
            "reader should receive the file id and Sequential access"
        );
    }

    #[test]
    fn hash_prefix_target_should_request_random_access() {
        let cfg = Config::default();
        let reader = Recording::new(vec![0u8; 100]);
        let target = ReadTarget {
            path: Path::new("x"),
            file_id: None,
            access: AccessPattern::Random,
        };
        hash_prefix_target(&reader, target, 100, &cfg)
            .expect("hash_prefix_target over a recording reader should succeed");
        assert_eq!(
            reader.recorded().map(|(_, access)| access),
            Some(AccessPattern::Random),
            "hash_prefix_target should open with Random access"
        );
    }

    mod hash_full_with {
        use super::*;
        use crate::error::Result;
        use crate::traits::{ContentReader, FileHandle, FileMeta};
        use std::path::Path;
        use std::sync::atomic::{AtomicU64, Ordering};

        /// Small enough that the sizes below span several reads.
        const CHUNK: usize = 4;

        /// `Mem` with a count of `open` calls. Reads are not counted, so the count
        /// measures only how often `hash_full_with` opens the file.
        struct CountingMem {
            inner: Mem,
            opens: AtomicU64,
        }

        impl ContentReader for CountingMem {
            fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
                self.inner.read_at(path, offset, buf)
            }

            fn metadata(&self, path: &Path) -> Result<FileMeta> {
                self.inner.metadata(path)
            }

            fn opens(&self) -> u64 {
                self.opens.load(Ordering::Relaxed)
            }

            fn open<'a>(&'a self, path: &'a Path) -> Result<Box<dyn FileHandle + 'a>> {
                self.opens.fetch_add(1, Ordering::Relaxed);
                self.inner.open(path)
            }
        }

        #[test]
        fn hash_full_with_should_equal_hash_full_when_size_is_zero() {
            let reader = Mem(Vec::new());
            let chunked = hash_full_with(&reader, Path::new("x"), 0, CHUNK)
                .expect("chunked hash of an empty file should succeed");
            let full = hash_full(&reader, Path::new("x"), 0)
                .expect("full hash of an empty file should succeed");
            assert_eq!(
                chunked, full,
                "chunk {CHUNK} gave {chunked:?}, hash_full gave {full:?}"
            );
        }

        #[test]
        fn hash_full_with_should_equal_hash_full_when_size_is_one() {
            let reader = Mem(vec![0x11]);
            let chunked = hash_full_with(&reader, Path::new("x"), 1, CHUNK)
                .expect("chunked hash of a 1-byte file should succeed");
            let full = hash_full(&reader, Path::new("x"), 1)
                .expect("full hash of a 1-byte file should succeed");
            assert_eq!(
                chunked, full,
                "chunk {CHUNK} gave {chunked:?}, hash_full gave {full:?}"
            );
        }

        #[test]
        fn hash_full_with_should_equal_hash_full_when_size_equals_chunk() {
            let reader = Mem(vec![0x22; CHUNK]);
            let size = CHUNK as u64;
            let chunked = hash_full_with(&reader, Path::new("x"), size, CHUNK)
                .expect("chunked hash of a chunk-sized file should succeed");
            let full = hash_full(&reader, Path::new("x"), size)
                .expect("full hash of a chunk-sized file should succeed");
            assert_eq!(
                chunked, full,
                "chunk {CHUNK} gave {chunked:?}, hash_full gave {full:?}"
            );
        }

        #[test]
        fn hash_full_with_should_equal_hash_full_when_size_is_chunk_plus_one() {
            let reader = Mem(vec![0x33; CHUNK + 1]);
            let size = (CHUNK + 1) as u64;
            let chunked = hash_full_with(&reader, Path::new("x"), size, CHUNK)
                .expect("chunked hash of a chunk+1 file should succeed");
            let full = hash_full(&reader, Path::new("x"), size)
                .expect("full hash of a chunk+1 file should succeed");
            assert_eq!(
                chunked, full,
                "chunk {CHUNK} gave {chunked:?}, hash_full gave {full:?}"
            );
        }

        #[test]
        fn hash_full_with_should_open_once_when_file_spans_many_chunks() {
            let reader = CountingMem {
                inner: Mem(vec![0x44; 1000]),
                opens: AtomicU64::new(0),
            };
            hash_full_with(&reader, Path::new("x"), 1000, CHUNK)
                .expect("hashing a 1000-byte file should succeed");
            assert_eq!(
                reader.opens(),
                1,
                "a 1000-byte file read in {CHUNK}-byte chunks should open once, got {}",
                reader.opens()
            );
        }

        #[test]
        fn hash_full_with_should_treat_zero_chunk_as_one() {
            let reader = Mem(b"hello".to_vec());
            let got = hash_full_with(&reader, Path::new("x"), 5, 0)
                .expect("hashing with a zero chunk should succeed");
            assert_eq!(
                got,
                hash_bytes(b"hello"),
                "a zero chunk should still hash the whole 5-byte file, got {got:?}"
            );
        }
    }
}
