/// LCN of the first extent in a `RETRIEVAL_POINTERS_BUFFER`, or `None`.
///
/// Layout (little-endian): `ExtentCount` u32 at 0, `StartingVcn` i64 at 8 (4 bytes of
/// padding before it), then extents of `{ NextVcn i64, Lcn i64 }`, the first at offset 16.
/// `returned` is the byte count the kernel reported. Only bytes up to it are trusted.
/// An LCN of -1 marks a sparse or virtual extent, so it is `None` like any other
/// negative value.
pub(crate) fn first_lcn(buf: &[u8], returned: usize) -> Option<i64> {
    const HEADER_LEN: usize = 16;
    const EXTENT_LEN: usize = 16;
    const LCN_OFFSET_IN_EXTENT: usize = 8;

    let valid = buf.get(..returned)?;
    let extent_count = u32::from_le_bytes(valid.get(0..4)?.try_into().ok()?);
    if extent_count == 0 {
        return None;
    }
    let first = valid.get(HEADER_LEN..HEADER_LEN + EXTENT_LEN)?;
    let lcn_bytes = first.get(LCN_OFFSET_IN_EXTENT..LCN_OFFSET_IN_EXTENT + 8)?;
    let lcn = i64::from_le_bytes(lcn_bytes.try_into().ok()?);
    (lcn >= 0).then_some(lcn)
}

/// Byte offset of a cluster: `lcn * bytes_per_cluster`, with checked arithmetic.
///
/// `None` for a negative LCN, a zero cluster size (which would collapse every file to
/// offset 0 and destroy the ordering), or overflow.
pub(crate) fn lcn_to_byte_offset(lcn: i64, bytes_per_cluster: u64) -> Option<u64> {
    let cluster = u64::try_from(lcn).ok()?;
    if bytes_per_cluster == 0 {
        return None;
    }
    cluster.checked_mul(bytes_per_cluster)
}

#[cfg(test)]
mod tests {
    use super::{first_lcn, lcn_to_byte_offset};

    /// A retrieval buffer holding one extent with the given LCN. `extent_count` is written
    /// as-is so a test can describe a zero-extent reply.
    fn retrieval_bytes(extent_count: u32, lcn: i64) -> Vec<u8> {
        let mut buf = vec![0u8; 32];
        buf[0..4].copy_from_slice(&extent_count.to_le_bytes());
        buf[8..16].copy_from_slice(&0i64.to_le_bytes());
        buf[16..24].copy_from_slice(&4i64.to_le_bytes());
        buf[24..32].copy_from_slice(&lcn.to_le_bytes());
        buf
    }

    #[test]
    fn first_lcn_should_return_lcn_when_one_extent_present() {
        let buf = retrieval_bytes(1, 4096);
        let parsed = first_lcn(&buf, buf.len());
        assert_eq!(
            parsed,
            Some(4096),
            "one extent with Lcn 4096 should parse to Some(4096), got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_zero_extents() {
        let buf = retrieval_bytes(0, 4096);
        let parsed = first_lcn(&buf, buf.len());
        assert_eq!(
            parsed, None,
            "ExtentCount 0 should give None even with an Lcn present, got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_buffer_truncated_before_extent() {
        let buf = retrieval_bytes(1, 4096);
        let parsed = first_lcn(&buf, 31);
        assert_eq!(
            parsed, None,
            "returned 31 bytes cuts the first extent short, so the Lcn is unreadable, got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_returned_exceeds_buffer() {
        let buf = retrieval_bytes(1, 4096);
        let parsed = first_lcn(&buf, buf.len() + 1);
        assert_eq!(
            parsed, None,
            "a returned count past the buffer end must not be trusted, got {parsed:?}"
        );
    }

    #[test]
    fn first_lcn_should_return_none_when_lcn_is_minus_one() {
        let buf = retrieval_bytes(1, -1);
        let parsed = first_lcn(&buf, buf.len());
        assert_eq!(
            parsed, None,
            "Lcn -1 marks a sparse extent and should give None, got {parsed:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_multiply_lcn_by_cluster_size() {
        let offset = lcn_to_byte_offset(3, 4096);
        assert_eq!(
            offset,
            Some(12_288),
            "Lcn 3 at 4096 bytes per cluster should be byte 12288, got {offset:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_return_none_when_cluster_size_zero() {
        let offset = lcn_to_byte_offset(3, 0);
        assert_eq!(
            offset, None,
            "a zero cluster size is invalid and should give None, got {offset:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_return_none_when_multiplication_overflows() {
        let offset = lcn_to_byte_offset(i64::MAX, u64::MAX);
        assert_eq!(
            offset, None,
            "an offset past u64::MAX should give None, got {offset:?}"
        );
    }

    #[test]
    fn lcn_to_byte_offset_should_return_none_when_lcn_negative() {
        let offset = lcn_to_byte_offset(-1, 4096);
        assert_eq!(
            offset, None,
            "a negative Lcn has no physical offset and should give None, got {offset:?}"
        );
    }
}
