//! Unit tests of facade modules whose cases depend on the target OS.

#[cfg(unix)]
use crate::probe::{fs_locality, FsLocality};

mod meta;
#[cfg(unix)]
mod probe;
mod reader;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod volume;
