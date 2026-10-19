//! `dedupe lookup`: does this file duplicate anything already indexed?

use std::io::IsTerminal;

use anyhow::{Context, Result};
use dedupe_backend_generic::GenericBackend;
use dedupe_core::config::Config;
use dedupe_core::db::Db;
use dedupe_core::lookup::{lookup_report, Freshness, InsertStatus, LookupOutcome, LookupReport};
use serde_json::json;

use crate::{LookupArgs, Resolved};

pub fn run(args: LookupArgs) -> Result<()> {
    let format = args
        .format
        .resolve(args.json, std::io::stdout().is_terminal());
    let cfg = Config::default();
    let backend = GenericBackend::new();

    // Read-only lookups require an existing DB; --insert may target a fresh one.
    let db = if args.insert {
        crate::insert_db_target(args.db, args.dir)?
    } else {
        crate::resolve_existing_db(args.db, args.dir)?
    };

    // --insert is a writer: exclude scans and other inserts for its whole run. Read-only
    // lookups take no lock (WAL readers are safe).
    let _lock = if args.insert {
        Some(
            dedupe_core::db::lock::acquire(&db)
                .context("another dedupe process is using this database")?,
        )
    } else {
        None
    };

    let conn = if args.insert {
        Db::open(&db, &cfg)
            .context("open database (read-write)")?
            .conn
    } else {
        Db::open_readonly(&db)
            .context("open database (read-only)")?
            .conn
    };

    let report = lookup_report(
        &conn,
        &backend.reader,
        &backend.codec,
        &backend.volume,
        &args.file,
        &cfg,
        args.insert,
    )
    .context("lookup failed")?;

    match format {
        Resolved::Json => println!("{}", to_json(&report)?),
        Resolved::Human => {
            print!("{}", to_human(&report.outcome));
            if let Some(note) = insert_note(report.insert) {
                eprintln!("note: {note}");
            }
        }
    }
    Ok(())
}

fn freshness_str(f: Freshness) -> &'static str {
    match f {
        Freshness::Fresh => "fresh",
        Freshness::Stale => "stale",
        Freshness::Missing => "missing",
        Freshness::Unmounted => "unmounted",
    }
}

fn insert_str(s: InsertStatus) -> &'static str {
    match s {
        InsertStatus::NotRequested => "not_requested",
        InsertStatus::Inserted { .. } => "inserted",
        InsertStatus::Updated { .. } => "updated",
        InsertStatus::Unchanged { .. } => "unchanged",
        InsertStatus::SkippedVolumeNotIndexed => "skipped_volume",
        InsertStatus::SkippedFolderNotIndexed => "skipped_folder",
    }
}

/// Stderr note for an `--insert` that did not write the query row.
fn insert_note(s: InsertStatus) -> Option<&'static str> {
    match s {
        InsertStatus::SkippedVolumeNotIndexed => Some("not inserted: volume not indexed"),
        InsertStatus::SkippedFolderNotIndexed => Some("not inserted: folder not indexed"),
        _ => None,
    }
}

fn to_json(report: &LookupReport) -> Result<String> {
    let mut doc = match &report.outcome {
        LookupOutcome::Skipped(reason) => {
            json!({ "outcome": "skipped", "reason": reason.to_string() })
        }
        LookupOutcome::NoMatch => json!({ "outcome": "no_match" }),
        LookupOutcome::Matches(rows) => json!({
            "outcome": "matches",
            "matches": rows.iter().map(|m| {
                let mut o = serde_json::Map::new();
                o.insert("volume_id".into(), m.volume_id.into());
                m.path.json_fields(&mut o);
                o.insert("freshness".into(), freshness_str(m.freshness).into());
                serde_json::Value::Object(o)
            }).collect::<Vec<_>>(),
        }),
    };
    if report.insert != InsertStatus::NotRequested {
        doc["insert"] = json!(insert_str(report.insert));
    }
    serde_json::to_string_pretty(&doc).context("serialize lookup result")
}

fn to_human(outcome: &LookupOutcome) -> String {
    match outcome {
        LookupOutcome::Skipped(reason) => format!("skipped: {reason}\n"),
        LookupOutcome::NoMatch => "no match\n".to_string(),
        LookupOutcome::Matches(rows) => {
            let mut out = format!("{} match(es)\n", rows.len());
            for m in rows {
                out.push_str(&format!(
                    "    [{}] {}\n",
                    freshness_str(m.freshness),
                    m.path.to_terminal()
                ));
            }
            out
        }
    }
}
