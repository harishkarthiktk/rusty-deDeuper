//! `dedupe report`: read-only report from a completed database.

use anyhow::{Context, Result};
use dedupe_core::db::Db;
use dedupe_core::report::Report;

use crate::{OutputFormat, ReportArgs};

pub fn run(args: ReportArgs) -> Result<()> {
    let db = crate::resolve_existing_db(args.db)?;
    let db = Db::open_readonly(&db).context("open database (read-only)")?;
    let report = Report::from_db(&db.conn, args.min_wasted).context("build report")?;
    match args.format {
        OutputFormat::Json => println!("{}", report.to_json()),
        OutputFormat::Human => print!("{}", report.to_human()),
    }
    Ok(())
}
