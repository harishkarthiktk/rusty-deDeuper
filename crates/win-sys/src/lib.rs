//! Shared Win32 handle and path helpers. The only owner of Win32 FFI used by more than one crate.
//! Empty on non-Windows targets (apart from the pure `file_id_bytes`, which is unit-tested everywhere).

// `deny`, not `forbid`: the `ffi` module opts in to `unsafe` locally with `#![expect]`.
#![deny(unsafe_code)]

#[cfg(windows)]
mod ffi;
#[cfg(windows)]
pub use ffi::*;

/// Stable file identity from one attributes-only handle. 8 bytes when the 128-bit id is the
/// zero-extended 64-bit index (NTFS), so walker and `$MFT` ids stay equal; otherwise all 16
/// bytes (ReFS), never truncated. With no 128-bit id, the 64-bit index is used.
#[cfg(any(windows, test))]
pub fn file_id_bytes(id128: Option<&[u8; 16]>, index: u64) -> Vec<u8> {
    let low = index.to_le_bytes();
    match id128 {
        Some(id) if id[8..] == [0u8; 8] && id[..8] == low => low.to_vec(),
        Some(id) => id.to_vec(),
        None => low.to_vec(),
    }
}

#[cfg(test)]
mod file_id_tests {
    use super::file_id_bytes;

    #[test]
    fn file_id_without_128_bit_id_uses_index_bytes() {
        assert_eq!(
            file_id_bytes(None, 0x0102),
            vec![0x02, 0x01, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn file_id_zero_extended_ntfs_id_is_8_bytes() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(file_id_bytes(Some(&id), 42), 42u64.to_le_bytes().to_vec());
    }

    #[test]
    fn file_id_with_high_bits_is_full_16_bytes() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&42u64.to_le_bytes());
        id[15] = 1;
        assert_eq!(file_id_bytes(Some(&id), 42), id.to_vec());
    }

    #[test]
    fn file_id_low_part_mismatch_is_full_16_bytes() {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(file_id_bytes(Some(&id), 42), id.to_vec());
    }
}
