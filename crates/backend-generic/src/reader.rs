//! `ContentReader` over `std::fs` with positional reads.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use dedupe_core::error::Result;
use dedupe_core::traits::{ContentReader, FileHandle, FileMeta};

#[derive(Debug, Default)]
pub struct FsContentReader {
    opens: AtomicU64,
}

impl FsContentReader {
    pub fn new() -> Self {
        FsContentReader {
            opens: AtomicU64::new(0),
        }
    }
}

/// Opens `path` for reading only if it is a regular file.
///
/// Unix: `O_NOFOLLOW` refuses a symlink swapped in after enumeration, and `O_NONBLOCK`
/// stops a FIFO swapped in the same way from hanging a worker. The non-blocking flag is
/// cleared again after the open. A non-regular file is rejected with `InvalidInput`.
/// Windows keeps the plain read-only open. The walker never yields reparse points as files.
pub(crate) fn open_regular(path: &Path) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_CLOEXEC matches std's default and is stated here so the policy is explicit.
        opts.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    #[cfg(unix)]
    clear_nonblock(&file)?;
    Ok(file)
}

/// Clears `O_NONBLOCK` on an open descriptor. Regular-file reads ignore the flag, so
/// clearing it only restores the default mode for later users of the handle.
#[cfg(unix)]
fn clear_nonblock(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let fd = file.as_raw_fd();
    // SAFETY: `fd` is owned by `file`, which is alive for both calls, so the descriptor is
    // open. F_GETFL takes no pointer argument.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor and liveness as above. F_SETFL takes an int argument only,
    // so no memory is passed to the kernel.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// An open file. Positional reads go through the OS offset API, so the handle
/// keeps no cursor state.
struct FsHandle {
    file: File,
}

impl FsHandle {
    /// One positional read. An `Interrupted` error is returned as is; `read_at` retries it.
    fn read_once(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buf, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buf, offset)
        }
        #[cfg(not(any(unix, windows)))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut file = &self.file;
            file.seek(SeekFrom::Start(offset))?;
            file.read(buf)
        }
    }
}

impl FileHandle for FsHandle {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        // A signal can interrupt the syscall before any byte is read. That is not a
        // failure, so retry the same read.
        loop {
            match self.read_once(offset, buf) {
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                r => return Ok(r?),
            }
        }
    }
}

impl ContentReader for FsContentReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file = open_regular(path)?;
        self.opens.fetch_add(1, Ordering::Relaxed);
        FsHandle { file }.read_at(offset, buf)
    }

    fn open<'a>(&'a self, path: &'a Path) -> Result<Box<dyn FileHandle + 'a>> {
        let file = open_regular(path)?;
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(FsHandle { file }))
    }

    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        // Same identity source as the walker: one handle on Windows (GEN-07), lstat on Unix.
        let md = path.symlink_metadata()?;
        Ok(crate::meta::file_meta_at(path, &md, md.file_type()))
    }

    fn opens(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }

    fn first_extent_offset(&self, path: &Path) -> Result<Option<u64>> {
        #[cfg(target_os = "linux")]
        {
            // The offset only orders reads. A file that vanished since enumeration
            // must not abort the scan: the hash pass reports it as an io error.
            Ok(crate::extents::first_extent_offset(path).unwrap_or(None))
        }
        #[cfg(windows)]
        {
            // Same policy as Linux: the offset only orders reads, so a failed lookup must
            // not abort the scan.
            Ok(crate::extents::first_extent_offset(path).unwrap_or(None))
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        {
            let _ = path;
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A temp file that is removed when the test ends, including on panic. This
    /// crate has no `tempfile` dev-dependency, so the name is made unique by
    /// process id, test name and time.
    struct TempFile(PathBuf);

    impl TempFile {
        fn with_bytes(test_name: &str, bytes: &[u8]) -> std::io::Result<Self> {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            let name = format!(
                "dedupe-reader-{}-{test_name}-{nanos}.bin",
                std::process::id()
            );
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, bytes)?;
            Ok(Self(path))
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            // Best effort: a missing file here means the test already cleaned up.
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn open_should_count_one_open_for_many_reads() {
        let file = TempFile::with_bytes("open_many_reads", &[0xAB; 64])
            .expect("temp file should be writable");
        let reader = FsContentReader::new();
        let mut handle = reader
            .open(&file.0)
            .expect("open should succeed on a temp file");
        let mut buf = [0u8; 16];
        handle
            .read_at(0, &mut buf)
            .expect("first read on the handle should succeed");
        handle
            .read_at(16, &mut buf)
            .expect("second read on the handle should succeed");
        assert_eq!(
            reader.opens(),
            1,
            "two reads on one handle should open the file once, got {}",
            reader.opens()
        );
    }

    #[test]
    fn read_at_should_count_one_open_per_call() {
        let file = TempFile::with_bytes("read_at_per_call", &[0xCD; 64])
            .expect("temp file should be writable");
        let reader = FsContentReader::new();
        let mut buf = [0u8; 8];
        reader
            .read_at(&file.0, 0, &mut buf)
            .expect("first read_at should succeed");
        reader
            .read_at(&file.0, 8, &mut buf)
            .expect("second read_at should succeed");
        assert_eq!(
            reader.opens(),
            2,
            "two read_at calls should open the file twice, got {}",
            reader.opens()
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_should_refuse_symlink() {
        let target = TempFile::with_bytes("symlink_target", &[0x11; 8])
            .expect("target file should be writable");
        let link = TempFile::with_bytes("symlink_link", &[])
            .expect("placeholder for the link should be writable");
        std::fs::remove_file(&link.0).expect("placeholder should be removable");
        std::os::unix::fs::symlink(&target.0, &link.0).expect("symlink should be creatable");
        let result = open_regular(&link.0);
        assert!(
            result.is_err(),
            "O_NOFOLLOW should refuse a symlink, got Ok on {}",
            link.0.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_regular_should_reject_fifo_without_hanging() {
        use std::os::unix::ffi::OsStrExt;
        let fifo =
            TempFile::with_bytes("fifo", &[]).expect("placeholder for the fifo should be writable");
        std::fs::remove_file(&fifo.0).expect("placeholder should be removable");
        let c_path = std::ffi::CString::new(fifo.0.as_os_str().as_bytes())
            .expect("temp path should contain no NUL bytes");
        // SAFETY: `c_path` is a NUL-terminated string that lives across the call. mkfifo
        // reads the path only and does not retain the pointer.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo should create the test FIFO");
        // O_NONBLOCK makes the open return at once. A regression to a blocking open would
        // hang the test, so the open runs on a worker thread with a deadline.
        let (tx, rx) = std::sync::mpsc::channel();
        let path = fifo.0.clone();
        std::thread::spawn(move || {
            let outcome = open_regular(&path).map(|_| ()).map_err(|e| e.kind());
            let _ = tx.send(outcome);
        });
        let outcome = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("open_regular on a FIFO should return within 5 seconds, not block");
        assert_eq!(
            outcome,
            Err(ErrorKind::InvalidInput),
            "a FIFO should be rejected with InvalidInput"
        );
    }

    #[test]
    fn read_at_should_return_zero_when_offset_is_past_eof() {
        let file =
            TempFile::with_bytes("past_eof", &[0x5A; 64]).expect("temp file should be writable");
        let reader = FsContentReader::new();
        let mut buf = [0u8; 8];
        let n = reader
            .read_at(&file.0, 64, &mut buf)
            .expect("read at exactly EOF should succeed");
        assert_eq!(n, 0, "read at EOF should return 0 bytes, got {n}");
        let n = reader
            .read_at(&file.0, 1_000, &mut buf)
            .expect("read well past EOF should succeed");
        assert_eq!(n, 0, "read past EOF should return 0 bytes, got {n}");
    }

    #[test]
    fn read_at_should_return_bytes_from_middle_offset() {
        let bytes: Vec<u8> = (0u8..64).collect();
        let file =
            TempFile::with_bytes("middle_offset", &bytes).expect("temp file should be writable");
        let reader = FsContentReader::new();
        let mut buf = [0u8; 4];
        let n = reader
            .read_at(&file.0, 10, &mut buf)
            .expect("middle read should succeed");
        assert_eq!(n, 4, "a read fully inside the file should fill the buffer");
        assert_eq!(buf, [10, 11, 12, 13], "bytes should start at offset 10");
        let mut tail = [0u8; 8];
        let n = reader
            .read_at(&file.0, 60, &mut tail)
            .expect("read crossing EOF should succeed");
        assert_eq!(
            n, 4,
            "a read crossing EOF should return only the remaining bytes"
        );
        assert_eq!(&tail[..4], &[60, 61, 62, 63]);
    }

    #[cfg(unix)]
    #[test]
    fn metadata_should_report_size_and_nlink_for_regular_file() {
        let file = TempFile::with_bytes("metadata_regular", &[0x33; 64])
            .expect("temp file should be writable");
        let link = TempFile::with_bytes("metadata_link", &[])
            .expect("placeholder for the hard link should be writable");
        std::fs::remove_file(&link.0).expect("placeholder should be removable");
        std::fs::hard_link(&file.0, &link.0).expect("hard link should be creatable");
        let reader = FsContentReader::new();
        let meta = reader
            .metadata(&file.0)
            .expect("metadata on a regular file should succeed");
        assert_eq!(meta.size, 64, "size should match the bytes written");
        assert_eq!(meta.nlink, 2, "a hard-linked file should report nlink 2");
    }

    #[test]
    fn open_should_fail_without_counting_when_path_is_directory() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("dedupe-reader-{}-dir-{nanos}", std::process::id()));
        std::fs::create_dir(&dir).expect("temp directory should be creatable");
        let reader = FsContentReader::new();
        let result = reader.open(&dir);
        assert!(
            result.is_err(),
            "open on a directory should fail, got Ok for {}",
            dir.display()
        );
        assert_eq!(reader.opens(), 0, "a rejected open should not be counted");
        let _ = std::fs::remove_dir(&dir);
    }
}
