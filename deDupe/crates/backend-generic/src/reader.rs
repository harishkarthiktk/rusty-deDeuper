//! `ContentReader` over `std::fs` with positional reads.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use dedupe_core::error::Result;
use dedupe_core::traits::{ContentReader, FileMeta};

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

impl ContentReader for FsContentReader {
    fn read_at(&self, path: &Path, offset: u64, buf: &mut [u8]) -> Result<usize> {
        let file = File::open(path)?;
        self.opens.fetch_add(1, Ordering::Relaxed);

        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            Ok(file.read_at(buf, offset)?)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            Ok(file.seek_read(buf, offset)?)
        }
        #[cfg(not(any(unix, windows)))]
        {
            use std::io::{Read, Seek, SeekFrom};
            let mut f = file;
            f.seek(SeekFrom::Start(offset))?;
            Ok(f.read(buf)?)
        }
    }

    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        let md = path.symlink_metadata()?;
        Ok(crate::meta::file_meta_from(&md, md.file_type()))
    }

    fn opens(&self) -> u64 {
        self.opens.load(Ordering::Relaxed)
    }
}
