//! Backend for every non-Windows build: the generic walker and positional reads.

use dedupe_backend_generic::GenericBackend;
use dedupe_core::cancel::Token;
use dedupe_core::storage_profile::Profile;
use dedupe_core::traits::ContentReader;

use super::{Backend, ScanExtras};
use crate::ScanArgs;

/// Nothing to carry on this platform.
pub struct Extras;

impl ScanExtras for Extras {
    fn enumerator_label(&self, _no_mft: bool) -> &'static str {
        "walker"
    }

    fn drain_notices(&self) -> Vec<String> {
        Vec::new()
    }
}

// Takes no observer: the `$MFT` path does not exist here, so there is nothing to draw.
pub fn make_backend(
    _args: &ScanArgs,
    _profile: &Profile,
    _cancel: &Token,
    _multi: Option<indicatif::MultiProgress>,
) -> Backend {
    let generic = GenericBackend::new();
    Backend {
        enumerator: Box::new(generic.enumerator),
        reader: Box::new(generic.reader),
        codec: generic.codec,
        volume: generic.volume,
        extras: Extras,
    }
}

/// The content reader alone, for commands that need no enumerator (`lookup`).
pub fn make_reader(_profile: &Profile) -> Box<dyn ContentReader> {
    Box::new(GenericBackend::new().reader)
}
