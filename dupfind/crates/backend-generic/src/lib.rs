//! Generic backend: `std::fs` walking + `pread`, portable path codec, and
//! per-OS volume identity. All `cfg` and `unsafe` live here, never in `core`.

pub mod codec;
pub mod meta;
pub mod reader;
pub mod volume;
pub mod walk;

pub use codec::GenericPathCodec;
pub use reader::FsContentReader;
pub use volume::GenericVolumeIdentity;
pub use walk::WalkdirEnumerator;

/// Convenience bundle so `cli` can pass one object around.
pub struct GenericBackend {
    pub codec: GenericPathCodec,
    pub reader: FsContentReader,
    pub enumerator: WalkdirEnumerator,
    pub volume: GenericVolumeIdentity,
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
        }
    }
}
