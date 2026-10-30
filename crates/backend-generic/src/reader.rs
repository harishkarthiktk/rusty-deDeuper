//! `ContentReader` over `std::fs` with positional reads.

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sys::{Os, Platform};
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
    Os::configure_open(&mut opts);
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "not a regular file",
        ));
    }
    Os::after_open(&file)?;
    Ok(file)
}

/// An open file. Positional reads go through the OS offset API, so the handle
/// keeps no cursor state.
struct FsHandle {
    file: File,
}

impl FsHandle {
    /// One positional read. An `Interrupted` error is returned as is; `read_at` retries it.
    fn read_once(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        Os::read_once(&self.file, offset, buf)
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
        Os::first_extent_hint(path)
    }
}
