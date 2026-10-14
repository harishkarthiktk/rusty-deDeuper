//! Bench harness (Phase 1 scope): synthetic 1 M-file DB — insert throughput,
//! group recompute (a proxy for resume finalize) and lookup latency.
//!
//! Run manually: `cargo run -p dupfind-bench --release -- [rows]`.

use std::time::Instant;

use dupfind_core::config::Config;
use dupfind_core::db::{queries, Db};
use dupfind_core::model::{FolderRow, FsType, Hash, State, UuidKind};
use dupfind_core::path::RawPath;
use dupfind_core::traits::VolumeInfo;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rows: i64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);

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
    let groups = queries::recompute_groups(&db.conn, vid, 0)?;
    println!(
        "  recompute_groups: {:?} ({} groups)",
        start.elapsed(),
        groups
    );

    let start = Instant::now();
    let n = queries::size_candidates(&db.conn, vid, 1)?.len();
    println!("  size_candidates: {:?} ({} sizes)", start.elapsed(), n);

    println!("db at {} (temp; dropped on exit)", db_path.display());
    Ok(())
}

fn insert_rows(db: &Db, vid: i64, rows: i64) -> Result<(), Box<dyn std::error::Error>> {
    let tx = db.conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,\
             ctime_ns,attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,\
             last_seen_run_id) VALUES(?1,?2,1,?3,?4,1,?5,0,NULL,0,?6,?7,NULL,?8,NULL,NULL,1)",
        )?;
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
