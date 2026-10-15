//! `dedupe db`: info / vacuum / drop.

use std::fs;

use anyhow::{bail, Context, Result};
use dedupe_core::config::Config;
use dedupe_core::db::{queries, schema, Db};

use crate::{DbAction, DbArgs};

pub fn run(args: DbArgs) -> Result<()> {
    match args.action {
        DbAction::Info => info(&crate::resolve_existing_db(args.db)?),
        DbAction::Vacuum => {
            let db = crate::resolve_existing_db(args.db)?;
            let dbh = Db::open(&db, &Config::default()).context("open database")?;
            dbh.conn.execute_batch("VACUUM").context("vacuum")?;
            println!("vacuumed {}", db.display());
            Ok(())
        }
        DbAction::Drop => {
            let db = match args.db {
                Some(p) => p,
                None => crate::cwd_default_db()?,
            };
            if !args.yes {
                bail!("refusing to drop without --yes (irreversible)");
            }
            for suffix in ["", "-wal", "-shm"] {
                let p = std::path::PathBuf::from(format!("{}{suffix}", db.display()));
                if p.exists() {
                    fs::remove_file(&p).with_context(|| format!("remove {}", p.display()))?;
                }
            }
            println!("dropped {}", db.display());
            Ok(())
        }
    }
}

fn info(path: &std::path::Path) -> Result<()> {
    let db = Db::open_readonly(path).context("open database (read-only)")?;
    let version = schema::current_version(&db.conn)?;
    println!("schema_version: {version}");
    println!("groups_total:   {}", queries::group_count(&db.conn)?);

    for v in queries::list_volumes(&db.conn)? {
        println!(
            "volume {}: uuid={} kind={} fs={} profile={} mount={}",
            v.id,
            v.uuid,
            v.uuid_kind,
            v.fs_type,
            v.profile,
            v.mount.to_display()
        );
        let states = queries::counts_by_state(&db.conn, v.id)?;
        let counts: Vec<String> = states.iter().map(|(s, n)| format!("{s}:{n}")).collect();
        println!("    states: {}", counts.join(" "));
        if let Some(run) = queries::latest_finished_run(&db.conn, v.id)? {
            println!(
                "    latest_run: {run}  stale_rows: {}",
                queries::stale_count(&db.conn, v.id, run)?
            );
        }
    }
    Ok(())
}
