//! Bench harness (Phase 1 scope): synthetic 1 M-file DB: insert throughput,
//! group recompute (a proxy for resume finalize), a DB-only lookup proxy, and
//! the real `core::lookup::lookup` against a sparse query file.
//!
//! Run manually:
//!   `cargo run -p dedupe-bench --release -- [rows]`
//!   `cargo run -p dedupe-bench --release -- --rows N --lookup-file-bytes N`
//! DoD run: `cargo run -p dedupe-bench --release -- 10000000`.
//! Defaults: rows 1,000,000; lookup file 10 GiB (sparse, so no real disk use).

use std::error::Error;
use std::fs::File;
use std::path::Path;
use std::str::FromStr;
use std::time::Instant;

use dedupe_backend_generic::GenericBackend;
use dedupe_core::config::Config;
use dedupe_core::db::{queries, Db};
use dedupe_core::hash::{hash_bytes, hash_full, hash_prefix};
use dedupe_core::lookup::{self, LookupOutcome};
use dedupe_core::model::{FolderRow, FsType, Hash, State, UuidKind};
use dedupe_core::path::RawPath;
use dedupe_core::traits::VolumeInfo;

const DEFAULT_ROWS: i64 = 1_000_000;
const DEFAULT_LOOKUP_FILE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// Candidates that share the query file's prefix and full hash (expected matches).
const LOOKUP_MATCHING: usize = 4;
/// Candidates with the same size but different hashes (filtered at the prefix stage).
const LOOKUP_NON_MATCHING: usize = 4;
/// Timed calls for the real lookup. Each call streams the whole query file
/// (the full hash is not cached), so this stays small.
const LOOKUP_REAL_SAMPLES: usize = 20;

const INSERT_FILE_SQL: &str =
    "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,\
     ctime_ns,attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,\
     last_seen_run_id) VALUES(?1,?2,1,?3,?4,1,?5,0,NULL,0,?6,?7,NULL,?8,NULL,NULL,1)";

struct Args {
    rows: i64,
    lookup_file_bytes: u64,
}

fn parse_args() -> Result<Args, Box<dyn Error>> {
    let mut args = Args {
        rows: DEFAULT_ROWS,
        lookup_file_bytes: DEFAULT_LOOKUP_FILE_BYTES,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--rows" => args.rows = next_value(&mut it, "--rows")?,
            "--lookup-file-bytes" => {
                args.lookup_file_bytes = next_value(&mut it, "--lookup-file-bytes")?
            }
            _ => {
                args.rows = arg.parse().map_err(|_| {
                    format!(
                    "unknown argument `{arg}`; usage: [rows] or [--rows N] [--lookup-file-bytes N]"
                )
                })?
            }
        }
    }
    Ok(args)
}

fn next_value<T: FromStr, I: Iterator<Item = String>>(
    it: &mut I,
    flag: &str,
) -> Result<T, Box<dyn Error>> {
    let raw = it.next().ok_or_else(|| format!("{flag} needs a value"))?;
    let value = raw
        .parse::<T>()
        .map_err(|_| format!("{flag}: invalid value `{raw}`"))?;
    Ok(value)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = parse_args()?;
    let rows = args.rows;

    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("bench.db");
    let db = Db::open(&db_path, &Config::default())?;

    let info = VolumeInfo {
        uuid: "bench-vol".into(),
        uuid_kind: UuidKind::FsUuid,
        mount: RawPath::new(b"/".to_vec()),
        fs_type: FsType::Ext4,
        label: None,
    };
    let vid = queries::upsert_volume(&db.conn, &info, 0)?;
    queries::insert_folder(
        &db.conn,
        &FolderRow {
            id: 1,
            volume_id: vid,
            parent_id: 0,
            name: RawPath::empty(),
            last_seen_run_id: Some(1),
        },
    )?;

    println!("inserting {rows} synthetic rows...");
    let start = Instant::now();
    insert_rows(&db, vid, rows)?;
    let elapsed = start.elapsed();
    println!(
        "  insert: {:?} ({:.0} rows/s)",
        elapsed,
        rows as f64 / elapsed.as_secs_f64()
    );

    let start = Instant::now();
    let groups = queries::recompute_groups(&db.conn, vid, 1, 0)?;
    println!(
        "  recompute_groups: {:?} ({} groups)",
        start.elapsed(),
        groups
    );

    let start = Instant::now();
    let n = queries::size_candidates(&db.conn, vid, 1)?.len();
    println!("  size_candidates: {:?} ({} sizes)", start.elapsed(), n);

    // DB-only proxy: the per-size candidate query, with no hashing or file I/O.
    // The real lookup is measured further down.
    let sample = 2_000usize;
    let mut lats: Vec<f64> = Vec::with_capacity(sample);
    for i in 0..sample {
        let size = 1024 + ((i % 1000) as i64) * 16;
        let t = Instant::now();
        let n: i64 = db.conn.query_row(
            "SELECT COUNT(*) FROM files WHERE size=?1 AND state NOT IN (-1,-2)",
            [size],
            |r| r.get(0),
        )?;
        std::hint::black_box(n);
        lats.push(t.elapsed().as_secs_f64());
    }
    let mut sorted = lats.clone();
    println!(
        "  lookup proxy (db-only) p50: {:.3} ms, p99: {:.3} ms",
        percentile(&mut sorted, 0.50) * 1e3,
        percentile(&mut sorted, 0.99) * 1e3
    );

    // Resume proxy: the hashed-set read a resume must page in to rebuild its maps.
    let t = Instant::now();
    let _ = queries::rows_in_state(&db.conn, vid, 1, State::Done)?;
    let _ = queries::size_candidates(&db.conn, vid, 1)?;
    let _ = queries::rows_with_prefix(&db.conn, vid, 1)?;
    println!("  resume_proxy (map rebuild): {:?}", t.elapsed());

    // Real lookup: sparse query file, pre-hashed candidate rows of the same size.
    let query_path = dir.path().join("lookup-query.bin");
    let query = File::create(&query_path)?;
    query.set_len(args.lookup_file_bytes)?;
    drop(query);
    let mut lats = bench_lookup_real(&db, vid, &query_path)?;
    println!(
        "  lookup real (n={}, query {} bytes): p50 {:.3} ms, p99 {:.3} ms",
        lats.len(),
        args.lookup_file_bytes,
        percentile(&mut lats, 0.50) * 1e3,
        percentile(&mut lats, 0.99) * 1e3
    );

    println!("db at {} (temp; dropped on exit)", db_path.display());
    Ok(())
}

/// Time `core::lookup::lookup` (insert = false) on the sparse `file`.
///
/// Inserts `LOOKUP_MATCHING` candidates whose prefix and full hashes equal the
/// file's (the sparse file reads as zeros, so the hashes are computed once here)
/// plus `LOOKUP_NON_MATCHING` same-size candidates with other hashes. Every call
/// must return exactly the matching candidates. Returns per-call latencies in seconds.
fn bench_lookup_real(db: &Db, vid: i64, file: &Path) -> Result<Vec<f64>, Box<dyn Error>> {
    let backend = GenericBackend::new();
    let cfg = Config::default();
    let size = std::fs::metadata(file)?.len();

    let (q_prefix, _) = hash_prefix(&backend.reader, file, size, &cfg)?;
    let q_full = hash_full(&backend.reader, file, size)?;

    let tx = db.conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(INSERT_FILE_SQL)?;
        for k in 0..(LOOKUP_MATCHING + LOOKUP_NON_MATCHING) {
            let (prefix, full) = if k < LOOKUP_MATCHING {
                (q_prefix, q_full)
            } else {
                let other = hash_bytes(format!("other-{k}").as_bytes());
                (other, other)
            };
            let name = format!("lookup-cand-{k}");
            stmt.execute(rusqlite::params![
                None::<i64>, // NULL id: SQLite assigns the rowid
                vid,
                name.as_bytes(),
                (k as u64).to_le_bytes().to_vec(),
                size as i64,
                prefix.as_bytes(),
                full.as_bytes(),
                State::Done.as_i64(),
            ])?;
        }
    }
    tx.commit()?;

    let mut lats = Vec::with_capacity(LOOKUP_REAL_SAMPLES);
    for _ in 0..LOOKUP_REAL_SAMPLES {
        let t = Instant::now();
        let outcome = lookup::lookup(
            &db.conn,
            &backend.reader,
            &backend.codec,
            &backend.volume,
            file,
            &cfg,
            false,
        )?;
        lats.push(t.elapsed().as_secs_f64());
        match outcome {
            LookupOutcome::Matches(m) if m.len() == LOOKUP_MATCHING => {}
            other => {
                return Err(format!(
                    "real lookup: expected {LOOKUP_MATCHING} matches, got {other:?}"
                )
                .into())
            }
        }
    }
    Ok(lats)
}

/// Percentile `p` (0.0..=1.0) of `sorted`, sorting in place first.
fn percentile(sorted: &mut [f64], p: f64) -> f64 {
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn insert_rows(db: &Db, vid: i64, rows: i64) -> Result<(), Box<dyn std::error::Error>> {
    let tx = db.conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(INSERT_FILE_SQL)?;
        for i in 0..rows {
            let group = (i / 4) % 1000;
            let size = 1024 + group * 16;
            let file_id = i.to_le_bytes().to_vec();
            let name = format!("f{i}");
            let hash = Hash([(group % 251) as u8; 16]);
            stmt.execute(rusqlite::params![
                i + 1,
                vid,
                name.as_bytes(),
                file_id,
                size,
                hash.as_bytes(),
                hash.as_bytes(),
                State::Done.as_i64(),
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
}
