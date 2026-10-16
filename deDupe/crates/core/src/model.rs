//! Core data model: ids, hashes, states, phases, attributes and the row structs.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::path::RawPath;

pub type VolumeId = i64;
pub type FileId = i64;

/// A 128-bit BLAKE3 hash (first 16 bytes of the digest).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash(pub [u8; 16]);

impl Hash {
    pub const LEN: usize = 16;

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Hash> {
        if bytes.len() == Self::LEN {
            let mut a = [0u8; Self::LEN];
            a.copy_from_slice(bytes);
            Some(Hash(a))
        } else {
            None
        }
    }

    pub fn to_hex(&self) -> String {
        let mut s = String::with_capacity(Self::LEN * 2);
        for b in self.0 {
            s.push(char::from_digit((b >> 4) as u32, 16).unwrap());
            s.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
        }
        s
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.to_hex())
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl AsRef<[u8]> for Hash {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// `files.state`. See `.plan.md` 4.4.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(i64)]
pub enum State {
    #[default]
    Enumerated = 0,
    SizeCandidate = 1,
    PrefixHashed = 2,
    FullHashed = 3,
    Verified = 4,
    Done = 5,
    Skipped = -1,
    Error = -2,
    UniqueSize = -3,
    UniquePrefix = -4,
}

impl State {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(v: i64) -> Option<State> {
        Some(match v {
            0 => State::Enumerated,
            1 => State::SizeCandidate,
            2 => State::PrefixHashed,
            3 => State::FullHashed,
            4 => State::Verified,
            5 => State::Done,
            -1 => State::Skipped,
            -2 => State::Error,
            -3 => State::UniqueSize,
            -4 => State::UniquePrefix,
            _ => return None,
        })
    }

    /// Terminal states are never revisited within a run.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            State::Done | State::Skipped | State::Error | State::UniqueSize | State::UniquePrefix
        )
    }

    /// States that hold a `full_hash` and are eligible for reporting.
    pub fn is_hashed(self) -> bool {
        matches!(self, State::FullHashed | State::Verified | State::Done)
    }
}

/// Whether `from -> to` is a legal transition within a run (4.4). Includes the
/// merged-pass exceptions `SizeCandidate -> Done` (small files / HDD merged pass) and
/// `SizeCandidate -> FullHashed` (small files under `VerifyMode::Byte`).
pub fn is_legal_transition(from: State, to: State) -> bool {
    use State::*;
    match from {
        Enumerated => matches!(to, SizeCandidate | UniqueSize | Skipped | Error),
        SizeCandidate => {
            matches!(
                to,
                PrefixHashed | UniquePrefix | FullHashed | Done | Skipped | Error
            )
        }
        PrefixHashed => matches!(to, FullHashed | Done | Skipped | Error),
        FullHashed => matches!(to, Verified | Done | Error),
        Verified => matches!(to, Done),
        Done | Skipped | Error | UniqueSize | UniquePrefix => false,
    }
}

/// `run_volumes.phase_reached`. See `.plan.md` 4.4b.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Phase {
    None = 0,
    Enumerated = 1,
    SizeDone = 2,
    PrefixDone = 3,
    FullDone = 4,
    Verified = 5,
    GroupsDone = 6,
}

impl Phase {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(v: i64) -> Option<Phase> {
        Some(match v {
            0 => Phase::None,
            1 => Phase::Enumerated,
            2 => Phase::SizeDone,
            3 => Phase::PrefixDone,
            4 => Phase::FullDone,
            5 => Phase::Verified,
            6 => Phase::GroupsDone,
            _ => return None,
        })
    }

    /// The pass that runs immediately after this one is fully committed.
    pub fn next(self) -> Phase {
        match self {
            Phase::None => Phase::Enumerated,
            Phase::Enumerated => Phase::SizeDone,
            Phase::SizeDone => Phase::PrefixDone,
            Phase::PrefixDone => Phase::FullDone,
            Phase::FullDone => Phase::Verified,
            Phase::Verified => Phase::GroupsDone,
            Phase::GroupsDone => Phase::GroupsDone,
        }
    }
}

/// `files.attrs` bitfield. Reserves room for reflink/clone labels in later phases.
pub mod attrs {
    pub const HIDDEN: i64 = 1 << 0;
    pub const SYSTEM: i64 = 1 << 1;
    pub const READONLY: i64 = 1 << 2;
    pub const SPARSE: i64 = 1 << 3;
    pub const REFLINK: i64 = 1 << 4;
    pub const HARDLINK: i64 = 1 << 5;
    pub const SYMLINK: i64 = 1 << 6;
    pub const PLACEHOLDER: i64 = 1 << 7;

    pub fn has(value: i64, flag: i64) -> bool {
        value & flag != 0
    }
}

/// Why a file was skipped (never persisted directly; maps to `State::Skipped`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    ZeroLength,
    NonRegular,
    Symlink,
    Placeholder,
    Filtered,
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            SkipReason::ZeroLength => "zero-length",
            SkipReason::NonRegular => "non-regular",
            SkipReason::Symlink => "symlink",
            SkipReason::Placeholder => "cloud placeholder",
            SkipReason::Filtered => "filtered",
        })
    }
}

/// `volumes.fs_type`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsType {
    Ntfs,
    Fat32,
    Exfat,
    Ext4,
    Xfs,
    Btrfs,
    Apfs,
    Other,
}

impl FsType {
    pub fn as_str(self) -> &'static str {
        match self {
            FsType::Ntfs => "ntfs",
            FsType::Fat32 => "fat32",
            FsType::Exfat => "exfat",
            FsType::Ext4 => "ext4",
            FsType::Xfs => "xfs",
            FsType::Btrfs => "btrfs",
            FsType::Apfs => "apfs",
            FsType::Other => "other",
        }
    }

    pub fn parse(s: &str) -> FsType {
        match s.to_ascii_lowercase().as_str() {
            "ntfs" => FsType::Ntfs,
            "fat32" | "vfat" | "fat" => FsType::Fat32,
            "exfat" => FsType::Exfat,
            "ext4" | "ext2" | "ext3" => FsType::Ext4,
            "xfs" => FsType::Xfs,
            "btrfs" => FsType::Btrfs,
            "apfs" | "hfs" | "hfs+" => FsType::Apfs,
            _ => FsType::Other,
        }
    }
}

/// `volumes.uuid_kind`. See `.plan.md` 4.10.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UuidKind {
    FsUuid,
    VolumeSerial,
    FsidFallback,
    None,
}

impl UuidKind {
    pub fn as_str(self) -> &'static str {
        match self {
            UuidKind::FsUuid => "fs_uuid",
            UuidKind::VolumeSerial => "volume_serial",
            UuidKind::FsidFallback => "fsid_fallback",
            UuidKind::None => "none",
        }
    }

    pub fn parse(s: &str) -> UuidKind {
        match s {
            "fs_uuid" => UuidKind::FsUuid,
            "volume_serial" => UuidKind::VolumeSerial,
            "fsid_fallback" => UuidKind::FsidFallback,
            _ => UuidKind::None,
        }
    }
}

/// `errors.kind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum ErrorKind {
    Permission = 0,
    Io = 1,
    TooLong = 2,
    ChangedDuringRead = 3,
    Other = 4,
    VerifyMismatch = 5,
}

impl ErrorKind {
    pub fn as_i64(self) -> i64 {
        self as i64
    }
}

/// One `files` row. `id` is pre-assigned by [`crate::db::ids::IdAllocator`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileEntry {
    pub id: FileId,
    pub volume_id: VolumeId,
    pub folder_id: i64,
    pub name: RawPath,
    pub file_id: Option<Vec<u8>>,
    pub nlink: i64,
    pub size: i64,
    pub mtime_ns: i64,
    pub ctime_ns: Option<i64>,
    pub attrs: i64,
    pub prefix_hash: Option<Hash>,
    pub full_hash: Option<Hash>,
    pub prefix_bytes: Option<i64>,
    pub state: State,
    pub err: Option<i64>,
    pub scanned_at: Option<i64>,
    pub last_seen_run_id: i64,
}

impl FileEntry {
    /// A freshly enumerated row (state 0, no hashes).
    pub fn enumerated(
        id: FileId,
        volume_id: VolumeId,
        folder_id: i64,
        name: RawPath,
        run_id: i64,
    ) -> Self {
        FileEntry {
            id,
            volume_id,
            folder_id,
            name,
            file_id: None,
            nlink: 1,
            size: 0,
            mtime_ns: 0,
            ctime_ns: None,
            attrs: 0,
            prefix_hash: None,
            full_hash: None,
            prefix_bytes: None,
            state: State::Enumerated,
            err: None,
            scanned_at: None,
            last_seen_run_id: run_id,
        }
    }
}

/// One `folders` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub volume_id: VolumeId,
    pub parent_id: i64,
    pub name: RawPath,
    pub last_seen_run_id: Option<i64>,
}

/// One `errors` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrRow {
    pub run_id: i64,
    pub file_row_id: Option<i64>,
    pub path: Option<RawPath>,
    pub kind: ErrorKind,
    pub message: Option<String>,
}

/// Nanoseconds since the Unix epoch, or 0 if the clock is before the epoch.
pub fn now_unix_ns() -> i64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_nanos() as i64,
        Err(_) => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trip() {
        for v in [-4i64, -3, -2, -1, 0, 1, 2, 3, 4, 5] {
            let s = State::from_i64(v).unwrap();
            assert_eq!(s.as_i64(), v);
        }
        assert!(State::from_i64(6).is_none());
    }

    #[test]
    fn phase_order_and_round_trip() {
        for v in 0..=6i64 {
            let p = Phase::from_i64(v).unwrap();
            assert_eq!(p.as_i64(), v);
        }
        assert!(Phase::Enumerated > Phase::None);
        assert_eq!(Phase::None.next(), Phase::Enumerated);
        assert_eq!(Phase::GroupsDone.next(), Phase::GroupsDone);
    }

    #[test]
    fn hash_hex() {
        let h = Hash([0xab; 16]);
        assert_eq!(h.to_hex().len(), 32);
        assert!(h.to_hex().starts_with("abab"));
    }

    #[test]
    fn transition_legality() {
        use State::*;
        assert!(is_legal_transition(Enumerated, SizeCandidate));
        assert!(is_legal_transition(SizeCandidate, Done)); // merged-pass exception
        assert!(is_legal_transition(FullHashed, Done));
        assert!(!is_legal_transition(Enumerated, Done));
        assert!(!is_legal_transition(Done, Enumerated));
        assert!(!is_legal_transition(Skipped, Enumerated));
        assert!(!is_legal_transition(UniqueSize, Enumerated));
    }
}
