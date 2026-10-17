//! `ContentReader` over `std::fs` with positional reads.

use std::fs::File;
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

/// An open file. Positional reads go through the OS offset API, so the handle
/// keeps no cursor state.
struct FsHandle {
    file: File,
}

impl FileHandle for FsHandle {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            Ok(self.file.read_at(buf, offset)?)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            Ok(self.file.seek_read(buf, offset)?)
        }
        #[cfg(not(any(unix, windows)))]
        {
            use std::io::{Read, Seek, SeekFrom};
            self.file.seek(SeekFrom::Start(offset))?;
            Ok(self.file.read(buf)?)
        }
    }
}

impl ContentReader for FsContentReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file = File::open(path)?;
        self.opens.fetch_add(1, Ordering::Relaxed);
        FsHandle { file }.read_at(offset, buf)
    }

    fn open<'a>(&'a self, path: &'a Path) -> Result<Box<dyn FileHandle + 'a>> {
        let file = File::open(path)?;
        self.opens.fetch_add(1, Ordering::Relaxed);
        Ok(Box::new(FsHandle { file }))
    }

    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        let md = path.symlink_metadata()?;
        Ok(crate::meta::file_meta_from(&md, md.file_type()))
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
}
