//! USN change journal: portable parsing of `FSCTL_READ_USN_JOURNAL` output.
//!
//! The parsing and request-building code is pure and unit-tested on every OS. The Win32 ioctl
//! wrappers (`query_journal`, `read_journal`) are `cfg(windows)` items.

use std::fmt;
use std::io;

#[cfg(windows)]
use windows_sys::Win32::System::Ioctl::{FSCTL_QUERY_USN_JOURNAL, FSCTL_READ_USN_JOURNAL};

#[cfg(windows)]
use crate::volume_io::RawVolume;

/// `ERROR_JOURNAL_NOT_ACTIVE`: the volume has no USN journal, or it was deleted.
pub const ERROR_JOURNAL_NOT_ACTIVE: u32 = 1179;
/// `ERROR_JOURNAL_DELETE_IN_PROGRESS`: the journal is being deleted and cannot be read.
pub const ERROR_JOURNAL_DELETE_IN_PROGRESS: u32 = 1178;
/// `ERROR_JOURNAL_ENTRY_DELETED`: the requested USN has already been dropped from the journal.
pub const ERROR_JOURNAL_ENTRY_DELETED: u32 = 1181;

/// Size of the next-USN header that precedes the records in a read buffer.
const HEADER_LEN: usize = 8;
/// Smallest valid `USN_RECORD_V2`: the fixed fields, with no file name.
const RECORD_V2_MIN_LEN: usize = 60;
/// The only major version this parser accepts; the journal read requests it explicitly.
const RECORD_V2_MAJOR: u16 = 2;
/// Records are packed on 8-byte boundaries.
const RECORD_ALIGN: usize = 8;

/// One journal change, reduced to the fields the cache refresh needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UsnEntry {
    /// 64-bit file reference number: low 48 bits are the record number, high 16 bits the sequence.
    pub frn: u64,
    pub parent_frn: u64,
    pub usn: i64,
    pub reason: u32,
    pub attrs: u32,
}

impl UsnEntry {
    /// Record number (low 48 bits of `frn`). Matches the private `RECORD_NUMBER_MASK` in
    /// `volume_io`; keep the two in sync.
    pub fn record_number(&self) -> u64 {
        self.frn & ((1 << 48) - 1)
    }
}

/// One read of the journal: the USN to resume from, plus the records it returned.
#[derive(Debug, PartialEq, Eq)]
pub struct UsnBatch {
    pub next_usn: i64,
    pub entries: Vec<UsnEntry>,
}

#[derive(Debug)]
pub enum UsnError {
    /// The buffer ends before a field it must contain.
    Truncated,
    /// A record length is zero, too small, or runs past the end of the buffer.
    Malformed,
    /// A record's major version is not 2, so the request was answered with another format.
    UnsupportedVersion,
    /// The volume has no active USN journal.
    JournalNotActive,
    /// The journal is being deleted.
    JournalDeleteInProgress,
    /// The requested USN is no longer in the journal; the caller must rescan.
    EntryDeleted,
    Io(io::Error),
}

impl UsnError {
    /// Maps a Win32 error code to a journal variant. Unknown codes are wrapped as `Io(err)`.
    pub fn from_os_code(code: u32, err: io::Error) -> Self {
        match code {
            ERROR_JOURNAL_NOT_ACTIVE => Self::JournalNotActive,
            ERROR_JOURNAL_DELETE_IN_PROGRESS => Self::JournalDeleteInProgress,
            ERROR_JOURNAL_ENTRY_DELETED => Self::EntryDeleted,
            _ => Self::Io(err),
        }
    }
}

impl fmt::Display for UsnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("USN journal buffer is truncated"),
            Self::Malformed => f.write_str("USN journal record is malformed"),
            Self::UnsupportedVersion => f.write_str("USN journal record version is not 2"),
            Self::JournalNotActive => f.write_str("USN journal is not active on this volume"),
            Self::JournalDeleteInProgress => f.write_str("USN journal is being deleted"),
            Self::EntryDeleted => f.write_str("USN journal entry was deleted"),
            Self::Io(err) => write!(f, "USN journal I/O error: {err}"),
        }
    }
}

impl std::error::Error for UsnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for UsnError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Reads `N` bytes at `at`, or `Truncated` if they are not all present.
fn bytes_at<const N: usize>(buf: &[u8], at: usize) -> Result<[u8; N], UsnError> {
    let end = at.checked_add(N).ok_or(UsnError::Truncated)?;
    let slice = buf.get(at..end).ok_or(UsnError::Truncated)?;
    <[u8; N]>::try_from(slice).map_err(|_| UsnError::Truncated)
}

fn u16_at(buf: &[u8], at: usize) -> Result<u16, UsnError> {
    Ok(u16::from_le_bytes(bytes_at(buf, at)?))
}

fn u32_at(buf: &[u8], at: usize) -> Result<u32, UsnError> {
    Ok(u32::from_le_bytes(bytes_at(buf, at)?))
}

fn u64_at(buf: &[u8], at: usize) -> Result<u64, UsnError> {
    Ok(u64::from_le_bytes(bytes_at(buf, at)?))
}

fn i64_at(buf: &[u8], at: usize) -> Result<i64, UsnError> {
    Ok(i64::from_le_bytes(bytes_at(buf, at)?))
}

/// Parses a `FSCTL_READ_USN_JOURNAL` output buffer: an 8-byte next USN, then packed
/// `USN_RECORD_V2` records.
///
/// Trailing bytes shorter than one alignment unit after the last record are treated as padding
/// and ignored. Any record that claims a length past the end of the buffer is `Malformed`.
pub fn parse_usn_buffer(buf: &[u8]) -> Result<UsnBatch, UsnError> {
    let next_usn = i64_at(buf, 0)?;
    let mut entries = Vec::new();
    let mut at = HEADER_LEN;
    while buf.len().saturating_sub(at) >= RECORD_ALIGN {
        let len = usize::try_from(u32_at(buf, at)?).map_err(|_| UsnError::Malformed)?;
        if len < RECORD_V2_MIN_LEN {
            return Err(UsnError::Malformed);
        }
        let end = at.checked_add(len).ok_or(UsnError::Malformed)?;
        let record = buf.get(at..end).ok_or(UsnError::Malformed)?;
        if u16_at(record, 4)? != RECORD_V2_MAJOR {
            return Err(UsnError::UnsupportedVersion);
        }
        entries.push(UsnEntry {
            frn: u64_at(record, 8)?,
            parent_frn: u64_at(record, 16)?,
            usn: i64_at(record, 24)?,
            reason: u32_at(record, 40)?,
            attrs: u32_at(record, 52)?,
        });
        // `len >= RECORD_V2_MIN_LEN`, so this always advances by at least 64 bytes.
        let padded = len
            .checked_add(RECORD_ALIGN - 1)
            .ok_or(UsnError::Malformed)?
            & !(RECORD_ALIGN - 1);
        at = at.checked_add(padded).ok_or(UsnError::Malformed)?;
    }
    Ok(UsnBatch { next_usn, entries })
}

/// The journal fields the cache refresh needs, from `FSCTL_QUERY_USN_JOURNAL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalInfo {
    /// Identifies this journal instance. A deleted and recreated journal gets a new id.
    pub journal_id: u64,
    pub first_usn: i64,
    /// The USN the next record will get. Every record written so far has a smaller USN.
    pub next_usn: i64,
    /// The oldest USN still in the journal. Older records have been dropped.
    pub lowest_valid_usn: i64,
}

/// Bytes of `USN_JOURNAL_DATA_V0` that are read: the fields up to `LowestValidUsn`. V1 and V2
/// share this prefix.
const JOURNAL_PREFIX_LEN: usize = 32;
/// Output size for `FSCTL_QUERY_USN_JOURNAL`, larger than the 56-byte V0 struct.
#[cfg(windows)]
const QUERY_BUFFER_LEN: usize = 64;
/// Output size for one `FSCTL_READ_USN_JOURNAL` call. Holds many records per round trip.
#[cfg(windows)]
const READ_BUFFER_LEN: usize = 64 * 1024;
/// Size of `READ_USN_JOURNAL_DATA_V1`, the input to `FSCTL_READ_USN_JOURNAL`.
const READ_REQUEST_LEN: usize = 48;
/// Hard cap on journal reads per call. A malformed journal must not keep the scan looping.
#[cfg(windows)]
const MAX_READ_ROUNDS: usize = 1_000_000;

/// Parses a `USN_JOURNAL_DATA_V0` buffer, as written by `FSCTL_QUERY_USN_JOURNAL`.
///
/// Only the prefix is read, so a longer V1 or V2 buffer parses the same way.
pub fn parse_journal_data(buf: &[u8]) -> Result<JournalInfo, UsnError> {
    if buf.len() < JOURNAL_PREFIX_LEN {
        return Err(UsnError::Truncated);
    }
    Ok(JournalInfo {
        journal_id: u64_at(buf, 0)?,
        first_usn: i64_at(buf, 8)?,
        next_usn: i64_at(buf, 16)?,
        lowest_valid_usn: i64_at(buf, 24)?,
    })
}

/// Builds the `READ_USN_JOURNAL_DATA_V1` input for one read starting at `start_usn`.
///
/// The reason mask is all bits, and both wait fields are zero, so the call returns at once when
/// the journal has no more records. Major version 2 selects `USN_RECORD_V2`.
pub fn read_request_bytes(start_usn: i64, journal_id: u64) -> [u8; READ_REQUEST_LEN] {
    let mut buf = [0u8; READ_REQUEST_LEN];
    buf[0..8].copy_from_slice(&start_usn.to_le_bytes());
    buf[8..12].copy_from_slice(&u32::MAX.to_le_bytes());
    // ReturnOnlyOnClose (12..16), Timeout (16..24) and BytesToWaitFor (24..32) stay zero.
    buf[32..40].copy_from_slice(&journal_id.to_le_bytes());
    buf[40..42].copy_from_slice(&RECORD_V2_MAJOR.to_le_bytes());
    buf[42..44].copy_from_slice(&RECORD_V2_MAJOR.to_le_bytes());
    buf
}

/// Maps an io error from a journal control to its journal variant.
#[cfg(windows)]
fn journal_error(err: io::Error) -> UsnError {
    UsnError::from_os_code(err.raw_os_error().unwrap_or(0) as u32, err)
}

/// Reads the journal state of an NTFS volume with `FSCTL_QUERY_USN_JOURNAL`.
///
/// Fails with `JournalNotActive` when the volume has no journal. The journal is never created.
#[cfg(windows)]
pub fn query_journal(vol: &RawVolume) -> Result<JournalInfo, UsnError> {
    let mut out = [0u8; QUERY_BUFFER_LEN];
    let written = vol
        .ioctl(FSCTL_QUERY_USN_JOURNAL, &[], &mut out)
        .map_err(journal_error)?;
    parse_journal_data(&out[..written])
}

/// Reads every journal record with a USN below `end_usn`, starting at `start_usn`.
///
/// Stops when the cursor reaches `end_usn`, when a read returns no records, or when a read does
/// not move the cursor. Returns `Malformed` after `MAX_READ_ROUNDS` reads.
#[cfg(windows)]
pub fn read_journal(
    vol: &RawVolume,
    journal_id: u64,
    start_usn: i64,
    end_usn: i64,
) -> Result<Vec<UsnEntry>, UsnError> {
    let mut entries = Vec::new();
    let mut cursor = start_usn;
    let mut buf = vec![0u8; READ_BUFFER_LEN];
    for _ in 0..MAX_READ_ROUNDS {
        if cursor >= end_usn {
            return Ok(entries);
        }
        let request = read_request_bytes(cursor, journal_id);
        let written = vol
            .ioctl(FSCTL_READ_USN_JOURNAL, &request, &mut buf)
            .map_err(journal_error)?;
        let batch = parse_usn_buffer(&buf[..written])?;
        let has_records = !batch.entries.is_empty();
        let advanced = batch.next_usn > cursor;
        entries.extend(
            batch
                .entries
                .into_iter()
                .filter(|entry| entry.usn < end_usn),
        );
        if !has_records || !advanced {
            // The journal has nothing more to give. That is a clean end only if it reached
            // `end_usn`; otherwise records were lost, and a partial delta must not be applied.
            return if batch.next_usn.max(cursor) >= end_usn {
                Ok(entries)
            } else {
                Err(UsnError::Malformed)
            };
        }
        cursor = batch.next_usn;
    }
    Err(UsnError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds one `USN_RECORD_V2` byte image. The name is UTF-16 and the image is padded to 8 bytes.
    fn v2_record(frn: u64, parent: u64, usn: i64, reason: u32, attrs: u32, name: &str) -> Vec<u8> {
        let name_units: Vec<u16> = name.encode_utf16().collect();
        let name_bytes = name_units.len() * 2;
        let len = RECORD_V2_MIN_LEN + name_bytes;
        let padded = (len + RECORD_ALIGN - 1) & !(RECORD_ALIGN - 1);
        let mut rec = vec![0u8; padded];
        rec[0..4].copy_from_slice(&(len as u32).to_le_bytes());
        rec[4..6].copy_from_slice(&2u16.to_le_bytes());
        rec[6..8].copy_from_slice(&0u16.to_le_bytes());
        rec[8..16].copy_from_slice(&frn.to_le_bytes());
        rec[16..24].copy_from_slice(&parent.to_le_bytes());
        rec[24..32].copy_from_slice(&usn.to_le_bytes());
        rec[40..44].copy_from_slice(&reason.to_le_bytes());
        rec[52..56].copy_from_slice(&attrs.to_le_bytes());
        rec[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
        rec[58..60].copy_from_slice(&(RECORD_V2_MIN_LEN as u16).to_le_bytes());
        for (i, unit) in name_units.iter().enumerate() {
            let at = RECORD_V2_MIN_LEN + i * 2;
            rec[at..at + 2].copy_from_slice(&unit.to_le_bytes());
        }
        rec
    }

    /// Prefixes the 8-byte next USN to the given record images.
    fn usn_buffer(next_usn: i64, records: &[Vec<u8>]) -> Vec<u8> {
        let mut buf = next_usn.to_le_bytes().to_vec();
        for rec in records {
            buf.extend_from_slice(rec);
        }
        buf
    }

    #[test]
    fn parse_usn_buffer_should_return_entries_when_two_v2_records() {
        let first = v2_record(0x0001_0000_0000_0005, 5, 100, 0x100, 0x20, "a.txt");
        let second = v2_record(
            0x0002_0000_0000_0042,
            0x0005_0000_0000_0005,
            228,
            0x2,
            0x10,
            "longer_name.bin",
        );
        let buf = usn_buffer(500, &[first, second]);

        let batch = parse_usn_buffer(&buf).expect("two well-formed V2 records must parse");

        assert_eq!(
            batch.next_usn, 500,
            "next USN must come from the 8-byte header"
        );
        assert_eq!(
            batch.entries,
            vec![
                UsnEntry {
                    frn: 0x0001_0000_0000_0005,
                    parent_frn: 5,
                    usn: 100,
                    reason: 0x100,
                    attrs: 0x20
                },
                UsnEntry {
                    frn: 0x0002_0000_0000_0042,
                    parent_frn: 0x0005_0000_0000_0005,
                    usn: 228,
                    reason: 0x2,
                    attrs: 0x10,
                },
            ],
            "both records must round-trip every field, including the padded second record"
        );
    }

    #[test]
    fn parse_usn_buffer_should_return_empty_when_only_header() {
        let buf = usn_buffer(42, &[]);

        let batch = parse_usn_buffer(&buf).expect("a header-only buffer is a valid empty batch");

        assert_eq!(
            batch.next_usn, 42,
            "next USN must still be read from the header"
        );
        assert!(
            batch.entries.is_empty(),
            "no records means no entries, got {:?}",
            batch.entries
        );
    }

    #[test]
    fn parse_usn_buffer_should_error_when_major_version_is_3() {
        let mut rec = v2_record(1, 5, 10, 0, 0, "x");
        rec[4..6].copy_from_slice(&3u16.to_le_bytes());
        let buf = usn_buffer(20, &[rec]);

        let err = parse_usn_buffer(&buf).expect_err("a V3 record must not be parsed as V2");

        assert!(
            matches!(err, UsnError::UnsupportedVersion),
            "expected UnsupportedVersion, got {err:?}"
        );
    }

    #[test]
    fn parse_usn_buffer_should_error_when_buffer_shorter_than_header() {
        let buf = [0u8; 7];

        let err = parse_usn_buffer(&buf).expect_err("a 7-byte buffer has no complete header");

        assert!(
            matches!(err, UsnError::Truncated),
            "expected Truncated, got {err:?}"
        );
    }

    #[test]
    fn parse_usn_buffer_should_error_when_record_length_overruns_buffer() {
        let mut rec = v2_record(1, 5, 10, 0, 0, "x");
        rec[0..4].copy_from_slice(&200u32.to_le_bytes());
        let buf = usn_buffer(20, &[rec]);

        let err =
            parse_usn_buffer(&buf).expect_err("a record longer than the buffer must be rejected");

        assert!(
            matches!(err, UsnError::Malformed),
            "expected Malformed, got {err:?}"
        );
    }

    #[test]
    fn parse_usn_buffer_should_error_when_record_length_is_zero() {
        let mut rec = v2_record(1, 5, 10, 0, 0, "x");
        rec[0..4].copy_from_slice(&0u32.to_le_bytes());
        let buf = usn_buffer(20, &[rec]);

        let err = parse_usn_buffer(&buf)
            .expect_err("a zero-length record must be rejected, not looped on");

        assert!(
            matches!(err, UsnError::Malformed),
            "expected Malformed, got {err:?}"
        );
    }

    #[test]
    fn parse_usn_buffer_should_error_when_record_length_too_small() {
        let mut rec = v2_record(1, 5, 10, 0, 0, "x");
        rec[0..4].copy_from_slice(&59u32.to_le_bytes());
        let buf = usn_buffer(20, &[rec]);

        let err = parse_usn_buffer(&buf)
            .expect_err("a record below the 60-byte minimum must be rejected");

        assert!(
            matches!(err, UsnError::Malformed),
            "expected Malformed, got {err:?}"
        );
    }

    #[test]
    fn usn_error_from_os_code_should_map_journal_codes() {
        let io_err = || io::Error::other("synthetic");

        assert!(matches!(
            UsnError::from_os_code(1179, io_err()),
            UsnError::JournalNotActive
        ));
        assert!(matches!(
            UsnError::from_os_code(1178, io_err()),
            UsnError::JournalDeleteInProgress
        ));
        assert!(matches!(
            UsnError::from_os_code(1181, io_err()),
            UsnError::EntryDeleted
        ));
        assert!(
            matches!(UsnError::from_os_code(5, io_err()), UsnError::Io(_)),
            "unknown codes must wrap as Io"
        );
    }

    #[test]
    fn usn_entry_record_number_should_mask_sequence_bits() {
        let entry = UsnEntry {
            frn: (7u64 << 48) | 1234,
            parent_frn: 5,
            usn: 0,
            reason: 0,
            attrs: 0,
        };

        assert_eq!(
            entry.record_number(),
            1234,
            "the high 16 sequence bits must be masked off, leaving the record number"
        );
    }

    #[test]
    fn parse_journal_data_should_read_fields_when_buffer_is_v0_layout() {
        let mut buf = vec![0u8; 56];
        buf[0..8].copy_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
        buf[8..16].copy_from_slice(&10i64.to_le_bytes());
        buf[16..24].copy_from_slice(&20i64.to_le_bytes());
        buf[24..32].copy_from_slice(&5i64.to_le_bytes());

        let info = parse_journal_data(&buf).expect("a full V0 buffer must parse");

        assert_eq!(
            info,
            JournalInfo {
                journal_id: 0x0102_0304_0506_0708,
                first_usn: 10,
                next_usn: 20,
                lowest_valid_usn: 5,
            },
            "each field must come from its V0 offset"
        );
    }

    #[test]
    fn parse_journal_data_should_error_when_buffer_too_short() {
        let buf = [0u8; 31];

        let err = parse_journal_data(&buf).expect_err("31 bytes is one short of the prefix");

        assert!(
            matches!(err, UsnError::Truncated),
            "expected Truncated, got {err:?}"
        );
    }

    #[test]
    fn read_request_bytes_should_place_fields_at_documented_offsets() {
        let req = read_request_bytes(-7, 0xAABB_CCDD_EEFF_0011);

        assert_eq!(&req[0..8], &(-7i64).to_le_bytes(), "StartUsn at offset 0");
        assert_eq!(
            &req[8..12],
            &u32::MAX.to_le_bytes(),
            "ReasonMask at offset 8 is all bits"
        );
        assert_eq!(
            &req[12..32],
            &[0u8; 20],
            "ReturnOnlyOnClose, Timeout and BytesToWaitFor are zero"
        );
        assert_eq!(
            &req[32..40],
            &0xAABB_CCDD_EEFF_0011u64.to_le_bytes(),
            "UsnJournalID at offset 32"
        );
        assert_eq!(
            &req[40..42],
            &2u16.to_le_bytes(),
            "MinMajorVersion at offset 40"
        );
        assert_eq!(
            &req[42..44],
            &2u16.to_le_bytes(),
            "MaxMajorVersion at offset 42"
        );
        assert_eq!(
            &req[44..48],
            &[0u8; 4],
            "the tail past MaxMajorVersion is zero"
        );
    }
}

/// Layout check for the Win32 USN structs. It compiles only for Windows, so `cargo check --target
/// x86_64-pc-windows-gnu --all-targets` verifies the offsets without running anything.
#[cfg(all(test, windows))]
mod layout_tests {
    use std::mem::{offset_of, size_of};

    use windows_sys::Win32::System::Ioctl::{READ_USN_JOURNAL_DATA_V1, USN_JOURNAL_DATA_V0};

    use super::READ_REQUEST_LEN;

    #[test]
    fn usn_struct_layout_should_match_request_and_parser_offsets_when_built_for_windows() {
        assert_eq!(
            size_of::<READ_USN_JOURNAL_DATA_V1>(),
            READ_REQUEST_LEN,
            "READ_USN_JOURNAL_DATA_V1 size differs from the request buffer length"
        );
        assert_eq!(
            size_of::<USN_JOURNAL_DATA_V0>(),
            56,
            "USN_JOURNAL_DATA_V0 size differs from the 56 bytes the parser expects"
        );
        assert_eq!(
            offset_of!(READ_USN_JOURNAL_DATA_V1, StartUsn),
            0,
            "StartUsn offset differs"
        );
        assert_eq!(
            offset_of!(READ_USN_JOURNAL_DATA_V1, ReasonMask),
            8,
            "ReasonMask offset differs"
        );
        assert_eq!(
            offset_of!(READ_USN_JOURNAL_DATA_V1, UsnJournalID),
            32,
            "UsnJournalID offset differs"
        );
        assert_eq!(
            offset_of!(READ_USN_JOURNAL_DATA_V1, MinMajorVersion),
            40,
            "MinMajorVersion offset differs"
        );
        assert_eq!(
            offset_of!(READ_USN_JOURNAL_DATA_V1, MaxMajorVersion),
            42,
            "MaxMajorVersion offset differs"
        );
        assert_eq!(
            offset_of!(USN_JOURNAL_DATA_V0, UsnJournalID),
            0,
            "UsnJournalID offset differs"
        );
        assert_eq!(
            offset_of!(USN_JOURNAL_DATA_V0, FirstUsn),
            8,
            "FirstUsn offset differs"
        );
        assert_eq!(
            offset_of!(USN_JOURNAL_DATA_V0, NextUsn),
            16,
            "NextUsn offset differs"
        );
        assert_eq!(
            offset_of!(USN_JOURNAL_DATA_V0, LowestValidUsn),
            24,
            "LowestValidUsn offset differs"
        );
    }
}
