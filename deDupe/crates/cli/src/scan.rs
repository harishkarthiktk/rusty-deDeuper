//! `dedupe scan`. Persists by default (`.dedupe.db` under the first root); `--no-db`
//! runs the naive Phase 0 pipeline with no persistence; `--db` selects an explicit path.

use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use dedupe_backend_generic::GenericBackend;
use dedupe_core::cancel::Token;
use dedupe_core::config::Config;
use dedupe_core::hash::Blake3Hasher;
use dedupe_core::pipeline::scan::{run_scan, ScanOptions};
use dedupe_core::pipeline::{scan_naive, Services};
use dedupe_core::progress::SharedSink;
use dedupe_core::report::Report;

use crate::progress::{should_draw, BarSink};
use crate::{OutputFormat, ScanArgs};

pub fn run(args: ScanArgs, cancel: &Token) -> Result<()> {
    // Persistence mode: --no-db -> naive (no persistence); --db -> explicit path;
    // otherwise default to <first root>/.dedupe.db.
    let db_path = if args.no_db {
        None
    } else if let Some(db) = args.db.clone() {
        Some(db)
    } else {
        Some(crate::scan_default_db(&args.roots)?)
    };

    let defaults = Config::default();
    let cfg = Config {
        db_path,
        include_placeholders: args.include_placeholders,
        force: args.force,
        one_file_system: !args.cross_mounts,
        mem_budget: args
            .mem_budget
            .map(|b| b as usize)
            .unwrap_or(defaults.mem_budget),
        ..defaults
    };

    #[cfg(feature = "test-hooks")]
    let stop_after = args.stop_after_pass.map(|n| n as i64);
    #[cfg(not(feature = "test-hooks"))]
    let stop_after = None;

    let hasher = Blake3Hasher;
    let backend = GenericBackend::new();
    // Only the DB path reports progress; `--no-db` (naive) keeps the null sink.
    let draw = should_draw(
        args.progress,
        args.format == OutputFormat::Json,
        std::io::stderr().is_terminal(),
    );
    let sink: SharedSink = if draw {
        Arc::new(Mutex::new(BarSink::new()))
    } else {
        dedupe_core::progress::null_sink()
    };
    let svc = Services {
        enumr: &backend.enumerator,
        reader: &backend.reader,
        codec: &backend.codec,
        volume: &backend.volume,
        hasher: &hasher,
        cancel,
        sink: &sink,
    };

    match &cfg.db_path {
        None => {
            let (report, _stats) = scan_naive(
                &args.roots,
                svc.enumr,
                svc.reader,
                svc.hasher,
                svc.codec,
                &cfg,
                cancel,
            )
            .context("scan failed")?;
            emit(&report, args.format);
        }
        Some(db) => {
            let opts = ScanOptions {
                cli_args: std::env::args().collect::<Vec<_>>().join(" "),
                resume: args.resume,
                stop_after,
            };
            let run_id = run_scan(db, &args.roots, &cfg, &svc, &opts).context("scan failed")?;
            let dbh = dedupe_core::db::Db::open_readonly(db).context("open db for report")?;
            let report = Report::from_db(&dbh.conn, 0).context("build report")?;
            emit(&report, args.format);
            eprintln!("scan run {run_id} finished");
        }
    }
    Ok(())
}

fn emit(report: &Report, format: OutputFormat) {
    match format {
        OutputFormat::Json => println!("{}", report.to_json()),
        OutputFormat::Human => print!("{}", report.to_human()),
    }
}
