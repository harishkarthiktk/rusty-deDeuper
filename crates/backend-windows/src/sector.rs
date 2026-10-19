//! Unaligned reads over a sector-addressed source.
//!
//! A raw volume handle only accepts reads whose offset and length are multiples of the
//! sector size. `SectorReader` serves arbitrary `Read + Seek` calls from a chunk-sized
//! cache that is always loaded with aligned requests. The logic is portable: the source
//! is any `Read + Seek`, so the tests use an in-memory fake.

use std::io::{self, Read, Seek, SeekFrom};

/// One cached aligned window of the source.
struct Slot {
    buf: Vec<u8>,
    /// Source offset of `buf[0]`. Always a multiple of `chunk`.
    start: u64,
    /// One past the last valid source offset in `buf`. Equal to `start` when empty.
    end: u64,
}

impl Slot {
    fn empty() -> Self {
        Slot {
            buf: Vec::new(),
            start: 0,
            end: 0,
        }
    }

    fn contains(&self, pos: u64) -> bool {
        (self.start..self.end).contains(&pos)
    }
}

/// Reads arbitrary byte ranges from a source that only accepts aligned requests.
///
/// Invariant: every `read` issued on `inner` has an offset and a length that are
/// multiples of the sector size. The source is assumed to hold whole sectors.
///
/// Two windows are cached: a streaming window for sequential reads, and an optional
/// pinned window for one hot chunk, so reads of it never evict the stream.
pub struct SectorReader<R> {
    inner: R,
    sector: u64,
    chunk: u64,
    len: u64,
    pos: u64,
    /// Streaming window for sequential record reads.
    stream: Slot,
    /// Window pinned to one hot chunk (the `$MFT` record 0 that `Ntfs::file` re-reads).
    pinned: Slot,
    /// Chunk-aligned start of the pinned chunk, if any.
    pin_at: Option<u64>,
}

impl<R: Read + Seek> SectorReader<R> {
    /// Keeps the chunk that holds `offset` in its own slot, so reads of it never evict the
    /// streaming window.
    pub fn pin(&mut self, offset: u64) {
        self.pin_at = Some(offset - offset % self.chunk);
    }

    /// Drops cached bytes so the next read goes to the source. Used to retry a torn record.
    pub fn invalidate(&mut self) {
        self.stream.end = self.stream.start;
        self.pinned.end = self.pinned.start;
    }

    /// Wraps `inner`, a source of `len` bytes.
    ///
    /// A `sector_size` of 0 is treated as 1. `chunk` is rounded down to a multiple of the
    /// sector size, with a minimum of one sector. The cache buffers are allocated on the
    /// first read, not here.
    pub fn new(inner: R, sector_size: u64, chunk: u64, len: u64) -> Self {
        let sector = sector_size.max(1);
        let chunk = (chunk / sector).max(1) * sector;
        Self {
            inner,
            sector,
            chunk,
            len,
            pos: 0,
            stream: Slot::empty(),
            pinned: Slot::empty(),
            pin_at: None,
        }
    }

    /// The slot that serves `pos`, loaded first if it does not hold it yet.
    fn slot_for(&mut self, pos: u64) -> io::Result<&Slot> {
        let start = pos - pos % self.chunk;
        let use_pinned = self.pin_at == Some(start);
        let hit = if use_pinned {
            self.pinned.contains(pos)
        } else {
            self.stream.contains(pos)
        };
        if !hit {
            let (inner, sector, chunk, len) = (&mut self.inner, self.sector, self.chunk, self.len);
            let slot = if use_pinned {
                &mut self.pinned
            } else {
                &mut self.stream
            };
            fill_slot(inner, slot, sector, chunk, len, pos)?;
        }
        Ok(if use_pinned {
            &self.pinned
        } else {
            &self.stream
        })
    }
}

/// Loads the chunk that contains `pos` into `slot` with aligned requests only.
fn fill_slot<R: Read + Seek>(
    inner: &mut R,
    slot: &mut Slot,
    sector: u64,
    chunk: u64,
    len: u64,
    pos: u64,
) -> io::Result<()> {
    let start = pos - pos % chunk;
    let remaining = len - start;
    // Bytes the caller may see from this chunk.
    let needed = remaining.min(chunk);
    // The last block is requested in whole sectors, capped at one chunk.
    let request = remaining
        .checked_next_multiple_of(sector)
        .map_or(chunk, |rounded| rounded.min(chunk));

    let needed_len = to_usize(needed)?;
    let request_len = to_usize(request)?;
    let chunk_len = to_usize(chunk)?;
    if slot.buf.len() < chunk_len {
        slot.buf.resize(chunk_len, 0);
    }

    // Mark the slot empty first so a failed fill cannot serve stale bytes.
    slot.start = start;
    slot.end = start;
    inner.seek(SeekFrom::Start(start))?;

    let mut filled = 0usize;
    while filled < needed_len {
        match inner.read(&mut slot.buf[filled..request_len]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "source ended before its declared length",
                ));
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    slot.end = start + needed;
    Ok(())
}

impl<R: Read + Seek> Read for SectorReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        let pos = self.pos;
        let slot = self.slot_for(pos)?;
        // `slot_for` guarantees `pos` is inside the slot, so `available` is at least 1.
        let within = to_usize(pos - slot.start)?;
        let available = to_usize(slot.end - pos)?;
        let n = buf.len().min(available);
        buf[..n].copy_from_slice(&slot.buf[within..within + n]);
        self.pos += to_u64(n)?;
        Ok(n)
    }
}

impl<R: Read + Seek> Seek for SectorReader<R> {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let next = match from {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::Current(delta) => offset_by(self.pos, delta),
            SeekFrom::End(delta) => offset_by(self.len, delta),
        };
        let next = next.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek to a negative or overflowing position",
            )
        })?;
        self.pos = next;
        Ok(next)
    }
}

/// `base + delta` with checked arithmetic; `None` when the result is negative or overflows.
fn offset_by(base: u64, delta: i64) -> Option<u64> {
    if delta >= 0 {
        base.checked_add(delta.unsigned_abs())
    } else {
        base.checked_sub(delta.unsigned_abs())
    }
}

fn to_usize(value: u64) -> io::Result<usize> {
    usize::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "value does not fit in usize on this platform",
        )
    })
}

fn to_u64(value: usize) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "value does not fit in u64 on this platform",
        )
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    const SECTOR: u64 = 512;
    const CHUNK: u64 = 1024;
    const DATA_LEN: usize = 4096;

    /// In-memory source that records every read and rejects unaligned ones, like a raw
    /// volume handle does.
    struct RecordingSource {
        inner: Cursor<Vec<u8>>,
        sector: u64,
        requests: Vec<(u64, usize)>,
    }

    impl Read for RecordingSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let offset = self.inner.position();
            self.requests.push((offset, buf.len()));
            let len = to_u64(buf.len())?;
            if !offset.is_multiple_of(self.sector) || !len.is_multiple_of(self.sector) {
                return Err(io::Error::other(format!(
                    "unaligned read at offset {offset} length {len}"
                )));
            }
            self.inner.read(buf)
        }
    }

    impl Seek for RecordingSource {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn reader_over(data: Vec<u8>, chunk: u64) -> SectorReader<RecordingSource> {
        let len = data.len() as u64;
        let source = RecordingSource {
            inner: Cursor::new(data),
            sector: SECTOR,
            requests: Vec::new(),
        };
        SectorReader::new(source, SECTOR, chunk, len)
    }

    fn sector_reader() -> SectorReader<RecordingSource> {
        reader_over(pattern(DATA_LEN), CHUNK)
    }

    fn read_n(reader: &mut SectorReader<RecordingSource>, n: usize) -> io::Result<Vec<u8>> {
        let mut out = vec![0u8; n];
        let got = reader.read(&mut out)?;
        out.truncate(got);
        Ok(out)
    }

    #[test]
    fn read_should_return_expected_bytes_when_offset_is_unaligned() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(100)).expect("seek");
        let got = read_n(&mut reader, 50);
        assert_eq!(
            got.expect("read"),
            pattern(DATA_LEN)[100..150].to_vec(),
            "an unaligned read must return the bytes at that offset"
        );
    }

    #[test]
    fn read_should_return_bytes_across_chunk_boundary_when_range_spans_two_chunks() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(1000)).expect("seek");
        // `read` may stop at the chunk end (io::Read allows short reads), so loop to fill.
        let mut got = vec![0u8; 100];
        reader.read_exact(&mut got).expect("read_exact");
        assert_eq!(
            got,
            pattern(DATA_LEN)[1000..1100].to_vec(),
            "a read crossing the 1024 byte chunk boundary must be stitched correctly"
        );
    }

    #[test]
    fn read_should_return_last_byte_when_positioned_one_before_len() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(4095)).expect("seek");
        let got = read_n(&mut reader, 10);
        assert_eq!(
            got.expect("read"),
            vec![pattern(DATA_LEN)[4095]],
            "a read at len - 1 must return exactly the final byte"
        );
    }

    #[test]
    fn read_should_return_zero_when_position_is_past_len() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(5000)).expect("seek");
        let got = read_n(&mut reader, 10);
        assert_eq!(
            got.expect("read").len(),
            0,
            "a read past len must return 0 bytes"
        );
    }

    #[test]
    fn read_should_return_zero_when_buffer_is_empty() {
        let mut reader = sector_reader();
        let got = reader.read(&mut []);
        assert_eq!(got.expect("read"), 0, "an empty buffer must return 0");
    }

    #[test]
    fn seek_should_return_new_position_when_start_is_given() {
        let mut reader = sector_reader();
        let got = reader.seek(SeekFrom::Start(300));
        assert_eq!(
            got.expect("seek"),
            300,
            "SeekFrom::Start must return the offset"
        );
    }

    #[test]
    fn seek_should_advance_position_when_current_delta_is_positive() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(10)).expect("seek");
        let got = reader.seek(SeekFrom::Current(5));
        assert_eq!(
            got.expect("seek"),
            15,
            "SeekFrom::Current(+5) from 10 must be 15"
        );
    }

    #[test]
    fn seek_should_move_back_when_current_delta_is_negative() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(100)).expect("seek");
        let got = reader.seek(SeekFrom::Current(-30));
        assert_eq!(
            got.expect("seek"),
            70,
            "SeekFrom::Current(-30) from 100 must be 70"
        );
    }

    #[test]
    fn seek_should_position_from_len_when_end_delta_is_negative() {
        let mut reader = sector_reader();
        let got = reader.seek(SeekFrom::End(-16));
        assert_eq!(
            got.expect("seek"),
            4080,
            "SeekFrom::End(-16) on a 4096 byte source must be 4080"
        );
    }

    #[test]
    fn seek_should_return_error_when_position_would_be_negative() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(0)).expect("seek");
        let got = reader.seek(SeekFrom::Current(-1));
        assert!(
            matches!(got, Err(ref e) if e.kind() == io::ErrorKind::InvalidInput),
            "seeking before offset 0 must fail with InvalidInput, got {got:?}"
        );
    }

    #[test]
    fn read_should_issue_one_aligned_chunk_read_when_request_is_one_byte() {
        let mut reader = sector_reader();
        reader.seek(SeekFrom::Start(1)).expect("seek");
        read_n(&mut reader, 1).expect("read");
        assert_eq!(
            reader.inner.requests,
            vec![(0, CHUNK as usize)],
            "a one-byte read must still fetch one aligned chunk from the source"
        );
    }

    #[test]
    fn read_should_issue_no_extra_inner_read_when_reads_stay_in_same_chunk() {
        let mut reader = sector_reader();
        for offset in [0u64, 500, 1000] {
            reader.seek(SeekFrom::Start(offset)).expect("seek");
            read_n(&mut reader, 10).expect("read");
        }
        assert_eq!(
            reader.inner.requests.len(),
            1,
            "reads inside one cached chunk must not reach the source again"
        );
    }

    #[test]
    fn new_should_round_chunk_down_to_sector_multiple_when_chunk_is_not_aligned() {
        let mut reader = reader_over(pattern(DATA_LEN), 1000);
        read_n(&mut reader, 1).expect("read");
        assert_eq!(
            reader.inner.requests,
            vec![(0, 512)],
            "a 1000 byte chunk with 512 byte sectors must be rounded down to 512"
        );
    }

    #[test]
    fn new_should_use_one_sector_when_chunk_is_smaller_than_a_sector() {
        let mut reader = reader_over(pattern(DATA_LEN), 10);
        read_n(&mut reader, 1).expect("read");
        assert_eq!(
            reader.inner.requests,
            vec![(0, 512)],
            "a chunk below one sector must be raised to one sector"
        );
    }

    #[test]
    fn read_to_end_should_return_all_bytes_when_len_is_not_sector_multiple() {
        let data = pattern(1000);
        let mut reader = reader_over(data.clone(), CHUNK);
        let mut out = Vec::new();
        reader.read_to_end(&mut out).expect("read_to_end");
        assert_eq!(
            out, data,
            "a source whose len is not a sector multiple must read fully"
        );
    }

    #[test]
    fn read_should_request_whole_sectors_when_len_is_not_sector_multiple() {
        let mut reader = reader_over(pattern(1000), CHUNK);
        read_n(&mut reader, 1).expect("read");
        assert_eq!(
            reader.inner.requests,
            vec![(0, 1024)],
            "the last block must be requested in whole sectors, capped at one chunk"
        );
    }

    #[test]
    fn read_should_match_cursor_when_many_offset_length_pairs_are_read() {
        let data = pattern(DATA_LEN);
        let mut reader = reader_over(data.clone(), CHUNK);
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..500 {
            let offset = next() % (DATA_LEN as u64 + 64);
            let wanted = (next() % 700) as usize;
            reader.seek(SeekFrom::Start(offset)).expect("seek");
            let mut got = Vec::new();
            (&mut reader)
                .take(wanted as u64)
                .read_to_end(&mut got)
                .expect("read");
            let start = (offset as usize).min(DATA_LEN);
            let end = (start + wanted).min(DATA_LEN);
            assert_eq!(
                got,
                data[start..end].to_vec(),
                "read at offset {offset} length {wanted} must match the plain source"
            );
        }
    }

    #[test]
    fn read_should_issue_only_aligned_requests_when_reading_unaligned_ranges() {
        let mut reader = sector_reader();
        for offset in [3u64, 700, 1500, 4000] {
            reader.seek(SeekFrom::Start(offset)).expect("seek");
            read_n(&mut reader, 37).expect("read");
        }
        let all_aligned = reader.inner.requests.iter().all(|&(offset, len)| {
            offset.is_multiple_of(SECTOR) && (len as u64).is_multiple_of(SECTOR)
        });
        assert!(
            all_aligned,
            "every source request must be sector aligned, got {:?}",
            reader.inner.requests
        );
    }
}
