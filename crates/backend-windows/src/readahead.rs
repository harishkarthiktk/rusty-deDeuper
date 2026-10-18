//! Read-ahead for positional reads over an asynchronous backend.
//!
//! The backend owns the slot buffers, because overlapped I/O needs them pinned. This module
//! decides what to submit, what to wait for, and what to copy out. It is plain logic, so it
//! is tested with a fake backend on every OS.

use std::collections::VecDeque;
use std::io;

/// Asynchronous positional reads into numbered slots.
///
/// Contract, relied on by [`ReadAhead`]:
/// - `submit` either starts one read in `slot`, or returns `Err` and starts nothing.
/// - Every successful `submit` is matched by exactly one `complete` on the same slot, also
///   after `cancel_all`. `complete` ends the operation even when it fails.
/// - `data(slot)` is valid for the byte count returned by the last `complete` on that slot.
/// - A slot buffer must not move or be freed until its `complete` has returned.
/// - `cancel_all` asks outstanding reads to stop early. It does not replace `complete`.
pub trait AsyncBackend {
    /// Starts reading `len` bytes at `offset` into the buffer of `slot`.
    fn submit(&mut self, slot: usize, offset: u64, len: usize) -> io::Result<()>;
    /// Waits for the read in `slot` to finish and returns the bytes it produced.
    fn complete(&mut self, slot: usize) -> io::Result<usize>;
    /// The buffer of `slot`. The first `n` bytes hold the last completed read.
    fn data(&self, slot: usize) -> &[u8];
    /// Requests cancellation of every outstanding read.
    fn cancel_all(&mut self);
}

/// One submitted read and its place in the window.
#[derive(Clone, Copy, Debug)]
struct Chunk {
    slot: usize,
    offset: u64,
    /// Bytes requested from the backend.
    expected: usize,
    /// Bytes the backend reported. Meaningful once `completed` is set.
    valid: usize,
    completed: bool,
}

/// Serves sequential reads from a window of in-flight chunk reads.
///
/// The window covers `[cursor, next_submit)`, oldest chunk first. A read at the cursor is
/// served from the window. A read anywhere else drains the window and starts again there.
pub struct ReadAhead<B: AsyncBackend> {
    backend: B,
    chunk: usize,
    file_len: u64,
    /// Offset of the next byte the consumer will receive.
    cursor: u64,
    /// Offset of the next chunk to submit.
    next_submit: u64,
    window: VecDeque<Chunk>,
    free_slots: Vec<usize>,
}

impl<B: AsyncBackend> ReadAhead<B> {
    /// `depth` slots of `chunk` bytes each, over a file of `file_len` bytes.
    ///
    /// The backend must provide at least `depth` slots of at least `chunk` bytes. A `depth`
    /// or `chunk` of 0 is raised to 1.
    pub fn new(backend: B, depth: usize, chunk: usize, file_len: u64) -> Self {
        let depth = depth.max(1);
        Self {
            backend,
            chunk: chunk.max(1),
            file_len,
            cursor: 0,
            next_submit: 0,
            window: VecDeque::with_capacity(depth),
            free_slots: (0..depth).rev().collect(),
        }
    }

    /// Reads up to `buf.len()` bytes at `offset`, returning fewer only at end of file.
    ///
    /// An error is returned only when nothing was delivered by this call. If bytes were
    /// already copied, the call returns them and the next call retries from the cursor.
    pub fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || offset >= self.file_len {
            return Ok(0);
        }
        if offset != self.cursor {
            self.restart_at(offset);
        }
        let mut copied = 0;
        while copied < buf.len() && self.cursor < self.file_len {
            match self.step(&mut buf[copied..]) {
                Ok(0) => break,
                Ok(n) => copied += n,
                Err(err) if copied == 0 => return Err(err),
                Err(err) => {
                    tracing::warn!(error = %err, "read-ahead failed after partial delivery");
                    break;
                }
            }
        }
        Ok(copied)
    }

    /// One round: top up the window, wait for the oldest chunk, copy from it, top up again.
    fn step(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.fill().inspect_err(|_| self.abandon())?;
        if self.window.front().is_some_and(|chunk| !chunk.completed) {
            self.complete_front().inspect_err(|_| self.abandon())?;
        }
        let copied = self.copy_front(buf).inspect_err(|_| self.abandon())?;
        if let Err(err) = self.fill() {
            // The next read reissues the submit, and reports the error if it persists.
            tracing::warn!(error = %err, "read-ahead submit failed; retrying on next read");
            self.abandon();
        }
        Ok(copied)
    }

    /// Submits chunks from `next_submit` while slots are free and the file has bytes left.
    fn fill(&mut self) -> io::Result<()> {
        let chunk = to_u64(self.chunk)?;
        while self.next_submit < self.file_len {
            let Some(&slot) = self.free_slots.last() else {
                break;
            };
            let expected = to_usize((self.file_len - self.next_submit).min(chunk))?;
            self.backend.submit(slot, self.next_submit, expected)?;
            self.free_slots.pop();
            self.window.push_back(Chunk {
                slot,
                offset: self.next_submit,
                expected,
                valid: 0,
                completed: false,
            });
            self.next_submit = self.next_submit.saturating_add(to_u64(expected)?);
        }
        Ok(())
    }

    /// Waits for the oldest chunk. A failed wait removes it, because the backend has
    /// finished that operation.
    fn complete_front(&mut self) -> io::Result<()> {
        let Some(front) = self.window.front().copied() else {
            return Ok(());
        };
        match self.backend.complete(front.slot) {
            Ok(reported) => {
                let valid = reported.min(front.expected);
                if let Some(entry) = self.window.front_mut() {
                    entry.valid = valid;
                    entry.completed = true;
                }
                if valid < front.expected {
                    // A short read only happens at end of file, so the file ends here and
                    // the chunks queued behind this one are discarded.
                    self.file_len = front.offset.saturating_add(to_u64(valid)?);
                    self.release_after_front();
                    self.next_submit = self.file_len;
                }
                Ok(())
            }
            Err(err) => {
                self.window.pop_front();
                self.free_slots.push(front.slot);
                Err(err)
            }
        }
    }

    /// Copies from the completed front chunk. Releases the chunk once the cursor passes it.
    fn copy_front(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(front) = self.window.front().copied() else {
            return Ok(0);
        };
        let behind = self
            .cursor
            .checked_sub(front.offset)
            .ok_or_else(|| io::Error::other("read-ahead window is out of step with the cursor"))?;
        let within = to_usize(behind)?;
        let n = buf.len().min(front.valid.saturating_sub(within));
        let source = self
            .backend
            .data(front.slot)
            .get(within..within + n)
            .ok_or_else(|| io::Error::other("backend slot is shorter than its completed length"))?;
        buf[..n].copy_from_slice(source);
        self.cursor = self.cursor.saturating_add(to_u64(n)?);

        let end = front.offset.saturating_add(to_u64(front.valid)?);
        if self.cursor >= end {
            self.window.pop_front();
            self.free_slots.push(front.slot);
        }
        Ok(n)
    }

    /// Drops every chunk except the front one, which must already be completed.
    fn release_after_front(&mut self) {
        while self.window.len() > 1 {
            let Some(chunk) = self.window.pop_back() else {
                break;
            };
            self.release_chunk(chunk);
        }
    }

    /// Discards every chunk and moves the cursor to `offset`.
    fn restart_at(&mut self, offset: u64) {
        self.drain();
        self.cursor = offset;
        self.next_submit = offset;
    }

    /// Discards the window after an error. The next read at the cursor starts fresh.
    fn abandon(&mut self) {
        self.drain();
        self.next_submit = self.cursor;
    }

    fn drain(&mut self) {
        while let Some(chunk) = self.window.pop_front() {
            self.release_chunk(chunk);
        }
    }

    /// Waits for an in-flight chunk and frees its slot. Its result is discarded.
    fn release_chunk(&mut self, chunk: Chunk) {
        if !chunk.completed {
            // The bytes are no longer wanted. The operation is over either way, so the
            // slot can be reused, and the error adds nothing.
            let _ = self.backend.complete(chunk.slot);
        }
        self.free_slots.push(chunk.slot);
    }
}

impl<B: AsyncBackend> Drop for ReadAhead<B> {
    fn drop(&mut self) {
        // Cancel first so pending reads finish quickly, then wait for each one. The backend
        // contract forbids freeing a slot buffer while its read is still running.
        self.backend.cancel_all();
        self.drain();
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
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Event {
        Submit {
            slot: usize,
            offset: u64,
            len: usize,
        },
        Complete {
            slot: usize,
        },
        Cancel,
    }

    type Log = Rc<RefCell<Vec<Event>>>;

    /// Serves `file` from memory. Each submitted read is held until `complete`, so the test
    /// can see what is in flight at any point.
    struct FakeBackend {
        file: Vec<u8>,
        slots: Vec<Vec<u8>>,
        in_flight: Vec<Option<(u64, usize)>>,
        fail_next_complete: bool,
        log: Log,
    }

    impl FakeBackend {
        fn new(file: Vec<u8>, depth: usize, chunk: usize, log: Log) -> Self {
            Self {
                file,
                slots: vec![vec![0; chunk]; depth],
                in_flight: vec![None; depth],
                fail_next_complete: false,
                log,
            }
        }
    }

    impl AsyncBackend for FakeBackend {
        fn submit(&mut self, slot: usize, offset: u64, len: usize) -> io::Result<()> {
            self.log
                .borrow_mut()
                .push(Event::Submit { slot, offset, len });
            let entry = self
                .in_flight
                .get_mut(slot)
                .ok_or_else(|| io::Error::other("no such slot"))?;
            if entry.is_some() {
                return Err(io::Error::other("slot already in flight"));
            }
            *entry = Some((offset, len));
            Ok(())
        }

        fn complete(&mut self, slot: usize) -> io::Result<usize> {
            self.log.borrow_mut().push(Event::Complete { slot });
            let (offset, len) = self
                .in_flight
                .get_mut(slot)
                .and_then(Option::take)
                .ok_or_else(|| io::Error::other("complete without a submit"))?;
            if std::mem::take(&mut self.fail_next_complete) {
                return Err(io::Error::other("injected failure"));
            }
            // Clamp both ends: a read may start past the bytes the fake actually holds.
            let start = (offset as usize).min(self.file.len());
            let end = (start + len).min(self.file.len());
            let n = end - start;
            self.slots[slot][..n].copy_from_slice(&self.file[start..end]);
            Ok(n)
        }

        fn data(&self, slot: usize) -> &[u8] {
            &self.slots[slot]
        }

        fn cancel_all(&mut self) {
            self.log.borrow_mut().push(Event::Cancel);
        }
    }

    fn bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 253) as u8).collect()
    }

    /// A reader over `file` whose declared length is `file_len`.
    fn reader(
        file: Vec<u8>,
        depth: usize,
        chunk: usize,
        file_len: u64,
    ) -> (ReadAhead<FakeBackend>, Log) {
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let backend = FakeBackend::new(file, depth, chunk, Rc::clone(&log));
        (ReadAhead::new(backend, depth, chunk, file_len), log)
    }

    fn read_all(ra: &mut ReadAhead<FakeBackend>, step: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = vec![0u8; step];
        let mut offset = 0u64;
        loop {
            let n = ra.read_at(offset, &mut buf)?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
            offset += n as u64;
        }
        Ok(out)
    }

    fn submits(log: &Log) -> Vec<(u64, usize)> {
        log.borrow()
            .iter()
            .filter_map(|event| match *event {
                Event::Submit { offset, len, .. } => Some((offset, len)),
                _ => None,
            })
            .collect()
    }

    fn max_in_flight(log: &Log) -> usize {
        let mut current = 0usize;
        let mut max = 0usize;
        for event in log.borrow().iter() {
            match event {
                Event::Submit { .. } => {
                    current += 1;
                    max = max.max(current);
                }
                Event::Complete { .. } => current = current.saturating_sub(1),
                Event::Cancel => {}
            }
        }
        max
    }

    /// Submits minus completes: the number of reads still waiting to be awaited.
    fn outstanding(events: &[Event]) -> i64 {
        events.iter().fold(0i64, |acc, event| match event {
            Event::Submit { .. } => acc + 1,
            Event::Complete { .. } => acc - 1,
            Event::Cancel => acc,
        })
    }

    #[test]
    fn read_at_should_keep_depth_reads_in_flight_when_reading_sequentially() {
        let (mut ra, log) = reader(bytes(1000), 4, 10, 1000);
        let mut buf = [0u8; 5];
        ra.read_at(0, &mut buf).expect("read");
        assert_eq!(
            max_in_flight(&log),
            4,
            "the window must hold depth reads in flight ahead of the consumer"
        );
    }

    #[test]
    fn read_at_should_return_matching_bytes_when_reading_sequentially() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        let got = read_all(&mut ra, 7);
        assert_eq!(
            got.expect("read_all"),
            bytes(1000),
            "sequential reads must reproduce the file"
        );
    }

    #[test]
    fn read_at_should_fill_buffer_across_chunks_when_buffer_exceeds_chunk() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        let mut buf = [0u8; 25];
        let n = ra.read_at(0, &mut buf).expect("read");
        assert_eq!(
            (n, buf.to_vec()),
            (25, bytes(1000)[..25].to_vec()),
            "a buffer larger than a chunk must be filled across chunks"
        );
    }

    #[test]
    fn read_at_should_return_part_of_chunk_when_buffer_is_smaller_than_chunk() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        let mut buf = [0u8; 3];
        let n = ra.read_at(0, &mut buf).expect("read");
        assert_eq!(
            n, 3,
            "a buffer smaller than a chunk must receive exactly its size"
        );
    }

    #[test]
    fn read_at_should_submit_short_last_chunk_when_file_is_not_chunk_multiple() {
        let (mut ra, log) = reader(bytes(25), 4, 10, 25);
        read_all(&mut ra, 4).expect("read_all");
        assert_eq!(
            submits(&log),
            [(0, 10), (10, 10), (20, 5)],
            "the last chunk must be shorter and nothing may be submitted past EOF"
        );
    }

    #[test]
    fn read_at_should_not_submit_past_eof_when_window_is_larger_than_file() {
        let (mut ra, log) = reader(bytes(25), 8, 10, 25);
        read_all(&mut ra, 4).expect("read_all");
        assert_eq!(
            submits(&log).len(),
            3,
            "a window of 8 over a 25 byte file must submit only 3 chunks"
        );
    }

    #[test]
    fn read_at_should_restart_window_when_offset_jumps() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        let mut first = [0u8; 5];
        ra.read_at(0, &mut first).expect("first read");
        let mut jumped = [0u8; 5];
        let n = ra.read_at(500, &mut jumped).expect("jump read");
        assert_eq!(
            (n, jumped.to_vec()),
            (5, bytes(1000)[500..505].to_vec()),
            "a read at a new offset must return the bytes at that offset"
        );
    }

    #[test]
    fn read_at_should_complete_every_submit_before_restart_when_offset_jumps() {
        let (mut ra, log) = reader(bytes(1000), 4, 10, 1000);
        let mut buf = [0u8; 5];
        ra.read_at(0, &mut buf).expect("first read");
        ra.read_at(500, &mut buf).expect("jump read");
        let events = log.borrow();
        let first_new = events
            .iter()
            .position(|event| matches!(event, Event::Submit { offset: 500, .. }))
            .expect("a submit at the new offset");
        assert_eq!(
            outstanding(&events[..first_new]),
            0,
            "every read in flight before the jump must be awaited before the restart"
        );
    }

    #[test]
    fn read_at_should_return_zero_when_buffer_is_empty() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        let n = ra.read_at(0, &mut []).expect("read");
        assert_eq!(n, 0, "an empty buffer must return 0");
    }

    #[test]
    fn read_at_should_return_zero_when_offset_is_at_eof() {
        let (mut ra, _log) = reader(bytes(25), 4, 10, 25);
        let mut buf = [0u8; 5];
        let n = ra.read_at(25, &mut buf).expect("read");
        assert_eq!(n, 0, "an offset at EOF must return 0");
    }

    #[test]
    fn read_at_should_return_error_when_complete_fails() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        ra.backend.fail_next_complete = true;
        let mut buf = [0u8; 5];
        let result = ra.read_at(0, &mut buf);
        assert!(
            matches!(result, Err(ref e) if e.to_string() == "injected failure"),
            "a failed complete must surface its error, got {result:?}"
        );
    }

    #[test]
    fn read_at_should_leave_no_slot_in_flight_when_complete_fails() {
        let (mut ra, log) = reader(bytes(1000), 4, 10, 1000);
        ra.backend.fail_next_complete = true;
        let mut buf = [0u8; 5];
        let _ = ra.read_at(0, &mut buf);
        assert_eq!(
            outstanding(&log.borrow()),
            0,
            "after a failed complete every other read must also be awaited"
        );
    }

    #[test]
    fn read_at_should_return_correct_bytes_when_retried_after_complete_failure() {
        let (mut ra, _log) = reader(bytes(1000), 4, 10, 1000);
        ra.backend.fail_next_complete = true;
        let mut buf = [0u8; 5];
        let _ = ra.read_at(0, &mut buf);
        let n = ra.read_at(0, &mut buf).expect("retry");
        assert_eq!(
            (n, buf.to_vec()),
            (5, bytes(1000)[..5].to_vec()),
            "a retry after a failure must return the correct bytes"
        );
    }

    #[test]
    fn read_at_should_stop_at_short_completion_when_backend_returns_fewer_bytes() {
        let (mut ra, _log) = reader(bytes(15), 4, 10, 25);
        let mut buf = [0u8; 25];
        let n = ra.read_at(0, &mut buf).expect("read");
        assert_eq!(
            n, 15,
            "a short completion must end the file at the bytes actually read"
        );
    }

    #[test]
    fn read_at_should_keep_one_read_in_flight_when_depth_is_one() {
        let (mut ra, log) = reader(bytes(1000), 1, 10, 1000);
        read_all(&mut ra, 7).expect("read_all");
        assert_eq!(
            max_in_flight(&log),
            1,
            "depth 1 must never have more than one read in flight"
        );
    }

    #[test]
    fn read_at_should_submit_chunks_in_order_when_depth_is_one() {
        let (mut ra, log) = reader(bytes(25), 1, 10, 25);
        read_all(&mut ra, 3).expect("read_all");
        assert_eq!(
            submits(&log),
            [(0, 10), (10, 10), (20, 5)],
            "depth 1 must submit one chunk at a time, in file order"
        );
    }

    #[test]
    fn drop_should_cancel_when_reads_are_in_flight() {
        let (mut ra, log) = reader(bytes(1000), 4, 10, 1000);
        let mut buf = [0u8; 5];
        ra.read_at(0, &mut buf).expect("read");
        drop(ra);
        assert!(
            log.borrow().contains(&Event::Cancel),
            "dropping with reads in flight must call cancel_all"
        );
    }

    #[test]
    fn drop_should_await_every_submit_when_reads_are_in_flight() {
        let (mut ra, log) = reader(bytes(1000), 4, 10, 1000);
        let mut buf = [0u8; 5];
        ra.read_at(0, &mut buf).expect("read");
        drop(ra);
        assert_eq!(
            outstanding(&log.borrow()),
            0,
            "dropping must complete every outstanding read before the buffers go"
        );
    }
}
