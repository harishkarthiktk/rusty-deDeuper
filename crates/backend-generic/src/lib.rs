//! Generic backend: `std::fs` walking + `pread`, portable path codec, and
//! per-OS volume identity. Target `cfg` and `unsafe` live in `sys/` (pure parsers in
//! `parse/`), never in `core`.

#![deny(unsafe_code)]

pub mod codec;
pub mod extents;
pub mod meta;
mod parse;
pub mod probe;
pub mod reader;
mod sys;
pub mod volume;
pub mod walk;

pub use codec::GenericPathCodec;
pub use probe::{fs_locality, FsLocality, GenericStorageProbe};
pub use reader::FsContentReader;
pub use volume::GenericVolumeIdentity;
pub use walk::WalkdirEnumerator;

/// Convenience bundle so `cli` can pass one object around.
pub struct GenericBackend {
    pub codec: GenericPathCodec,
    pub reader: FsContentReader,
    pub enumerator: WalkdirEnumerator,
    pub volume: GenericVolumeIdentity,
    pub probe: GenericStorageProbe,
}

impl Default for GenericBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl GenericBackend {
    pub fn new() -> Self {
        GenericBackend {
            codec: GenericPathCodec,
            reader: FsContentReader::new(),
            enumerator: WalkdirEnumerator,
            volume: GenericVolumeIdentity,
            probe: GenericStorageProbe,
        }
    }
}
