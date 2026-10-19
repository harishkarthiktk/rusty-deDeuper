//! Raw NTFS volume access: `\\.\X:` reads, the volume's NTFS geometry, and the file reference
//! of a path.
//!
//! The byte-level helpers ([`raw_volume_path`], [`geometry_from_volume_data`],
//! [`record_number_of`]) are portable and tested on every OS. The Win32 items are gated to
//! Windows and live in a private module that is the only place `unsafe` appears in this file.

use crate::mft_walk::VolumeGeometry;

/// Size of `NTFS_VOLUME_DATA_BUFFER`. A Windows-only test checks it against the windows-sys struct.
const VOLUME_DATA_LEN: usize = 96;
/// Field offsets inside `NTFS_VOLUME_DATA_BUFFER`, little-endian. The Windows-only layout test
/// checks each one against the windows-sys struct, so a drift fails the Windows build.
const OFFSET_VOLUME_SERIAL: usize = 0;
const OFFSET_TOTAL_CLUSTERS: usize = 16;
const OFFSET_BYTES_PER_SECTOR: usize = 40;
const OFFSET_BYTES_PER_CLUSTER: usize = 44;
const OFFSET_BYTES_PER_RECORD: usize = 48;
const OFFSET_MFT_VALID_LENGTH: usize = 56;

/// A file reference is a 48-bit record number in the low bits and a 16-bit reuse count above it.
const RECORD_NUMBER_MASK: u64 = (1 << 48) - 1;

/// Converts a volume root such as `C:\`, `C:`, or `\\?\Volume{GUID}\` into the raw device name
/// `\\.\C:` or `\\.\Volume{GUID}`.
///
/// The result is UTF-16 with a trailing NUL, the form `CreateFileW` takes. Returns `None` for
/// anything that is not exactly a drive or volume-GUID root, including unpaired surrogates.
pub fn raw_volume_path(volume_root: &[u16]) -> Option<Vec<u16>> {
    let root = String::from_utf16(volume_root).ok()?;
    let device = device_name(root.trim_end_matches('\\'))?;
    Some(device.encode_utf16().chain(std::iter::once(0)).collect())
}

/// Device name for a root whose trailing backslashes are already removed.
fn device_name(root: &str) -> Option<String> {
    // A canonicalized scan root arrives as `\\?\C:\`, so accept the verbatim prefix too.
    let bare = root.strip_prefix(r"\\?\").unwrap_or(root);
    if let Some(letter) = drive_letter(bare) {
        return Some(format!(r"\\.\{letter}:"));
    }
    let guid = root.strip_prefix(r"\\?\Volume{")?.strip_suffix('}')?;
    is_guid(guid).then(|| format!(r"\\.\Volume{{{guid}}}"))
}

/// The drive letter of `C:`, uppercased. Any other shape is `None`.
fn drive_letter(root: &str) -> Option<char> {
    match root.as_bytes() {
        [letter, b':'] if letter.is_ascii_alphabetic() => {
            Some(char::from(letter.to_ascii_uppercase()))
        }
        _ => None,
    }
}

/// The canonical `8-4-4-4-12` hex form, without braces.
fn is_guid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// `\\?\Volume{GUID}\rest` -> `\\?\Volume{GUID}\`. `None` for any other shape.
pub fn guid_volume_prefix(final_path: &[u16]) -> Option<Vec<u16>> {
    const HEAD: &str = r"\\?\Volume{";
    // `\\?\Volume{` + 36 GUID characters + `}`.
    let prefix_len = HEAD.len() + 36 + 1;
    let head = String::from_utf16(final_path.get(..prefix_len)?).ok()?;
    let guid = head.strip_prefix(HEAD)?.strip_suffix('}')?;
    is_guid(guid).then(|| format!("{head}\\").encode_utf16().collect())
}

/// Parses an `NTFS_VOLUME_DATA_BUFFER` image into the geometry the MFT walk needs.
///
/// Returns `None` when the buffer is shorter than the struct, when a size field is zero, or when
/// the total byte count overflows. A zero size would make every offset computed from it wrong.
pub fn geometry_from_volume_data(buf: &[u8]) -> Option<VolumeGeometry> {
    if buf.len() < VOLUME_DATA_LEN {
        return None;
    }
    let bytes_per_sector = u64::from(read_u32(buf, OFFSET_BYTES_PER_SECTOR)?);
    let bytes_per_cluster = u64::from(read_u32(buf, OFFSET_BYTES_PER_CLUSTER)?);
    let bytes_per_record = u64::from(read_u32(buf, OFFSET_BYTES_PER_RECORD)?);
    let total_clusters = u64::try_from(read_i64(buf, OFFSET_TOTAL_CLUSTERS)?).ok()?;
    let mft_valid_length = u64::try_from(read_i64(buf, OFFSET_MFT_VALID_LENGTH)?).ok()?;
    if bytes_per_sector == 0 || bytes_per_cluster == 0 || bytes_per_record == 0 {
        return None;
    }
    Some(VolumeGeometry {
        bytes_per_sector,
        bytes_per_cluster,
        bytes_per_record,
        mft_valid_length,
        total_bytes: total_clusters.checked_mul(bytes_per_cluster)?,
    })
}

/// The NTFS volume serial number from an `NTFS_VOLUME_DATA_BUFFER` image, as its raw bits.
///
/// Returns `None` when the buffer is shorter than the struct. The field is signed on the wire,
/// so a negative value keeps its bit pattern.
pub fn volume_serial_from_volume_data(buf: &[u8]) -> Option<u64> {
    if buf.len() < VOLUME_DATA_LEN {
        return None;
    }
    // The serial is an opaque 64-bit id, so reinterpret the bits as unsigned.
    read_i64(buf, OFFSET_VOLUME_SERIAL).map(|serial| serial as u64)
}

/// The record number inside a file reference, with the reuse count removed.
pub fn record_number_of(frn: u64) -> u64 {
    frn & RECORD_NUMBER_MASK
}

fn read_u32(buf: &[u8], offset: usize) -> Option<u32> {
    buf.get(offset..)?
        .first_chunk::<4>()
        .map(|bytes| u32::from_le_bytes(*bytes))
}

fn read_i64(buf: &[u8], offset: usize) -> Option<i64> {
    buf.get(offset..)?
        .first_chunk::<8>()
        .map(|bytes| i64::from_le_bytes(*bytes))
}

#[cfg(windows)]
pub use win32::{
    frn_of, ntfs_volume_data, ntfs_volume_serial, open_raw_volume, resolve_root, RawVolume,
    ResolvedRoot,
};

#[cfg(windows)]
mod win32 {
    #![expect(
        unsafe_code,
        reason = "raw volume and file-id Win32 calls; each unsafe block states its invariants"
    )]

    use std::io::{self, Read, Seek, SeekFrom};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    use std::ptr::{from_mut, null, null_mut};

    use dedupe_backend_generic::win_sys::{open_handle, to_wide_nul, OwnedHandle};
    use windows_sys::Win32::Foundation::{FILETIME, GENERIC_READ};
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, GetFinalPathNameByHandleW, ReadFile, SetFilePointerEx,
        BY_HANDLE_FILE_INFORMATION, FILE_BEGIN, FILE_CURRENT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        VOLUME_NAME_GUID,
    };
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_GET_NTFS_VOLUME_DATA, GET_LENGTH_INFORMATION, IOCTL_DISK_GET_LENGTH_INFO,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    use super::{
        geometry_from_volume_data, guid_volume_prefix, raw_volume_path,
        volume_serial_from_volume_data, VOLUME_DATA_LEN,
    };
    use crate::mft_walk::VolumeGeometry;

    /// `GET_LENGTH_INFORMATION` is one little-endian i64. A `const` assert keeps the byte
    /// parsing below honest if the windows-sys struct ever changes.
    const LENGTH_INFO_LEN: usize = 8;
    const _: () = assert!(size_of::<GET_LENGTH_INFORMATION>() == LENGTH_INFO_LEN);

    const ZERO_FILETIME: FILETIME = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };

    /// A read-only raw volume handle with its length, closed on drop.
    ///
    /// Reads are positional and loop until the buffer is full or the volume ends. The
    /// sector-aligned contract is the caller's: wrap this in `SectorReader`, which only ever
    /// issues aligned reads.
    pub struct RawVolume {
        handle: OwnedHandle,
        len: u64,
    }

    /// Opens `volume_root` (such as `C:\`) as a raw device with read access only.
    ///
    /// Read access to a volume needs elevation in practice, so this fails for unelevated callers.
    pub fn open_raw_volume(volume_root: &Path) -> io::Result<RawVolume> {
        let root: Vec<u16> = volume_root.as_os_str().encode_wide().collect();
        let device = raw_volume_path(&root).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path is not a volume root")
        })?;
        // Sharing read and write, not delete, so the volume stays usable while it is read.
        let handle = open_handle(&device, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, 0)?;
        let len = volume_length(&handle)?;
        Ok(RawVolume { handle, len })
    }

    impl RawVolume {
        /// Runs a control code that takes `input` and writes into `out`, and returns how many
        /// bytes the kernel wrote. Used for the USN journal controls, which take a request record.
        pub fn ioctl(&self, code: u32, input: &[u8], out: &mut [u8]) -> io::Result<usize> {
            ioctl_io(&self.handle, code, input, out)
        }
    }

    /// NTFS geometry of an open volume, from `FSCTL_GET_NTFS_VOLUME_DATA`.
    ///
    /// Fails for a volume that is not NTFS, because the control code is then rejected.
    pub fn ntfs_volume_data(vol: &RawVolume) -> io::Result<VolumeGeometry> {
        let mut buf = [0u8; VOLUME_DATA_LEN];
        let written = ioctl_output(&vol.handle, FSCTL_GET_NTFS_VOLUME_DATA, &mut buf)?;
        geometry_from_volume_data(&buf[..written]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "NTFS volume data is missing or malformed",
            )
        })
    }

    /// The NTFS volume serial number, from `FSCTL_GET_NTFS_VOLUME_DATA`. It keys the cache file.
    pub fn ntfs_volume_serial(vol: &RawVolume) -> io::Result<u64> {
        let mut buf = [0u8; VOLUME_DATA_LEN];
        let written = ioctl_output(&vol.handle, FSCTL_GET_NTFS_VOLUME_DATA, &mut buf)?;
        volume_serial_from_volume_data(&buf[..written]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "NTFS volume data is missing or malformed",
            )
        })
    }

    /// `BY_HANDLE_FILE_INFORMATION` of an open handle.
    fn by_handle_info(handle: &OwnedHandle) -> io::Result<BY_HANDLE_FILE_INFORMATION> {
        let mut info = BY_HANDLE_FILE_INFORMATION {
            dwFileAttributes: 0,
            ftCreationTime: ZERO_FILETIME,
            ftLastAccessTime: ZERO_FILETIME,
            ftLastWriteTime: ZERO_FILETIME,
            dwVolumeSerialNumber: 0,
            nFileSizeHigh: 0,
            nFileSizeLow: 0,
            nNumberOfLinks: 0,
            nFileIndexHigh: 0,
            nFileIndexLow: 0,
        };
        // SAFETY: `handle` is a live handle the caller owns. `info` is a live, writable
        // `BY_HANDLE_FILE_INFORMATION`, and it is read only after the call reports success.
        let ok = unsafe { GetFileInformationByHandle(handle.raw(), &mut info) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info)
    }

    /// Opens `path` for attribute reads, following any link (like walkdir's root). Read
    /// attributes access needs no read rights, and backup semantics let the call open a
    /// directory, which a plain file open refuses.
    fn open_for_attributes(path: &Path) -> io::Result<OwnedHandle> {
        open_handle(
            &to_wide_nul(path.as_os_str()),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    }

    /// The 64-bit file reference (file index) of `path`. Directories are included.
    pub fn frn_of(path: &Path) -> io::Result<u64> {
        let info = by_handle_info(&open_for_attributes(path)?)?;
        Ok((u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow))
    }

    /// The scan root as the file system sees it after following any link.
    pub struct ResolvedRoot {
        /// File reference of the (followed) root directory.
        pub frn: u64,
        /// `dwVolumeSerialNumber` of the volume that really holds the root.
        pub serial_low: u32,
        /// `\\?\Volume{GUID}\`, the raw volume to read.
        pub volume_root: Vec<u16>,
    }

    /// Opens the root once, following links, and reads its file index, volume serial and
    /// GUID volume path from that one handle. The volume and the record number therefore
    /// always describe the same object, even when the root is a junction.
    pub fn resolve_root(path: &Path) -> io::Result<ResolvedRoot> {
        let handle = open_for_attributes(path)?;
        let info = by_handle_info(&handle)?;
        let mut buf = vec![0u16; 512];
        let len = loop {
            let cap = u32::try_from(buf.len()).map_err(io::Error::other)?;
            // SAFETY: `handle` is live; `buf` is writable for `cap` u16 units and that
            // count is passed.
            let n = unsafe {
                GetFinalPathNameByHandleW(handle.raw(), buf.as_mut_ptr(), cap, VOLUME_NAME_GUID)
            };
            if n == 0 {
                return Err(io::Error::last_os_error());
            }
            let n = usize::try_from(n).map_err(io::Error::other)?;
            if n < buf.len() {
                break n;
            }
            // `n` is the required size including the NUL.
            buf.resize(n + 1, 0);
        };
        let volume_root = guid_volume_prefix(&buf[..len]).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "final path has no volume GUID prefix",
            )
        })?;
        Ok(ResolvedRoot {
            frn: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
            serial_low: info.dwVolumeSerialNumber,
            volume_root,
        })
    }

    /// Length of the volume in bytes, from `IOCTL_DISK_GET_LENGTH_INFO`.
    fn volume_length(handle: &OwnedHandle) -> io::Result<u64> {
        let mut info = [0u8; LENGTH_INFO_LEN];
        let written = ioctl_output(handle, IOCTL_DISK_GET_LENGTH_INFO, &mut info)?;
        if written != LENGTH_INFO_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "volume length reply has the wrong size",
            ));
        }
        u64::try_from(i64::from_le_bytes(info))
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "volume length is negative"))
    }

    /// Runs an output-only `DeviceIoControl` and returns how many bytes the kernel wrote.
    fn ioctl_output(handle: &OwnedHandle, code: u32, out: &mut [u8]) -> io::Result<usize> {
        let out_len = u32::try_from(out.len()).map_err(io::Error::other)?;
        let mut returned: u32 = 0;
        // SAFETY: `handle` is a live handle owned by the caller. No input buffer is passed
        // (null, 0), so the kernel reads nothing from our memory. `out` is writable for
        // `out_len` bytes and that exact length is passed, so the kernel cannot write past
        // it. `returned` is a live u32 out-parameter. The overlapped pointer is null, so the
        // call is synchronous and no pointer outlives it.
        let ok = unsafe {
            DeviceIoControl(
                handle.raw(),
                code,
                null(),
                0,
                out.as_mut_ptr().cast(),
                out_len,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let written = usize::try_from(returned).map_err(io::Error::other)?;
        Ok(written.min(out.len()))
    }

    /// Runs a `DeviceIoControl` that takes an input record and returns how many bytes the kernel
    /// wrote. An empty `input` is passed as a null pointer with length 0.
    fn ioctl_io(
        handle: &OwnedHandle,
        code: u32,
        input: &[u8],
        out: &mut [u8],
    ) -> io::Result<usize> {
        let in_len = u32::try_from(input.len()).map_err(io::Error::other)?;
        let in_ptr = if input.is_empty() {
            null()
        } else {
            input.as_ptr().cast()
        };
        let out_len = u32::try_from(out.len()).map_err(io::Error::other)?;
        let mut returned: u32 = 0;
        // SAFETY: `handle` is a live handle owned by the caller. `input` is readable for
        // `in_len` bytes, and that exact length is passed, so the kernel reads only inside it
        // (an empty input is passed as null with length 0, so nothing is read). `out` is
        // writable for `out_len` bytes and that exact length is passed, so the kernel cannot
        // write past it. `returned` is a live u32 out-parameter. The overlapped pointer is null,
        // so the call is synchronous and no pointer outlives it.
        let ok = unsafe {
            DeviceIoControl(
                handle.raw(),
                code,
                in_ptr,
                in_len,
                out.as_mut_ptr().cast(),
                out_len,
                &mut returned,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        let written = usize::try_from(returned).map_err(io::Error::other)?;
        Ok(written.min(out.len()))
    }

    impl Read for RawVolume {
        /// Fills `buf` completely unless the volume ends first. A short count means end of
        /// volume, never a short read, because `SectorReader` assumes full reads.
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut filled = 0;
            while filled < buf.len() {
                let chunk = &mut buf[filled..];
                let want = u32::try_from(chunk.len()).unwrap_or(u32::MAX);
                let mut got: u32 = 0;
                // SAFETY: `self.handle` is live and owned by `self`. `chunk` is writable for
                // `want` bytes, because `want` is at most `chunk.len()`. `got` is a live u32
                // out-parameter. The overlapped pointer is null, so the call is synchronous.
                let ok = unsafe {
                    ReadFile(
                        self.handle.raw(),
                        chunk.as_mut_ptr(),
                        want,
                        &mut got,
                        null_mut(),
                    )
                };
                if ok == 0 {
                    return Err(io::Error::last_os_error());
                }
                if got == 0 {
                    break;
                }
                // Clamp so a count above the buffer can never make `filled` run past it.
                let n = usize::try_from(got)
                    .map_err(io::Error::other)?
                    .min(chunk.len());
                filled += n;
            }
            Ok(filled)
        }
    }

    impl Seek for RawVolume {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            let (method, distance) = match from {
                SeekFrom::Start(offset) => {
                    (FILE_BEGIN, i64::try_from(offset).map_err(io::Error::other)?)
                }
                SeekFrom::Current(delta) => (FILE_CURRENT, delta),
                SeekFrom::End(delta) => {
                    // Windows cannot seek from the end of a raw volume, so resolve it here
                    // against the length read at open.
                    let len = i64::try_from(self.len).map_err(io::Error::other)?;
                    let target = len
                        .checked_add(delta)
                        .filter(|target| *target >= 0)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "seek before the start of the volume",
                            )
                        })?;
                    (FILE_BEGIN, target)
                }
            };
            let mut new_pos: i64 = 0;
            // SAFETY: `self.handle` is live and owned by `self`. `new_pos` is a live i64
            // out-parameter that the call writes on success. `method` is one of the documented
            // FILE_BEGIN and FILE_CURRENT constants.
            let ok = unsafe {
                SetFilePointerEx(self.handle.raw(), distance, from_mut(&mut new_pos), method)
            };
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            u64::try_from(new_pos).map_err(io::Error::other)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nul_wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn put(buf: &mut [u8], offset: usize, bytes: &[u8]) {
        buf[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    /// A `NTFS_VOLUME_DATA_BUFFER` image holding only the fields the parser reads.
    fn volume_data_image(
        sector: u32,
        cluster: u32,
        record: u32,
        total_clusters: i64,
        mft_valid: i64,
    ) -> Vec<u8> {
        let mut buf = vec![0u8; VOLUME_DATA_LEN];
        put(
            &mut buf,
            OFFSET_TOTAL_CLUSTERS,
            &total_clusters.to_le_bytes(),
        );
        put(&mut buf, OFFSET_BYTES_PER_SECTOR, &sector.to_le_bytes());
        put(&mut buf, OFFSET_BYTES_PER_CLUSTER, &cluster.to_le_bytes());
        put(&mut buf, OFFSET_BYTES_PER_RECORD, &record.to_le_bytes());
        put(&mut buf, OFFSET_MFT_VALID_LENGTH, &mft_valid.to_le_bytes());
        buf
    }

    #[test]
    fn raw_volume_path_should_return_device_name_when_root_has_trailing_backslash() {
        assert_eq!(
            raw_volume_path(&wide("C:\\")),
            Some(nul_wide("\\\\.\\C:")),
            "C:\\ should map to \\\\.\\C:"
        );
    }

    #[test]
    fn raw_volume_path_should_return_device_name_when_root_has_verbatim_prefix() {
        assert_eq!(
            raw_volume_path(&wide("\\\\?\\C:\\")),
            Some(nul_wide("\\\\.\\C:")),
            "\\\\?\\C:\\ should map to \\\\.\\C:"
        );
    }

    #[test]
    fn raw_volume_path_should_return_device_name_when_root_has_no_backslash() {
        assert_eq!(
            raw_volume_path(&wide("C:")),
            Some(nul_wide("\\\\.\\C:")),
            "C: should map to \\\\.\\C:"
        );
    }

    #[test]
    fn raw_volume_path_should_uppercase_drive_letter_when_lowercase() {
        assert_eq!(
            raw_volume_path(&wide("d:\\")),
            Some(nul_wide("\\\\.\\D:")),
            "d:\\ should map to \\\\.\\D:"
        );
    }

    #[test]
    fn raw_volume_path_should_strip_prefix_when_root_is_volume_guid() {
        assert_eq!(
            raw_volume_path(&wide(
                "\\\\?\\Volume{12345678-1234-1234-1234-1234567890ab}\\"
            )),
            Some(nul_wide(
                "\\\\.\\Volume{12345678-1234-1234-1234-1234567890ab}"
            )),
            "a volume GUID root should map to the raw device name without the trailing backslash"
        );
    }

    #[test]
    fn raw_volume_path_should_return_none_when_root_is_not_a_volume() {
        assert_eq!(
            raw_volume_path(&wide("C:\\Users")),
            None,
            "a directory path is not a volume root"
        );
    }

    #[test]
    fn raw_volume_path_should_return_none_when_input_is_empty() {
        assert_eq!(raw_volume_path(&[]), None, "an empty root is not a volume");
    }

    #[test]
    fn raw_volume_path_should_return_none_when_guid_is_malformed() {
        assert_eq!(
            raw_volume_path(&wide("\\\\?\\Volume{not-a-guid}\\")),
            None,
            "a GUID that is not 8-4-4-4-12 hex is rejected"
        );
    }

    #[test]
    fn raw_volume_path_should_return_none_when_input_has_unpaired_surrogate() {
        assert_eq!(
            raw_volume_path(&[0xD800]),
            None,
            "an unpaired surrogate cannot form a root"
        );
    }

    #[test]
    fn geometry_from_volume_data_should_parse_fields_when_buffer_is_complete() {
        let buf = volume_data_image(512, 4096, 1024, 1000, 40960);
        assert_eq!(
            geometry_from_volume_data(&buf),
            Some(VolumeGeometry {
                bytes_per_sector: 512,
                bytes_per_cluster: 4096,
                bytes_per_record: 1024,
                mft_valid_length: 40960,
                total_bytes: 4_096_000,
            }),
            "a well-formed buffer should parse to the geometry it describes"
        );
    }

    #[test]
    fn geometry_from_volume_data_should_return_none_when_buffer_is_short() {
        let buf = volume_data_image(512, 4096, 1024, 1000, 40960);
        assert_eq!(
            geometry_from_volume_data(&buf[..VOLUME_DATA_LEN - 1]),
            None,
            "one byte short of the struct must not be parsed"
        );
    }

    #[test]
    fn geometry_from_volume_data_should_return_none_when_cluster_size_is_zero() {
        let buf = volume_data_image(512, 0, 1024, 1000, 40960);
        assert_eq!(
            geometry_from_volume_data(&buf),
            None,
            "a zero cluster size would make every byte total zero"
        );
    }

    #[test]
    fn geometry_from_volume_data_should_return_none_when_record_size_is_zero() {
        let buf = volume_data_image(512, 4096, 0, 1000, 40960);
        assert_eq!(
            geometry_from_volume_data(&buf),
            None,
            "a zero record size would make the record count undefined"
        );
    }

    #[test]
    fn geometry_from_volume_data_should_return_none_when_total_bytes_overflow() {
        let buf = volume_data_image(512, 4096, 1024, i64::MAX, 40960);
        assert_eq!(
            geometry_from_volume_data(&buf),
            None,
            "clusters times cluster size must not wrap"
        );
    }

    #[test]
    fn record_number_of_should_drop_sequence_bits_when_sequence_is_set() {
        assert_eq!(
            record_number_of(0x0001_0000_0000_002A),
            42,
            "the high 16 bits are the sequence number and must be removed"
        );
    }

    #[test]
    fn record_number_of_should_keep_record_when_sequence_is_zero() {
        assert_eq!(
            record_number_of(42),
            42,
            "a reference with no sequence bits is its own record number"
        );
    }

    #[test]
    fn record_number_of_should_keep_all_48_bits_when_record_is_maximal() {
        assert_eq!(
            record_number_of(0xFFFF_FFFF_FFFF_FFFF),
            0x0000_FFFF_FFFF_FFFF,
            "the largest record number is 48 bits and must survive the mask"
        );
    }

    #[test]
    fn volume_serial_from_volume_data_should_read_offset_zero_when_buffer_long_enough() {
        let mut buf = volume_data_image(512, 4096, 1024, 1000, 40960);
        put(&mut buf, OFFSET_VOLUME_SERIAL, &(-2i64).to_le_bytes());
        assert_eq!(
            volume_serial_from_volume_data(&buf),
            Some(u64::MAX - 1),
            "the serial is the i64 at offset 0, and a negative value keeps its bit pattern"
        );
    }

    #[test]
    fn volume_serial_from_volume_data_should_return_none_when_buffer_short() {
        let buf = volume_data_image(512, 4096, 1024, 1000, 40960);
        assert_eq!(
            volume_serial_from_volume_data(&buf[..VOLUME_DATA_LEN - 1]),
            None,
            "one byte short of the struct must not be parsed"
        );
    }
}

/// Layout check for the Win32 struct. It compiles only for Windows, so `cargo check --target
/// x86_64-pc-windows-gnu --all-targets` verifies the offsets without running anything.
#[cfg(all(test, windows))]
mod layout_tests {
    use std::mem::{offset_of, size_of};

    use windows_sys::Win32::System::Ioctl::NTFS_VOLUME_DATA_BUFFER;

    use super::{
        OFFSET_BYTES_PER_CLUSTER, OFFSET_BYTES_PER_RECORD, OFFSET_BYTES_PER_SECTOR,
        OFFSET_MFT_VALID_LENGTH, OFFSET_TOTAL_CLUSTERS, OFFSET_VOLUME_SERIAL, VOLUME_DATA_LEN,
    };

    #[test]
    fn ntfs_volume_data_layout_should_match_parser_offsets_when_built_for_windows() {
        assert_eq!(
            size_of::<NTFS_VOLUME_DATA_BUFFER>(),
            VOLUME_DATA_LEN,
            "struct size differs from the parser's buffer length"
        );
        assert_eq!(
            offset_of!(NTFS_VOLUME_DATA_BUFFER, VolumeSerialNumber),
            OFFSET_VOLUME_SERIAL,
            "VolumeSerialNumber offset differs"
        );
        assert_eq!(
            offset_of!(NTFS_VOLUME_DATA_BUFFER, TotalClusters),
            OFFSET_TOTAL_CLUSTERS,
            "TotalClusters offset differs"
        );
        assert_eq!(
            offset_of!(NTFS_VOLUME_DATA_BUFFER, BytesPerSector),
            OFFSET_BYTES_PER_SECTOR,
            "BytesPerSector offset differs"
        );
        assert_eq!(
            offset_of!(NTFS_VOLUME_DATA_BUFFER, BytesPerCluster),
            OFFSET_BYTES_PER_CLUSTER,
            "BytesPerCluster offset differs"
        );
        assert_eq!(
            offset_of!(NTFS_VOLUME_DATA_BUFFER, BytesPerFileRecordSegment),
            OFFSET_BYTES_PER_RECORD,
            "BytesPerFileRecordSegment offset differs"
        );
        assert_eq!(
            offset_of!(NTFS_VOLUME_DATA_BUFFER, MftValidDataLength),
            OFFSET_MFT_VALID_LENGTH,
            "MftValidDataLength offset differs"
        );
    }
}
