//! `dupfind report`: read-only report from a completed database.

use anyhow::{Context, Result};
use dupfind_core::config::Config;
use dupfind_core::db::Db;
use dupfind_core::report::Report;

use crate::{OutputFormat, ReportArgs};

pub fn run(args: ReportArgs) -> Result<()> {
    let db = Db::open_readonly(&args.db).context("open database (read-only)")?;
    let report = Report::from_db(&db.conn, args.min_wasted).context("build report")?;
    match args.format {
        OutputFormat::Json => println!("{}", report.to_json()),
        OutputFormat::Human => print!("{}", report.to_human()),
    }
    let _ = Config::default();
    Ok(())
}
