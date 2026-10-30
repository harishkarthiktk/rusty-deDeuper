//! Pure parsing of the seek-penalty reply. Compiled on every OS so it is unit-tested everywhere.

/// `sizeof(DEVICE_SEEK_PENALTY_DESCRIPTOR)`: `Version` and `Size` (u32 each),
/// `IncursSeekPenalty` (u8), then 3 bytes of padding.
pub(crate) const DESCRIPTOR_LEN: usize = 12;
const INCURS_SEEK_PENALTY_OFFSET: usize = 8;

/// `Some(true)` when the device incurs a seek penalty (rotational), `Some(false)` when
/// it does not. `None` when the kernel reported fewer bytes than the descriptor, or the
/// buffer cannot hold them, so a truncated reply is never read as a valid answer.
pub(crate) fn interpret_seek_penalty(buf: &[u8], returned: usize) -> Option<bool> {
    if returned < DESCRIPTOR_LEN {
        return None;
    }
    let descriptor = buf.get(..DESCRIPTOR_LEN)?;
    let incurs = descriptor.get(INCURS_SEEK_PENALTY_OFFSET)?;
    Some(*incurs != 0)
}

#[cfg(test)]
mod tests {
    use super::interpret_seek_penalty;

    /// A `DEVICE_SEEK_PENALTY_DESCRIPTOR` reply: Version 1, Size 12, then the flag byte.
    fn seek_penalty_bytes(incurs_seek_penalty: u8) -> [u8; 12] {
        let mut buf = [0u8; 12];
        buf[0..4].copy_from_slice(&1u32.to_le_bytes());
        buf[4..8].copy_from_slice(&12u32.to_le_bytes());
        buf[8] = incurs_seek_penalty;
        buf
    }

    #[test]
    fn interpret_seek_penalty_should_return_some_true_when_flag_set() {
        let buf = seek_penalty_bytes(1);
        let parsed = interpret_seek_penalty(&buf, buf.len());
        assert_eq!(
            parsed,
            Some(true),
            "IncursSeekPenalty 1 should parse to Some(true), got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_some_false_when_flag_clear() {
        let buf = seek_penalty_bytes(0);
        let parsed = interpret_seek_penalty(&buf, buf.len());
        assert_eq!(
            parsed,
            Some(false),
            "IncursSeekPenalty 0 should parse to Some(false), got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_none_when_buffer_too_short() {
        let buf = [0u8; 4];
        let parsed = interpret_seek_penalty(&buf, buf.len());
        assert_eq!(
            parsed, None,
            "a 4-byte buffer cannot hold the descriptor and should give None, got {parsed:?}"
        );
    }

    #[test]
    fn interpret_seek_penalty_should_return_none_when_returned_below_descriptor_length() {
        let buf = seek_penalty_bytes(1);
        let parsed = interpret_seek_penalty(&buf, 8);
        assert_eq!(
            parsed, None,
            "returned 8 bytes is a short reply and should give None even with the flag set, got {parsed:?}"
        );
    }
}
