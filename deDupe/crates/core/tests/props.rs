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

fn name_strategy() -> impl Strategy<Value = Vec<u8>> {
    // Unix: any bytes except NUL and `/`; weights toward invalid UTF-8.
    vec(any::<u8>(), 0..48).prop_filter("no NUL or slash", |v| {
        !v.iter().any(|&b| b == 0 || b == b'/')
    })
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
        let got = queries::find_by_path(&db.conn, vid, 1, &name).unwrap().unwrap();
        prop_assert_eq!(got.name.as_bytes(), e.name.as_bytes());
        prop_assert_eq!(got, e);
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
