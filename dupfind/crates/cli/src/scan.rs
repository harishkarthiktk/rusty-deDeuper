//! `dupfind scan`. Without `--db` it runs the naive Phase 0 pipeline; with `--db`
//! it runs the persistent Phase 1 pipeline and then prints the DB report.

use anyhow::{bail, Context, Result};
use dupfind_backend_generic::GenericBackend;
use dupfind_core::cancel::Token;
use dupfind_core::config::Config;
use dupfind_core::hash::Blake3Hasher;
use dupfind_core::pipeline::scan::{run_scan, ScanOptions};
use dupfind_core::pipeline::{scan_naive, Services};
use dupfind_core::report::Report;

use crate::{OutputFormat, ScanArgs};

pub fn run(args: ScanArgs, cancel: &Token) -> Result<()> {
    let defaults = Config::default();
    let cfg = Config {
        db_path: args.db.clone(),
        include_placeholders: args.include_placeholders,
        force: args.force,
        one_file_system: !args.cross_mounts,
        mem_budget: args
            .mem_budget
            .map(|b| b as usize)
            .unwrap_or(defaults.mem_budget),
        ..defaults
    };

    if args.resume && cfg.db_path.is_none() {
        bail!("--resume requires --db");
    }

    #[cfg(feature = "test-hooks")]
    let stop_after = args.stop_after_pass.map(|n| n as i64);
    #[cfg(not(feature = "test-hooks"))]
    let stop_after = None;

    let hasher = Blake3Hasher;
    let backend = GenericBackend::new();
    let svc = Services {
        enumr: &backend.enumerator,
        reader: &backend.reader,
        codec: &backend.codec,
        volume: &backend.volume,
        hasher: &hasher,
        cancel,
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
            let dbh = dupfind_core::db::Db::open_readonly(db).context("open db for report")?;
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
