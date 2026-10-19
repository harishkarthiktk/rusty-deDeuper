//! On-disk cache of a volume's `RecordFacts`, refreshed from the USN journal.
//!
//! Portable: the file format, validation and directory resolution have no Win32 types and are
//! tested on every OS. The cache is written and read only by the Windows enumerator.
//!
//! File layout (all integers little-endian): a header, the record bodies, then a 32-byte BLAKE3
//! hash of everything before it. [`decode`] checks that hash before it parses anything, so a
//! damaged file is rejected without reading a single field.

use std::collections::BTreeSet;
use std::error::Error;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::mft_walk::RecordSource;
use crate::records::RecordError;
use crate::tree::{BudgetExceeded, NameFact, NameSpace, RecordFacts, VolumeTree};
use crate::usn::UsnEntry;

const MAGIC: &[u8; 8] = b"DDMFTC01";
/// Version 2: `ctime_ft` holds the `$STANDARD_INFORMATION` change time (was creation time in v1).
/// Caches written by v1 hold the wrong instant, so `decode` rejects them and the scan falls back
/// to a full enumeration.
const FORMAT_VERSION: u32 = 2;
/// magic, version, then seven 8-byte fields: serial, bytes/record, total bytes, MFT valid length,
/// journal id, next USN, record count.
const HEADER_LEN: usize = 8 + 4 + 8 * 7;
const TRAILER_LEN: usize = 32;
/// Smallest encoded record: record, sequence, flags, attrs, reparse flag, mtime, ctime, size, count.
const MIN_RECORD_BYTES: usize = 8 + 2 + 1 + 4 + 1 + 8 + 8 + 8 + 2;
/// Smallest encoded name: parent, namespace, name length. A name may have zero units.
const MIN_NAME_BYTES: usize = 8 + 1 + 2;
const FLAG_IN_USE: u8 = 0b01;
const FLAG_IS_DIR: u8 = 0b10;
const FLAG_MASK: u8 = FLAG_IN_USE | FLAG_IS_DIR;
/// Reason given when the record iterator does not match the header count.
const COUNT_CHANGED: &str = "record count changed while writing";

/// The volume facts that must match the live volume before a cache is trusted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeStamp {
    pub volume_serial: u64,
    pub bytes_per_record: u64,
    pub total_bytes: u64,
    pub mft_valid_length: u64,
}

/// The USN journal position the cached records are current as of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalStamp {
    pub journal_id: u64,
    pub next_usn: i64,
}

/// A whole-volume record set plus the stamps that say when it was true.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CachedIndex {
    pub volume: VolumeStamp,
    pub journal: JournalStamp,
    pub records: Vec<RecordFacts>,
}

/// Why a cache file could not be read. Every variant means "do a full walk instead".
#[derive(Debug)]
pub enum CacheError {
    Io(io::Error),
    BadMagic,
    BadVersion(u32),
    Truncated,
    BadChecksum,
    Malformed(&'static str),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "MFT cache I/O error: {err}"),
            Self::BadMagic => write!(f, "MFT cache has the wrong file magic"),
            Self::BadVersion(version) => {
                write!(f, "MFT cache format version {version} is not supported")
            }
            Self::Truncated => write!(f, "MFT cache is truncated"),
            Self::BadChecksum => write!(f, "MFT cache checksum does not match its contents"),
            Self::Malformed(why) => write!(f, "MFT cache is malformed: {why}"),
        }
    }
}

impl Error for CacheError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for CacheError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// The cache file for one volume, named by its serial in hex.
pub fn cache_path(dir: &Path, volume_serial: u64) -> PathBuf {
    dir.join(format!("{volume_serial:016x}.cache"))
}

/// Serializes `index` into the cache format, trailer included.
pub fn encode(index: &CachedIndex) -> Vec<u8> {
    let mut out = Vec::new();
    // Writing to a `Vec` cannot fail, and the count is taken from the same slice, so it matches.
    write_index(
        &mut out,
        &index.volume,
        &index.journal,
        index.records.len() as u64,
        index.records.iter(),
    )
    .expect("writing to memory with a matching record count cannot fail");
    let hash = blake3::hash(&out);
    out.extend_from_slice(hash.as_bytes());
    out
}

/// Writes the header and every record. `count` goes into the header, so `records` must yield
/// exactly `count` items. Any other number fails, and the caller discards what was written.
fn write_index<'a, W: Write>(
    w: &mut W,
    volume: &VolumeStamp,
    journal: &JournalStamp,
    count: u64,
    records: impl Iterator<Item = &'a RecordFacts>,
) -> Result<(), CacheError> {
    w.write_all(MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    for field in [
        volume.volume_serial,
        volume.bytes_per_record,
        volume.total_bytes,
        volume.mft_valid_length,
        journal.journal_id,
    ] {
        w.write_all(&field.to_le_bytes())?;
    }
    w.write_all(&journal.next_usn.to_le_bytes())?;
    w.write_all(&count.to_le_bytes())?;
    let mut written = 0u64;
    for facts in records {
        if written == count {
            return Err(CacheError::Malformed(COUNT_CHANGED));
        }
        encode_record(w, facts)?;
        written += 1;
    }
    if written != count {
        return Err(CacheError::Malformed(COUNT_CHANGED));
    }
    Ok(())
}

fn encode_record<W: Write>(w: &mut W, facts: &RecordFacts) -> io::Result<()> {
    w.write_all(&facts.record.to_le_bytes())?;
    w.write_all(&facts.sequence.to_le_bytes())?;
    w.write_all(&[u8::from(facts.in_use) | (u8::from(facts.is_dir) << 1)])?;
    w.write_all(&facts.attrs.to_le_bytes())?;
    match facts.reparse_tag {
        Some(tag) => {
            w.write_all(&[1])?;
            w.write_all(&tag.to_le_bytes())?;
        }
        None => w.write_all(&[0])?,
    }
    w.write_all(&facts.mtime_ft.to_le_bytes())?;
    w.write_all(&facts.ctime_ft.to_le_bytes())?;
    w.write_all(&facts.size.to_le_bytes())?;
    // A file record holds far fewer than 65536 names, so the narrowing cannot truncate.
    w.write_all(&(facts.names.len() as u16).to_le_bytes())?;
    for name in &facts.names {
        w.write_all(&name.parent.to_le_bytes())?;
        w.write_all(&[namespace_byte(name.namespace)])?;
        // NTFS caps a file name at 255 UTF-16 units, so the narrowing cannot truncate.
        w.write_all(&(name.name.len() as u16).to_le_bytes())?;
        for unit in &name.name {
            w.write_all(&unit.to_le_bytes())?;
        }
    }
    Ok(())
}

/// Forwards writes to `inner` and feeds every byte written through it into a BLAKE3 hasher.
struct HashingWriter<W> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
        }
    }

    /// The inner writer, and the hash of every byte written so far.
    fn finish(self) -> (W, blake3::Hash) {
        (self.inner, self.hasher.finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

fn namespace_byte(namespace: NameSpace) -> u8 {
    match namespace {
        NameSpace::Posix => 0,
        NameSpace::Win32 => 1,
        NameSpace::Dos => 2,
        NameSpace::Win32AndDos => 3,
    }
}

fn namespace_from_byte(byte: u8) -> Result<NameSpace, CacheError> {
    match byte {
        0 => Ok(NameSpace::Posix),
        1 => Ok(NameSpace::Win32),
        2 => Ok(NameSpace::Dos),
        3 => Ok(NameSpace::Win32AndDos),
        _ => Err(CacheError::Malformed("unknown name namespace")),
    }
}

/// Parses a cache file. The trailer hash is verified before any field is read.
pub fn decode(bytes: &[u8]) -> Result<CachedIndex, CacheError> {
    if bytes.len() < HEADER_LEN + TRAILER_LEN {
        return Err(CacheError::Truncated);
    }
    let (body, trailer) = bytes.split_at(bytes.len() - TRAILER_LEN);
    if blake3::hash(body).as_bytes().as_slice() != trailer {
        return Err(CacheError::BadChecksum);
    }

    let mut cur = Cursor::new(body);
    if cur.take(MAGIC.len())? != MAGIC.as_slice() {
        return Err(CacheError::BadMagic);
    }
    let version = cur.u32()?;
    if version != FORMAT_VERSION {
        return Err(CacheError::BadVersion(version));
    }
    let volume = VolumeStamp {
        volume_serial: cur.u64()?,
        bytes_per_record: cur.u64()?,
        total_bytes: cur.u64()?,
        mft_valid_length: cur.u64()?,
    };
    let journal = JournalStamp {
        journal_id: cur.u64()?,
        next_usn: i64::from_le_bytes(cur.array()?),
    };
    let count = cur.u64()?;

    // An untrusted count cannot reserve more slots than the remaining bytes could hold.
    let capacity = usize::try_from(count)
        .unwrap_or(usize::MAX)
        .min(cur.remaining() / MIN_RECORD_BYTES);
    let mut records = Vec::with_capacity(capacity);
    for _ in 0..count {
        records.push(decode_record(&mut cur)?);
    }
    if cur.remaining() != 0 {
        return Err(CacheError::Malformed(
            "trailing bytes after the last record",
        ));
    }
    Ok(CachedIndex {
        volume,
        journal,
        records,
    })
}

fn decode_record(cur: &mut Cursor<'_>) -> Result<RecordFacts, CacheError> {
    let record = cur.u64()?;
    let sequence = cur.u16()?;
    let flags = cur.u8()?;
    if flags & !FLAG_MASK != 0 {
        return Err(CacheError::Malformed("unknown record flags"));
    }
    let attrs = cur.u32()?;
    let reparse_tag = match cur.u8()? {
        0 => None,
        1 => Some(cur.u32()?),
        _ => return Err(CacheError::Malformed("reparse flag is not 0 or 1")),
    };
    let mtime_ft = cur.u64()?;
    let ctime_ft = cur.u64()?;
    let size = cur.u64()?;
    let name_count = usize::from(cur.u16()?);
    let mut names = Vec::with_capacity(name_count.min(cur.remaining() / MIN_NAME_BYTES));
    for _ in 0..name_count {
        names.push(decode_name(cur)?);
    }
    Ok(RecordFacts {
        record,
        sequence,
        in_use: flags & FLAG_IN_USE != 0,
        is_dir: flags & FLAG_IS_DIR != 0,
        attrs,
        reparse_tag,
        mtime_ft,
        ctime_ft,
        size,
        names,
    })
}

fn decode_name(cur: &mut Cursor<'_>) -> Result<NameFact, CacheError> {
    let parent = cur.u64()?;
    let namespace = namespace_from_byte(cur.u8()?)?;
    let unit_count = usize::from(cur.u16()?);
    let mut name = Vec::with_capacity(unit_count.min(cur.remaining() / 2));
    for _ in 0..unit_count {
        name.push(cur.u16()?);
    }
    Ok(NameFact {
        parent,
        name,
        namespace,
    })
}

/// Bounds-checked little-endian reader. Every read fails with `Truncated` instead of panicking.
struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], CacheError> {
        let end = self.pos.checked_add(len).ok_or(CacheError::Truncated)?;
        let bytes = self.buf.get(self.pos..end).ok_or(CacheError::Truncated)?;
        self.pos = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], CacheError> {
        self.take(N)?.try_into().map_err(|_| CacheError::Truncated)
    }

    fn u8(&mut self) -> Result<u8, CacheError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, CacheError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, CacheError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, CacheError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

/// Writes the cache to `path` through a sibling `<name>.tmp` file and an atomic rename.
///
/// The temp file is removed when any step fails, so a failed write leaves the previous cache
/// (if any) untouched.
pub fn write_atomic(path: &Path, index: &CachedIndex) -> Result<(), CacheError> {
    write_atomic_stream(path, &index.volume, &index.journal, index.records.iter())
}

/// Like [`write_atomic`], but streams the records to disk instead of building the whole file in
/// memory. The bytes written are identical to [`encode`] for the same records.
///
/// `records.len()` sets the header count, so the iterator must yield exactly that many items. Any
/// other number fails with `Malformed`, and neither the target nor the temp file is left behind.
pub fn write_atomic_stream<'a, I>(
    path: &Path,
    volume: &VolumeStamp,
    journal: &JournalStamp,
    records: I,
) -> Result<(), CacheError>
where
    I: ExactSizeIterator<Item = &'a RecordFacts>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = temp_path(path);
    if let Err(err) = write_stream_then_rename(&tmp, path, volume, journal, records) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// The sibling `<name>.tmp` file that [`write_atomic_stream`] writes before the rename.
fn temp_path(path: &Path) -> PathBuf {
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    PathBuf::from(tmp_name)
}

fn write_stream_then_rename<'a, I>(
    tmp: &Path,
    path: &Path,
    volume: &VolumeStamp,
    journal: &JournalStamp,
    records: I,
) -> Result<(), CacheError>
where
    I: ExactSizeIterator<Item = &'a RecordFacts>,
{
    let file = fs::File::create(tmp)?;
    let mut hashing = HashingWriter::new(BufWriter::new(file));
    write_index(&mut hashing, volume, journal, records.len() as u64, records)?;
    let (mut out, hash) = hashing.finish();
    // The trailer is written past the hasher, so it is not part of its own checksum.
    out.write_all(hash.as_bytes())?;
    out.flush()?;
    let file = out.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    drop(file);
    // On Windows `rename` replaces an existing target, which is what the atomic swap needs.
    fs::rename(tmp, path)?;
    Ok(())
}

/// Reads and decodes the cache at `path`.
pub fn read(path: &Path) -> Result<CachedIndex, CacheError> {
    let bytes = fs::read(path)?;
    decode(&bytes)
}

/// Why a cached index cannot be used, or `None` when it is consistent with the live volume.
///
/// Returns a human-readable reason for the first check that fails. The MFT length rule is
/// one-way: the MFT may grow between scans, but shrinking means the cached records are stale.
pub fn validate(
    cached: &CachedIndex,
    live_volume: &VolumeStamp,
    journal_id: u64,
    lowest_valid_usn: i64,
    next_usn_now: i64,
) -> Option<&'static str> {
    let cached_volume = &cached.volume;
    if cached_volume.volume_serial != live_volume.volume_serial
        || cached_volume.bytes_per_record != live_volume.bytes_per_record
        || cached_volume.total_bytes != live_volume.total_bytes
    {
        return Some("volume identity or geometry changed");
    }
    if live_volume.mft_valid_length < cached_volume.mft_valid_length {
        return Some("MFT shrank");
    }
    if cached.journal.journal_id != journal_id {
        return Some("USN journal was recreated");
    }
    if cached.journal.next_usn < lowest_valid_usn {
        return Some("USN journal was truncated past the cache");
    }
    if cached.journal.next_usn > next_usn_now {
        return Some("USN position is ahead of the journal");
    }
    None
}

/// Resolves the cache directory from an environment lookup.
///
/// `DEDUPE_CACHE_DIR` wins when set and non-empty. Otherwise the directory is
/// `%LOCALAPPDATA%\dedupe\mft`. `None` means no cache location is available.
pub fn cache_dir(env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    if let Some(dir) = non_empty(env("DEDUPE_CACHE_DIR")) {
        return Some(PathBuf::from(dir));
    }
    let local = non_empty(env("LOCALAPPDATA"))?;
    Some(PathBuf::from(local).join("dedupe").join("mft"))
}

fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

/// First record past the NTFS metafile range (0..=23). `tree.rs` keeps the same value as a
/// private constant for its metafile filter, so keep the two in sync.
const FIRST_USER_RECORD: u64 = 24;
/// The NTFS root directory. It is the one reserved record that is kept, as in `tree.rs`.
const ROOT_DIR_RECORD: u64 = 5;

/// Counters for one applied delta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaStats {
    /// Journal entries received.
    pub entries: usize,
    /// Distinct records re-read from the source.
    pub reread: usize,
}

/// Why a delta could not be applied. Every variant means "do a full walk instead".
#[derive(Debug)]
pub enum DeltaError {
    /// The journal touched more distinct records than the limit allows. Nothing was changed.
    TooManyChanges(usize),
    /// Re-reading a changed record failed.
    Record(RecordError),
    /// A refreshed record would exceed the memory budget.
    Budget(BudgetExceeded),
}

impl fmt::Display for DeltaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyChanges(count) => {
                write!(
                    f,
                    "USN journal touched {count} MFT records, over the delta limit"
                )
            }
            Self::Record(err) => write!(f, "re-reading a changed MFT record failed: {err}"),
            Self::Budget(err) => write!(f, "{err}"),
        }
    }
}

impl Error for DeltaError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Record(err) => Some(err),
            Self::Budget(err) => Some(err),
            Self::TooManyChanges(_) => None,
        }
    }
}

/// Largest number of distinct records a delta may touch before a full walk is cheaper.
///
/// The floor is 100 000 records. Above that, the limit is a fifth of the cached records.
pub fn max_touched(cached_records: usize) -> usize {
    (cached_records / 5).max(100_000)
}

/// Brings `tree` up to date with the journal `entries` by re-reading each touched record.
///
/// The journal reasons are not interpreted. Every touched record is re-read from `source`, so
/// renames, moves, hard links, size and time changes and file-reference reuse all follow one
/// rule. A record that still reads as a file replaces its cached entry. A record with no file
/// left is removed. Metafile numbers other than the root directory are skipped, since
/// [`VolumeTree::push`] would drop them anyway.
///
/// The touched set is checked against `max_touched` before any record is read or changed, so
/// an over-limit delta leaves `tree` as it was.
///
/// On `Err` from a re-read or the budget, `tree` is in an unspecified state: some records may
/// be removed or replaced and others not. The caller must discard the tree and walk the volume.
pub fn apply_delta<S: RecordSource>(
    tree: &mut VolumeTree,
    source: &mut S,
    entries: &[UsnEntry],
    max_touched: usize,
) -> Result<DeltaStats, DeltaError> {
    let mut touched: BTreeSet<u64> = entries.iter().map(UsnEntry::record_number).collect();
    touched.retain(|&number| number == ROOT_DIR_RECORD || number >= FIRST_USER_RECORD);
    if touched.len() > max_touched {
        return Err(DeltaError::TooManyChanges(touched.len()));
    }
    for &number in &touched {
        match source.record(number).map_err(DeltaError::Record)? {
            Some(facts) => {
                // Remove first: `push` ignores a record that is now filtered out, which would
                // otherwise leave the old version in place.
                tree.remove(number);
                tree.push(facts).map_err(DeltaError::Budget)?;
            }
            None => tree.remove(number),
        }
    }
    Ok(DeltaStats {
        entries: entries.len(),
        reread: touched.len(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use dedupe_core::path::RawPath;
    use dedupe_core::traits::{FileMeta, WalkItem};

    use super::*;

    /// Display prefix the fixture trees are emitted under.
    const ROOT_PREFIX: &[u8] = b"C:\\scan";

    /// First byte of the first record body.
    const RECORD_START_OFFSET: usize = HEADER_LEN;
    const COUNT_OFFSET: usize = HEADER_LEN - 8;

    fn sample_records() -> Vec<RecordFacts> {
        vec![
            RecordFacts {
                record: 30,
                sequence: 2,
                in_use: true,
                is_dir: false,
                attrs: 0x20 | 0x0004_0000,
                reparse_tag: Some(0xA000_0003),
                mtime_ft: 132_000_000_000_000_001,
                ctime_ft: 131_000_000_000_000_002,
                size: 4096,
                names: vec![
                    NameFact {
                        parent: 5,
                        name: "report.txt".encode_utf16().collect(),
                        namespace: NameSpace::Win32,
                    },
                    NameFact {
                        parent: 5,
                        name: "REPORT~1.TXT".encode_utf16().collect(),
                        namespace: NameSpace::Dos,
                    },
                    NameFact {
                        parent: 40,
                        // Non-BMP character: encoded as a UTF-16 surrogate pair.
                        name: "a\u{1D11E}b".encode_utf16().collect(),
                        namespace: NameSpace::Win32AndDos,
                    },
                ],
            },
            RecordFacts {
                record: 31,
                sequence: 1,
                in_use: true,
                is_dir: true,
                attrs: 0x10,
                reparse_tag: None,
                mtime_ft: 1,
                ctime_ft: 2,
                size: 0,
                names: vec![NameFact {
                    parent: 5,
                    name: "dir".encode_utf16().collect(),
                    namespace: NameSpace::Posix,
                }],
            },
            RecordFacts {
                record: 32,
                sequence: 0,
                in_use: false,
                is_dir: false,
                attrs: 0,
                reparse_tag: None,
                mtime_ft: 0,
                ctime_ft: 0,
                size: 0,
                names: Vec::new(),
            },
        ]
    }

    fn sample_index() -> CachedIndex {
        CachedIndex {
            volume: VolumeStamp {
                volume_serial: 0xABCD_1234,
                bytes_per_record: 1024,
                total_bytes: 1 << 30,
                mft_valid_length: 1 << 20,
            },
            journal: JournalStamp {
                journal_id: 0x0102_0304_0506_0708,
                next_usn: 4096,
            },
            records: sample_records(),
        }
    }

    /// The encoded body without its 32-byte trailer.
    fn body_of(encoded: &[u8]) -> Vec<u8> {
        encoded[..encoded.len() - TRAILER_LEN].to_vec()
    }

    /// Appends a fresh BLAKE3 trailer so only the body edits are under test.
    fn reseal(mut body: Vec<u8>) -> Vec<u8> {
        let hash = blake3::hash(&body);
        body.extend_from_slice(hash.as_bytes());
        body
    }

    #[test]
    fn encode_decode_should_roundtrip_when_records_have_multiple_names() {
        let index = sample_index();
        let decoded = decode(&encode(&index));
        match decoded {
            Ok(back) => assert_eq!(
                back, index,
                "decoded index must equal the encoded one, including Dos names and reparse tags"
            ),
            Err(err) => panic!("valid cache failed to decode: {err}"),
        }
    }

    #[test]
    fn decode_should_reject_when_checksum_is_wrong() {
        let mut bytes = encode(&sample_index());
        bytes[RECORD_START_OFFSET + 2] ^= 0xFF;
        assert!(
            matches!(decode(&bytes), Err(CacheError::BadChecksum)),
            "a flipped body byte must fail the checksum before any field is parsed"
        );
    }

    #[test]
    fn decode_should_reject_when_version_differs() {
        let mut body = body_of(&encode(&sample_index()));
        body[8..12].copy_from_slice(&3u32.to_le_bytes());
        assert!(
            matches!(decode(&reseal(body)), Err(CacheError::BadVersion(3))),
            "a resealed file with format version 3 must be rejected as BadVersion(3)"
        );
    }

    #[test]
    fn decode_should_reject_when_version_is_pre_change_time() {
        // v1 stored the creation time in ctime_ft, so a v1 cache must not seed a scan.
        let mut body = body_of(&encode(&sample_index()));
        body[8..12].copy_from_slice(&1u32.to_le_bytes());
        assert!(
            matches!(decode(&reseal(body)), Err(CacheError::BadVersion(1))),
            "a resealed file with format version 1 must be rejected as BadVersion(1)"
        );
    }

    #[test]
    fn decode_should_accept_when_version_is_current() {
        let bytes = encode(&sample_index());
        assert!(
            decode(&bytes).is_ok(),
            "a file written by encode at the current FORMAT_VERSION must decode"
        );
    }

    #[test]
    fn decode_should_reject_when_magic_differs() {
        let mut body = body_of(&encode(&sample_index()));
        body[0] = b'X';
        assert!(
            matches!(decode(&reseal(body)), Err(CacheError::BadMagic)),
            "a resealed file with the wrong magic must be rejected as BadMagic"
        );
    }

    #[test]
    fn decode_should_reject_when_truncated() {
        assert!(
            matches!(decode(&[0u8; 10]), Err(CacheError::Truncated)),
            "a buffer shorter than header plus trailer must be Truncated"
        );
        let body = body_of(&encode(&sample_index()));
        let short = reseal(body[..HEADER_LEN + 5].to_vec());
        assert!(
            matches!(decode(&short), Err(CacheError::Truncated)),
            "a resealed body that ends mid-record must be Truncated"
        );
    }

    #[test]
    fn decode_should_reject_when_record_count_overruns_buffer() {
        let mut body = body_of(&encode(&sample_index()));
        body[COUNT_OFFSET..HEADER_LEN].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(
            matches!(decode(&reseal(body)), Err(CacheError::Truncated)),
            "a record count far beyond the buffer must fail as Truncated without a huge allocation"
        );
    }

    #[test]
    fn validate_should_reject_when_journal_id_differs() {
        let index = sample_index();
        let reason = validate(&index, &index.volume, 99, 0, 8192);
        assert_eq!(
            reason,
            Some("USN journal was recreated"),
            "a different journal id must mean the journal was recreated"
        );
    }

    #[test]
    fn validate_should_reject_when_usn_truncated_past_cache() {
        let index = sample_index();
        let reason = validate(
            &index,
            &index.volume,
            index.journal.journal_id,
            index.journal.next_usn + 1,
            8192,
        );
        assert_eq!(
            reason,
            Some("USN journal was truncated past the cache"),
            "the journal's lowest valid USN above the cached position must reject the cache"
        );
    }

    #[test]
    fn validate_should_reject_when_usn_ahead_of_journal() {
        let index = sample_index();
        let reason = validate(
            &index,
            &index.volume,
            index.journal.journal_id,
            0,
            index.journal.next_usn - 1,
        );
        assert_eq!(
            reason,
            Some("USN position is ahead of the journal"),
            "a cached USN beyond the journal's next USN must reject the cache"
        );
    }

    #[test]
    fn validate_should_reject_when_volume_serial_differs() {
        let index = sample_index();
        let mut live = index.volume;
        live.volume_serial ^= 1;
        let reason = validate(&index, &live, index.journal.journal_id, 0, 8192);
        assert_eq!(
            reason,
            Some("volume identity or geometry changed"),
            "a different volume serial must reject the cache"
        );
    }

    #[test]
    fn validate_should_reject_when_mft_shrank() {
        let index = sample_index();
        let mut live = index.volume;
        live.mft_valid_length -= 1;
        let reason = validate(&index, &live, index.journal.journal_id, 0, 8192);
        assert_eq!(
            reason,
            Some("MFT shrank"),
            "a live MFT shorter than the cached one must reject the cache"
        );
    }

    #[test]
    fn validate_should_accept_when_mft_grew() {
        let index = sample_index();
        let mut live = index.volume;
        live.mft_valid_length += 4096;
        let reason = validate(&index, &live, index.journal.journal_id, 0, 8192);
        assert_eq!(
            reason, None,
            "a live MFT longer than the cached one is allowed; new records arrive via the journal"
        );
    }

    #[test]
    fn write_atomic_then_read_should_roundtrip_when_directory_is_new() {
        let dir = std::env::temp_dir().join(format!(
            "dedupe-mft-cache-{}-write_atomic_then_read",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let index = sample_index();
        let path = cache_path(&dir, index.volume.volume_serial);

        let written = write_atomic(&path, &index);
        assert!(
            written.is_ok(),
            "write_atomic must create the missing directory and write the file: {written:?}"
        );
        let tmp_left_behind = path.with_extension("cache.tmp").exists();
        let read_back = read(&path);
        let _ = fs::remove_dir_all(&dir);

        assert!(
            !tmp_left_behind,
            "the temp file must be renamed away, not left next to the cache"
        );
        match read_back {
            Ok(back) => assert_eq!(
                back, index,
                "the file read back must equal the written index"
            ),
            Err(err) => panic!("read after write_atomic failed: {err}"),
        }
    }

    #[test]
    fn write_atomic_stream_should_produce_bytes_identical_to_encode_when_same_records() {
        let dir = std::env::temp_dir().join(format!(
            "dedupe-mft-cache-{}-write_atomic_stream_bytes",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let index = sample_index();
        let path = cache_path(&dir, index.volume.volume_serial);

        let written =
            write_atomic_stream(&path, &index.volume, &index.journal, index.records.iter());
        let on_disk = fs::read(&path);
        let _ = fs::remove_dir_all(&dir);

        assert!(
            written.is_ok(),
            "write_atomic_stream must create the file: {written:?}"
        );
        match on_disk {
            Ok(bytes) => assert_eq!(
                bytes,
                encode(&index),
                "the streamed file must be byte-identical to encode() of the same records"
            ),
            Err(err) => panic!("reading the streamed cache failed: {err}"),
        }
    }

    /// Yields the records of a slice but reports a different length, which simulates a count
    /// that changes between the header and the records.
    struct LyingLen<'a> {
        inner: std::slice::Iter<'a, RecordFacts>,
        reported: usize,
    }

    impl<'a> Iterator for LyingLen<'a> {
        type Item = &'a RecordFacts;

        fn next(&mut self) -> Option<Self::Item> {
            self.inner.next()
        }
    }

    impl ExactSizeIterator for LyingLen<'_> {
        fn len(&self) -> usize {
            self.reported
        }
    }

    #[test]
    fn write_atomic_stream_should_leave_no_temp_file_when_count_changes() {
        let dir = std::env::temp_dir().join(format!(
            "dedupe-mft-cache-{}-write_atomic_stream_count",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let index = sample_index();
        let path = cache_path(&dir, index.volume.volume_serial);
        let records = LyingLen {
            inner: index.records.iter(),
            reported: index.records.len() + 1,
        };

        let result = write_atomic_stream(&path, &index.volume, &index.journal, records);
        let target_exists = path.exists();
        let tmp_exists = temp_path(&path).exists();
        let _ = fs::remove_dir_all(&dir);

        assert!(
            matches!(
                result,
                Err(CacheError::Malformed("record count changed while writing"))
            ),
            "a count that changes mid-write must fail as Malformed, got {result:?}"
        );
        assert!(
            !target_exists,
            "a failed streamed write must not leave the target file"
        );
        assert!(
            !tmp_exists,
            "a failed streamed write must remove its temp file"
        );
    }

    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, OsString> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_string(), OsString::from(value)))
            .collect()
    }

    #[test]
    fn cache_dir_should_prefer_override_when_dedupe_cache_dir_set() {
        let map = env_from(&[
            ("DEDUPE_CACHE_DIR", "D:\\cache"),
            ("LOCALAPPDATA", "C:\\Users\\x\\AppData\\Local"),
        ]);
        let env = |key: &str| map.get(key).cloned();
        assert_eq!(
            cache_dir(&env),
            Some(PathBuf::from("D:\\cache")),
            "DEDUPE_CACHE_DIR must win over LOCALAPPDATA"
        );
    }

    #[test]
    fn cache_dir_should_use_localappdata_when_no_override() {
        let map = env_from(&[("LOCALAPPDATA", "C:\\Users\\x\\AppData\\Local")]);
        let env = |key: &str| map.get(key).cloned();
        assert_eq!(
            cache_dir(&env),
            Some(
                PathBuf::from("C:\\Users\\x\\AppData\\Local")
                    .join("dedupe")
                    .join("mft")
            ),
            "without an override the cache lives under LOCALAPPDATA\\dedupe\\mft"
        );
    }

    #[test]
    fn cache_dir_should_return_none_when_no_env() {
        let map = env_from(&[("DEDUPE_CACHE_DIR", ""), ("LOCALAPPDATA", "")]);
        let env = |key: &str| map.get(key).cloned();
        assert_eq!(
            cache_dir(&env),
            None,
            "empty overrides and no LOCALAPPDATA must mean no cache directory"
        );
    }

    /// Walk root of the delta fixtures. Records 0..=23 are metafiles and are never stored.
    const SCAN_ROOT: u64 = 24;
    const DELTA_BUDGET: usize = 1 << 24;

    /// In-memory source whose records can differ from the cached tree. Each call is logged in
    /// `record_calls`, and any number in `failing` returns a `Corrupt` error.
    struct FakeSource {
        total: u64,
        slots: BTreeMap<u64, RecordFacts>,
        failing: BTreeSet<u64>,
        record_calls: Vec<u64>,
    }

    impl FakeSource {
        fn new(total: u64, records: Vec<RecordFacts>, failing: &[u64]) -> Self {
            Self {
                total,
                slots: records
                    .into_iter()
                    .map(|facts| (facts.record, facts))
                    .collect(),
                failing: failing.iter().copied().collect(),
                record_calls: Vec::new(),
            }
        }
    }

    impl RecordSource for FakeSource {
        fn record_count(&self) -> u64 {
            self.total
        }

        fn record(&mut self, number: u64) -> Result<Option<RecordFacts>, RecordError> {
            self.record_calls.push(number);
            if self.failing.contains(&number) {
                return Err(RecordError::Corrupt("test record is damaged"));
            }
            Ok(self.slots.get(&number).cloned())
        }
    }

    fn component(units: &[u16]) -> RawPath {
        RawPath::new(String::from_utf16_lossy(units).into_bytes())
    }

    fn win32_name(parent: u64, text: &str) -> NameFact {
        NameFact {
            parent,
            name: text.encode_utf16().collect(),
            namespace: NameSpace::Win32,
        }
    }

    fn dos_name(parent: u64, text: &str) -> NameFact {
        NameFact {
            parent,
            name: text.encode_utf16().collect(),
            namespace: NameSpace::Dos,
        }
    }

    fn facts_of(record: u64, is_dir: bool, names: Vec<NameFact>) -> RecordFacts {
        RecordFacts {
            record,
            sequence: 1,
            in_use: true,
            is_dir,
            attrs: 0,
            reparse_tag: None,
            mtime_ft: 0,
            ctime_ft: 0,
            size: if is_dir { 0 } else { record * 10 },
            names,
        }
    }

    /// Journal entry for `record` at sequence 1. The reason is a rename, but `apply_delta`
    /// ignores it.
    fn entry(record: u64) -> UsnEntry {
        UsnEntry {
            frn: record | (1u64 << 48),
            parent_frn: 0,
            usn: 0,
            reason: 0x100,
            attrs: 0,
        }
    }

    /// Builds the tree a full walk would cache for `records`.
    fn tree_of(records: Vec<RecordFacts>) -> VolumeTree {
        VolumeTree::from_facts(SCAN_ROOT, DELTA_BUDGET, records)
            .expect("test budget is large enough")
    }

    /// Path of every item, in the order `into_items` returns them.
    fn item_paths(items: &[WalkItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item {
                WalkItem::Dir { path }
                | WalkItem::File { path, .. }
                | WalkItem::Error { path, .. } => path.to_display(),
            })
            .collect()
    }

    fn item_files(items: &[WalkItem]) -> Vec<(String, FileMeta)> {
        items
            .iter()
            .filter_map(|item| match item {
                WalkItem::File { path, meta } => Some((path.to_display(), meta.clone())),
                WalkItem::Dir { .. } | WalkItem::Error { .. } => None,
            })
            .collect()
    }

    /// Items of the tree a full walk would build from `records`.
    fn items_of(records: Vec<RecordFacts>) -> Vec<WalkItem> {
        tree_of(records).into_items(&RawPath::new(ROOT_PREFIX), &component)
    }

    /// Root 24 holds "Users" (25) and "b.txt" (28) and "Old" (29). "Users" holds "Docs" (26),
    /// which holds "a.txt" (27). "Old" holds "inner.txt" (31).
    fn original_records() -> Vec<RecordFacts> {
        vec![
            facts_of(SCAN_ROOT, true, vec![win32_name(SCAN_ROOT, ".")]),
            facts_of(25, true, vec![win32_name(SCAN_ROOT, "Users")]),
            facts_of(26, true, vec![win32_name(25, "Docs")]),
            facts_of(27, false, vec![win32_name(26, "a.txt")]),
            facts_of(28, false, vec![win32_name(SCAN_ROOT, "b.txt")]),
            facts_of(29, true, vec![win32_name(SCAN_ROOT, "Old")]),
            facts_of(31, false, vec![win32_name(29, "inner.txt")]),
        ]
    }

    /// The same volume after the changes: "a.txt" deleted, "b.txt" renamed, "Old" moved under
    /// "Users", and "new.txt" created under "Docs".
    fn mutated_records() -> Vec<RecordFacts> {
        vec![
            facts_of(SCAN_ROOT, true, vec![win32_name(SCAN_ROOT, ".")]),
            facts_of(25, true, vec![win32_name(SCAN_ROOT, "Users")]),
            facts_of(26, true, vec![win32_name(25, "Docs")]),
            facts_of(28, false, vec![win32_name(SCAN_ROOT, "b-renamed.txt")]),
            facts_of(29, true, vec![win32_name(25, "Old")]),
            facts_of(31, false, vec![win32_name(29, "inner.txt")]),
            facts_of(32, false, vec![win32_name(26, "new.txt")]),
        ]
    }

    #[test]
    fn apply_delta_should_equal_full_walk_when_files_added_renamed_and_deleted() {
        let mut tree = tree_of(original_records());
        let mut source = FakeSource::new(64, mutated_records(), &[]);
        let entries = [entry(27), entry(28), entry(29), entry(32)];

        let stats = apply_delta(&mut tree, &mut source, &entries, 1000)
            .expect("a delta over a valid source must apply");
        assert_eq!(
            stats,
            DeltaStats {
                entries: 4,
                reread: 4
            },
            "stats must count the four entries and the four distinct records re-read"
        );

        let delta_items = tree.into_items(&RawPath::new(ROOT_PREFIX), &component);
        let full_items = items_of(mutated_records());
        assert_eq!(
            (item_paths(&delta_items), item_files(&delta_items)),
            (item_paths(&full_items), item_files(&full_items)),
            "the delta result must match a full walk of the changed volume"
        );
    }

    #[test]
    fn apply_delta_should_remove_stale_record_when_new_version_is_dos_only() {
        let mut tree = tree_of(original_records());
        let mut records: Vec<RecordFacts> = original_records()
            .into_iter()
            .filter(|facts| facts.record != 28)
            .collect();
        // Record 28 now carries only its 8.3 name, which `push` drops.
        records.push(facts_of(28, false, vec![dos_name(SCAN_ROOT, "B~1.TXT")]));
        let mut source = FakeSource::new(64, records, &[]);

        apply_delta(&mut tree, &mut source, &[entry(28)], 1000)
            .expect("a DOS-only re-read is not an error");

        assert!(
            tree.records_iter().all(|facts| facts.record != 28),
            "a record whose re-read keeps only DOS names must be removed, not left at its old name"
        );
        let items = tree.into_items(&RawPath::new(ROOT_PREFIX), &component);
        assert!(
            !item_paths(&items)
                .iter()
                .any(|path| path.ends_with("b.txt")),
            "the old Win32 name must not survive the delta, got {:?}",
            item_paths(&items)
        );
    }

    #[test]
    fn apply_delta_should_error_when_touched_set_exceeds_limit() {
        let mut tree = tree_of(original_records());
        let before: Vec<RecordFacts> = tree.records_iter().cloned().collect();
        let mut source = FakeSource::new(64, mutated_records(), &[]);
        let entries = [entry(25), entry(26), entry(27), entry(28), entry(29)];

        let result = apply_delta(&mut tree, &mut source, &entries, 4);

        assert!(
            matches!(result, Err(DeltaError::TooManyChanges(5))),
            "5 touched records over a limit of 4 must fail with TooManyChanges(5), got {result:?}"
        );
        let after: Vec<RecordFacts> = tree.records_iter().cloned().collect();
        assert_eq!(
            after, before,
            "an over-limit delta must leave the tree exactly as it was"
        );
        assert!(
            source.record_calls.is_empty(),
            "no record may be re-read once the limit is exceeded, got {:?}",
            source.record_calls
        );
    }

    #[test]
    fn apply_delta_should_ignore_metafile_records_when_entries_touch_them() {
        let mut tree = tree_of(original_records());
        let mut source = FakeSource::new(64, mutated_records(), &[]);
        let entries = [entry(0), entry(3), entry(5), entry(24)];

        apply_delta(&mut tree, &mut source, &entries, 1000)
            .expect("metafile entries are skipped, not errors");

        assert_eq!(
            source.record_calls,
            vec![5, 24],
            "only the root directory (5) and user records may be re-read, so metafiles 0 and 3 \
             must not be read"
        );
    }

    #[test]
    fn apply_delta_should_return_record_error_when_reread_fails() {
        let mut tree = tree_of(original_records());
        let mut source = FakeSource::new(64, mutated_records(), &[28]);

        let result = apply_delta(&mut tree, &mut source, &[entry(28)], 1000);

        assert!(
            matches!(result, Err(DeltaError::Record(RecordError::Corrupt(_)))),
            "a failed re-read must surface as DeltaError::Record, got {result:?}"
        );
    }

    #[test]
    fn max_touched_should_scale_with_cache_size_when_cache_exceeds_floor() {
        assert_eq!(
            max_touched(0),
            100_000,
            "an empty cache must get the 100 000 record floor"
        );
        assert_eq!(
            max_touched(500_000),
            100_000,
            "a fifth of 500 000 equals the floor"
        );
        assert_eq!(
            max_touched(1_000_000),
            200_000,
            "above the floor the limit must be a fifth of the cached records"
        );
    }
}
