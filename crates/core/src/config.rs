//! All tunables in one struct, read by every phase. No globals.

use std::path::PathBuf;

use crate::exclude::ExcludeSet;
use crate::hash::HASH_ALGO;
use crate::storage_profile::Profile;

/// Whether `pass_verify` runs. `Byte` lands in Phase 2; Phase 1 only knows `Off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum VerifyMode {
    #[default]
    Off,
    Byte,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// SQLite path. `None` => Phase 0 naive scan with no persistence.
    pub db_path: Option<PathBuf>,
    /// Prefix first segment size (default 16 KiB).
    pub prefix_first_bytes: u64,
    /// Prefix last segment size (default 4 KiB).
    pub prefix_last_bytes: u64,
    /// Budget for the in-memory maps (default 1 GiB).
    pub mem_budget: usize,
    /// Do not cross mount points while walking (default true).
    pub one_file_system: bool,
    /// Include cloud placeholders instead of skipping them (default false).
    pub include_placeholders: bool,
    pub verify: VerifyMode,
    /// Override the hash-config / volume-UUID guards.
    pub force: bool,
    /// `meta.hash_algo`.
    pub hash_algo: String,
    /// Resolved I/O policy. The CLI probes storage and sets this; the default is
    /// the SSD policy. Not part of the hash-config guard, since it does not change
    /// hash output.
    pub profile: Profile,
    /// Paths matching any of these globs are not enumerated. Not part of the hash-config
    /// guard, since it does not change hash output.
    pub exclude: ExcludeSet,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            db_path: None,
            prefix_first_bytes: 16 * 1024,
            prefix_last_bytes: 4 * 1024,
            mem_budget: 1 << 30,
            one_file_system: true,
            include_placeholders: false,
            verify: VerifyMode::Off,
            force: false,
            hash_algo: HASH_ALGO.to_string(),
            profile: Profile::default(),
            exclude: ExcludeSet::default(),
        }
    }
}

impl Config {
    /// `F + L`: the size at or below which prefix == full (4.4a rule 4).
    pub fn prefix_total(&self) -> u64 {
        self.prefix_first_bytes + self.prefix_last_bytes
    }
}
