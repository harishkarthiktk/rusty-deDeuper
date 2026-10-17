//! Reads one `$MFT` file record through the `ntfs` crate and maps it to [`RecordFacts`].
//!
//! Portable: the reader is any `Read + Seek`, so nothing here needs a Windows handle. This is
//! the only module that calls the `ntfs` crate's record API.
//!
//! Crate behavior this module depends on (ntfs 0.4.0):
//! - `Ntfs::file` fails with `InvalidFileSignature` for a record that was never written, and
//!   with `InvalidFileRecordNumber` for a number past the end of `$MFT`.
//! - The base-record reference is private to the crate, so it is read from the raw header.
//! - `NtfsStandardInformation::file_attributes` keeps only the flags the crate names. It drops
//!   the cloud RECALL bits, so the attribute word is read from the raw value.
//! - `NtfsFileName::namespace` calls `unwrap` on the namespace byte. The byte is checked first.

use std::error::Error;
use std::fmt;
use std::io::{self, Read, Seek, SeekFrom};

use ntfs::structured_values::NtfsFileName;
use ntfs::{
    Ntfs, NtfsAttribute, NtfsAttributeType, NtfsError, NtfsFile, NtfsFileFlags, NtfsReadSeek,
};

use crate::tree::{NameFact, NameSpace, RecordFacts};

/// `file_attributes` sits at offset 32 of the `$STANDARD_INFORMATION` value (NTFS 1.2 layout).
const STD_INFO_ATTRIBUTES_OFFSET: usize = 32;
/// Bytes of the `$STANDARD_INFORMATION` value read: up to and including `file_attributes`.
const STD_INFO_PREFIX_LEN: usize = STD_INFO_ATTRIBUTES_OFFSET + 4;
/// Offset of the namespace byte in a `$FILE_NAME` value. The name length is the byte before it.
const FILE_NAME_NAMESPACE_OFFSET: u64 = 65;
/// Offset of the base-record reference in the FILE record header. Zero for base records.
const FILE_RECORD_BASE_REF_OFFSET: u64 = 32;
/// `FILE_ATTRIBUTE_REPARSE_POINT`, the only attribute bit this module interprets.
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

/// Why one record could not be read. The walk counts these and skips the record.
#[derive(Debug)]
pub enum RecordError {
    /// The `ntfs` crate rejected the record: a fixup mismatch, an attribute that does not
    /// parse, or a record number past the end of `$MFT`.
    Ntfs(NtfsError),
    /// Reading the volume failed while reading this record.
    Io(io::Error),
    /// The record parsed, but breaks a layout rule the crate does not check. The reason names
    /// the rule.
    Corrupt(&'static str),
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ntfs(err) => write!(f, "ntfs: {err}"),
            Self::Io(err) => write!(f, "io: {err}"),
            Self::Corrupt(reason) => write!(f, "corrupt record: {reason}"),
        }
    }
}

impl Error for RecordError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Ntfs(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Corrupt(_) => None,
        }
    }
}

impl From<NtfsError> for RecordError {
    /// The crate wraps read failures as `NtfsError::Io`. They are reported as I/O, not as
    /// format errors, so a failing volume is not mistaken for a malformed record.
    fn from(err: NtfsError) -> Self {
        match err {
            NtfsError::Io(io_err) => Self::Io(io_err),
            other => Self::Ntfs(other),
        }
    }
}

impl From<io::Error> for RecordError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

// The walk keeps `RecordError` in its error chain. A non-`Send` error would stop the caller
// from moving the walk to another thread later.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync + 'static>() {}
    assert_send_sync::<RecordError>();
};

/// Reads record `record` and returns its facts.
///
/// Returns `Ok(None)` for three kinds of record that are not files on their own:
/// - a record that was never written (no `FILE` signature), which the crate reports as an error
///   and this function treats as free space;
/// - a record whose IN_USE flag is clear (deleted, or free);
/// - an extension record, whose base-record reference is non-zero. Its attributes belong to
///   the base record, which is the one that is yielded.
///
/// Every other failure is an `Err`, and the caller decides whether it is fatal.
pub fn extract_record<T: Read + Seek>(
    ntfs: &Ntfs,
    fs: &mut T,
    record: u64,
) -> Result<Option<RecordFacts>, RecordError> {
    let file = match ntfs.file(fs, record) {
        Ok(file) => file,
        Err(NtfsError::InvalidFileSignature { .. }) => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    if !file.flags().contains(NtfsFileFlags::IN_USE) {
        return Ok(None);
    }
    if is_extension_record(base_reference_of(&file, fs)?) {
        return Ok(None);
    }

    let info = file.info()?;
    let is_dir = file.is_directory();
    let attrs = standard_information_attributes(&file, fs)?;
    let reparse_tag = if (attrs & FILE_ATTRIBUTE_REPARSE_POINT) != 0 {
        reparse_tag_of(&file, fs)
    } else {
        None
    };
    let size = if is_dir {
        0
    } else {
        unnamed_data_size(&file, fs)?
    };

    Ok(Some(RecordFacts {
        record,
        sequence: file.sequence_number(),
        in_use: true,
        is_dir,
        attrs,
        reparse_tag,
        mtime_ft: info.modification_time().nt_timestamp(),
        ctime_ft: info.creation_time().nt_timestamp(),
        size,
        names: file_names(&file, fs)?,
    }))
}

/// Maps the raw `$FILE_NAME` namespace byte. Values outside 0..=3 are corrupt.
fn namespace_from_byte(byte: u8) -> Option<NameSpace> {
    match byte {
        0 => Some(NameSpace::Posix),
        1 => Some(NameSpace::Win32),
        2 => Some(NameSpace::Dos),
        3 => Some(NameSpace::Win32AndDos),
        _ => None,
    }
}

/// The reparse tag is the first little-endian `u32` of the `$REPARSE_POINT` value.
fn parse_reparse_tag(value: &[u8]) -> Option<u32> {
    le_u32_at(value, 0)
}

/// The raw `file_attributes` word of a `$STANDARD_INFORMATION` value.
fn raw_file_attributes(value: &[u8]) -> Option<u32> {
    le_u32_at(value, STD_INFO_ATTRIBUTES_OFFSET)
}

/// A non-zero base-record reference marks an extension record.
fn is_extension_record(base_reference: u64) -> bool {
    base_reference != 0
}

fn le_u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    let word: [u8; 4] = bytes.get(offset..end)?.try_into().ok()?;
    Some(u32::from_le_bytes(word))
}

/// Reads the base-record reference from the raw FILE header.
///
/// The crate does not expose this field. The offset is in the first 512-byte sector, clear of
/// the update-sequence bytes at the end of each sector, so the raw bytes are valid without
/// fixups.
fn base_reference_of<T: Read + Seek>(file: &NtfsFile<'_>, fs: &mut T) -> Result<u64, RecordError> {
    let position = file
        .position()
        .value()
        .ok_or(RecordError::Corrupt("record has no position"))?;
    let offset = position
        .get()
        .checked_add(FILE_RECORD_BASE_REF_OFFSET)
        .ok_or(RecordError::Corrupt("record offset overflows"))?;
    fs.seek(SeekFrom::Start(offset))?;
    let mut raw = [0u8; 8];
    fs.read_exact(&mut raw)?;
    Ok(u64::from_le_bytes(raw))
}

/// Returns the raw `file_attributes` word from the top-level `$STANDARD_INFORMATION`.
///
/// The crate's own flag type truncates unknown bits, so the raw word is read instead.
fn standard_information_attributes<T: Read + Seek>(
    file: &NtfsFile<'_>,
    fs: &mut T,
) -> Result<u32, RecordError> {
    for attribute in file.attributes_raw() {
        let attribute = attribute?;
        if attribute.ty()? != NtfsAttributeType::StandardInformation {
            continue;
        }
        let mut value = attribute.value(fs)?;
        let mut prefix = [0u8; STD_INFO_PREFIX_LEN];
        value.read_exact(fs, &mut prefix)?;
        return raw_file_attributes(&prefix)
            .ok_or(RecordError::Corrupt("$STANDARD_INFORMATION is too short"));
    }
    Err(RecordError::Corrupt("record has no $STANDARD_INFORMATION"))
}

/// Returns the first `$REPARSE_POINT` tag, or `None` if it cannot be read.
///
/// A failed read is not fatal: the caller keeps the reparse bit in `attrs`, so the record is
/// still treated as a reparse point.
fn reparse_tag_of<T: Read + Seek>(file: &NtfsFile<'_>, fs: &mut T) -> Option<u32> {
    let mut attributes = file.attributes();
    while let Some(Ok(item)) = attributes.next(fs) {
        let Ok(attribute) = item.to_attribute() else {
            continue;
        };
        if attribute.ty().ok() != Some(NtfsAttributeType::ReparsePoint) {
            continue;
        }
        let mut value = attribute.value(fs).ok()?;
        let mut head = [0u8; 4];
        value.read_exact(fs, &mut head).ok()?;
        return parse_reparse_tag(&head);
    }
    None
}

/// Size of the unnamed `$DATA` stream. A record without one (a directory, or an empty file
/// with no data attribute) has size 0.
fn unnamed_data_size<T: Read + Seek>(file: &NtfsFile<'_>, fs: &mut T) -> Result<u64, RecordError> {
    let Some(item) = file.data(fs, "") else {
        return Ok(0);
    };
    Ok(item?.to_attribute()?.value_length())
}

/// Every `$FILE_NAME` of the file, one per hard link and namespace. Names are kept as UTF-16.
fn file_names<T: Read + Seek>(
    file: &NtfsFile<'_>,
    fs: &mut T,
) -> Result<Vec<NameFact>, RecordError> {
    let mut names = Vec::new();
    let mut attributes = file.attributes();
    while let Some(item) = attributes.next(fs) {
        let item = item?;
        let attribute = item.to_attribute()?;
        if attribute.ty()? != NtfsAttributeType::FileName {
            continue;
        }
        let namespace = file_name_namespace(&attribute, fs)?;
        let name = attribute.structured_value::<_, NtfsFileName>(fs)?;
        names.push(NameFact {
            parent: name.parent_directory_reference().file_record_number(),
            name: name.name().u16_iter().collect(),
            namespace,
        });
    }
    Ok(names)
}

/// Reads the namespace byte before the crate parses the name, because the crate's
/// `NtfsFileName::namespace` panics on an unknown value.
fn file_name_namespace<T: Read + Seek>(
    attribute: &NtfsAttribute<'_, '_>,
    fs: &mut T,
) -> Result<NameSpace, RecordError> {
    let mut value = attribute.value(fs)?;
    value.seek(fs, SeekFrom::Start(FILE_NAME_NAMESPACE_OFFSET))?;
    let mut byte = [0u8; 1];
    value.read_exact(fs, &mut byte)?;
    namespace_from_byte(byte[0]).ok_or(RecordError::Corrupt("unknown $FILE_NAME namespace"))
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn namespace_from_byte_should_return_posix_when_byte_is_zero() {
        assert_eq!(
            namespace_from_byte(0),
            Some(NameSpace::Posix),
            "byte 0 is the POSIX namespace"
        );
    }

    #[test]
    fn namespace_from_byte_should_return_win32_when_byte_is_one() {
        assert_eq!(
            namespace_from_byte(1),
            Some(NameSpace::Win32),
            "byte 1 is the Win32 namespace"
        );
    }

    #[test]
    fn namespace_from_byte_should_return_dos_when_byte_is_two() {
        assert_eq!(
            namespace_from_byte(2),
            Some(NameSpace::Dos),
            "byte 2 is the DOS 8.3 namespace"
        );
    }

    #[test]
    fn namespace_from_byte_should_return_win32_and_dos_when_byte_is_three() {
        assert_eq!(
            namespace_from_byte(3),
            Some(NameSpace::Win32AndDos),
            "byte 3 is the combined Win32 and DOS namespace"
        );
    }

    #[test]
    fn namespace_from_byte_should_return_none_when_byte_is_unknown() {
        assert_eq!(
            namespace_from_byte(4),
            None,
            "byte 4 is not a namespace and must be rejected before the crate panics on it"
        );
    }

    #[test]
    fn parse_reparse_tag_should_read_little_endian_when_value_has_four_bytes() {
        // IO_REPARSE_TAG_SYMLINK = 0xA000_000C, stored little-endian.
        assert_eq!(
            parse_reparse_tag(&[0x0C, 0x00, 0x00, 0xA0]),
            Some(0xA000_000C),
            "the tag must be the first four bytes read little-endian"
        );
    }

    #[test]
    fn parse_reparse_tag_should_return_none_when_value_shorter_than_four() {
        assert_eq!(
            parse_reparse_tag(&[0x0C, 0x00, 0x00]),
            None,
            "a value shorter than the tag cannot yield a tag"
        );
    }

    #[test]
    fn parse_reparse_tag_should_ignore_trailing_bytes_when_value_is_longer() {
        assert_eq!(
            parse_reparse_tag(&[0x0C, 0x00, 0x00, 0xA0, 0xFF, 0xFF]),
            Some(0xA000_000C),
            "bytes after the tag belong to the reparse data and must not change the tag"
        );
    }

    #[test]
    fn raw_file_attributes_should_read_offset_32_when_value_is_full_prefix() {
        let mut value = [0u8; STD_INFO_PREFIX_LEN];
        // Reparse point plus RECALL_ON_DATA_ACCESS (0x0040_0000), a bit the crate's flag type drops.
        value[32..36].copy_from_slice(&0x0040_0400u32.to_le_bytes());
        assert_eq!(
            raw_file_attributes(&value),
            Some(0x0040_0400),
            "file_attributes must be read from offset 32 without truncating unknown bits"
        );
    }

    #[test]
    fn raw_file_attributes_should_return_none_when_value_too_short() {
        assert_eq!(
            raw_file_attributes(&[0u8; 35]),
            None,
            "a value that ends before file_attributes must yield None"
        );
    }

    #[test]
    fn is_extension_record_should_return_false_when_base_reference_is_zero() {
        assert!(
            !is_extension_record(0),
            "a zero base reference marks a base record, which is a file"
        );
    }

    #[test]
    fn is_extension_record_should_return_true_when_base_reference_is_set() {
        assert!(
            is_extension_record(0x0002_0000_0000_0005),
            "a non-zero base reference marks an extension record"
        );
    }

    #[test]
    fn record_error_from_should_map_io_when_ntfs_wraps_io_error() {
        let err = RecordError::from(NtfsError::Io(io::Error::other("disk gone")));
        assert!(
            matches!(err, RecordError::Io(_)),
            "a read failure wrapped by the crate must surface as Io, got {err}"
        );
    }

    #[test]
    fn record_error_display_should_name_corrupt_when_variant_is_corrupt() {
        let text = RecordError::Corrupt("test reason").to_string();
        assert!(
            text.contains("corrupt record") && text.contains("test reason"),
            "the display text must name the corruption and its reason, got {text:?}"
        );
    }
}
