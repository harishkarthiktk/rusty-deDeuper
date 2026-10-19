//! Property tests: `FileEntry -> DB -> FileEntry` round-trip preserves every field,
//! including raw (possibly invalid-UTF-8) name bytes.

use dedupe_core::config::Config;
use dedupe_core::db::{queries, Db};
use dedupe_core::model::{
    is_legal_transition, FileEntry, FolderRow, FsType, Hash, State, UuidKind,
};
use dedupe_core::path::RawPath;
use dedupe_core::traits::VolumeInfo;
use proptest::collection::vec;
use proptest::prelude::*;
use proptest::test_runner::TestCaseError;
use unicode_normalization::UnicodeNormalization;

fn name_strategy() -> impl Strategy<Value = Vec<u8>> {
    // Unix: any bytes except NUL and `/`; weights toward invalid UTF-8.
    vec(any::<u8>(), 0..48).prop_filter("no NUL or slash", |v| {
        !v.iter().any(|&b| b == 0 || b == b'/')
    })
}

/// Windows: UTF-16 units including unpaired surrogates, excluding NUL and the
/// reserved characters `\ / : * ? " < > |`, encoded as WTF-8.
#[cfg(windows)]
fn wide_name_strategy() -> impl Strategy<Value = Vec<u8>> {
    const RESERVED: &[u16] = &[
        0,
        b'\\' as u16,
        b'/' as u16,
        b':' as u16,
        b'*' as u16,
        b'?' as u16,
        b'"' as u16,
        b'<' as u16,
        b'>' as u16,
        b'|' as u16,
    ];
    // Weighted toward the surrogate block so unpaired halves are common.
    let unit = prop_oneof![
        3 => any::<u16>(),
        1 => 0xD800u16..=0xDFFF,
    ]
    .prop_filter("no NUL or reserved", |u| !RESERVED.contains(u));
    vec(unit, 0..24).prop_map(|units| wtf8_encode(&units))
}

/// Valid surrogate pairs become UTF-8; unpaired surrogates become 3-byte
/// generalized UTF-8, the WTF-8 form `backend-generic` uses for Windows names.
#[cfg(windows)]
fn wtf8_encode(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::with_capacity(units.len() * 3);
    for r in char::decode_utf16(units.iter().copied()) {
        match r {
            Ok(c) => {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
            Err(e) => {
                let cp = u32::from(e.unpaired_surrogate());
                out.extend_from_slice(&[
                    0xE0 | (cp >> 12) as u8,
                    0x80 | ((cp >> 6) & 0x3F) as u8,
                    0x80 | (cp & 0x3F) as u8,
                ]);
            }
        }
    }
    out
}

/// A name and its NFC and NFD spellings. The two are distinct byte strings.
fn nfc_nfd_pair() -> impl Strategy<Value = (String, String)> {
    "[a-zA-Z\u{00C0}-\u{017F}]{1,16}"
        .prop_map(|s| (s.nfc().collect::<String>(), s.nfd().collect::<String>()))
        .prop_filter("forms differ", |(nfc, nfd)| nfc != nfd)
}

/// Stores one file entry named `name` and checks every field survives the round trip.
fn check_round_trip(name: Vec<u8>, size: i64, mtime: i64, ino: u64) -> Result<(), TestCaseError> {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("p.db");
    let db = Db::open(&path, &Config::default()).unwrap();

    let info = VolumeInfo {
        uuid: "prop-vol".into(),
        uuid_kind: UuidKind::FsUuid,
        mount: RawPath::new(b"/".to_vec()),
        fs_type: FsType::Ext4,
        label: None,
    };
    let vid = queries::upsert_volume(&db.conn, &info, 0).unwrap();
    queries::insert_folder(
        &db.conn,
        &FolderRow {
            id: 1,
            volume_id: vid,
            parent_id: 0,
            name: RawPath::empty(),
            last_seen_run_id: Some(1),
        },
    )
    .unwrap();

    let name = RawPath::new(name);
    let mut e = FileEntry::enumerated(5, vid, 1, name.clone(), 1);
    e.size = size;
    e.mtime_ns = mtime;
    e.file_id = Some(ino.to_le_bytes().to_vec());
    e.nlink = 1;
    e.state = State::Done;
    e.prefix_hash = Some(Hash([1; 16]));
    e.full_hash = Some(Hash([2; 16]));
    e.prefix_bytes = Some(size);

    queries::upsert_file(&db.conn, &e).unwrap();
    let got = queries::find_by_path(&db.conn, vid, 1, &name)
        .unwrap()
        .unwrap();
    prop_assert_eq!(got.name.as_bytes(), e.name.as_bytes());
    prop_assert_eq!(got, e);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn file_entry_round_trip(
        name in name_strategy(),
        size in 0i64..1_000_000,
        mtime in any::<i64>(),
        ino in any::<u64>(),
    ) {
        check_round_trip(name, size, mtime, ino)?;
    }

    /// NFC and NFD spellings stay distinct raw names, but display folds both to NFC.
    #[test]
    fn nfc_nfd_names_are_distinct_but_display_equal((nfc, nfd) in nfc_nfd_pair()) {
        let a = RawPath::new(nfc.as_bytes());
        let b = RawPath::new(nfd.as_bytes());
        prop_assert_ne!(&a, &b);
        prop_assert_eq!(a.to_display(), nfc.clone());
        prop_assert_eq!(b.to_display(), nfc);
    }
}

#[cfg(windows)]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn wide_file_entry_round_trip(name in wide_name_strategy(), size in 0i64..1_000_000) {
        check_round_trip(name, size, 0, 1)?;
    }
}

/// Any of the ten `State` variants (see 4.4).
fn state() -> impl Strategy<Value = State> {
    use State::*;
    prop_oneof![
        Just(Enumerated),
        Just(SizeCandidate),
        Just(PrefixHashed),
        Just(FullHashed),
        Just(Verified),
        Just(Done),
        Just(Skipped),
        Just(Error),
        Just(UniqueSize),
        Just(UniquePrefix),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1024))]

    /// Terminal states are never revisited within a run (4.4): they have no
    /// outgoing legal transition.
    #[test]
    fn terminal_states_have_no_outgoing_edges(from in state(), to in state()) {
        prop_assert!(!(from.is_terminal() && is_legal_transition(from, to)));
    }

    /// A random walk that only takes legal transitions never reaches an invalid
    /// state and never leaves a terminal state.
    #[test]
    fn random_walk_stays_valid(step in vec(state(), 0..128)) {
        let mut cur = State::Enumerated;
        for to in step {
            let legal = is_legal_transition(cur, to);
            prop_assert!(!legal || !cur.is_terminal());
            if legal {
                cur = to;
            }
            prop_assert!(State::from_i64(cur.as_i64()).is_some());
        }
    }
}
