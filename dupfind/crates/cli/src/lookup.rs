//! `dupfind lookup`: does this file duplicate anything already indexed?

use anyhow::{Context, Result};
use dupfind_backend_generic::GenericBackend;
use dupfind_core::config::Config;
use dupfind_core::db::Db;
use dupfind_core::lookup::{lookup, Freshness, LookupOutcome};
use serde_json::json;

use crate::{LookupArgs, OutputFormat};

pub fn run(args: LookupArgs) -> Result<()> {
    let cfg = Config::default();
    let backend = GenericBackend::new();

    // Read-only unless we are inserting; --insert needs a writer connection.
    let conn = if args.insert {
        Db::open(&args.db, &cfg)
            .context("open database (read-write)")?
            .conn
    } else {
        Db::open_readonly(&args.db)
            .context("open database (read-only)")?
            .conn
    };

    let outcome = lookup(
        &conn,
        &backend.reader,
        &backend.codec,
        &backend.volume,
        &args.file,
        &cfg,
        args.insert,
    )
    .context("lookup failed")?;

    match args.format {
        OutputFormat::Json => println!("{}", to_json(&outcome)),
        OutputFormat::Human => print!("{}", to_human(&outcome)),
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

fn to_json(outcome: &LookupOutcome) -> String {
    let doc = match outcome {
        LookupOutcome::Skipped(reason) => {
            json!({ "outcome": "skipped", "reason": reason.to_string() })
        }
        LookupOutcome::NoMatch => json!({ "outcome": "no_match" }),
        LookupOutcome::Matches(rows) => json!({
            "outcome": "matches",
            "matches": rows.iter().map(|m| json!({
                "volume_id": m.volume_id,
                "path": m.path.to_display(),
                "freshness": freshness_str(m.freshness),
            })).collect::<Vec<_>>(),
        }),
    };
    serde_json::to_string_pretty(&doc).expect("lookup serializes")
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
                    m.path.to_display()
                ));
            }
            out
        }
    }
}
