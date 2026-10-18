//! `dedupe report`: read-only report from a completed database.

use std::io::{BufWriter, IsTerminal, Write};

use anyhow::{Context, Result};
use dedupe_core::cancel::Token;
use dedupe_core::db::Db;
use dedupe_core::report::{Report, ReportEvent};
use dedupe_core::CoreError;

use crate::progress::{should_draw, ReportBar};
use crate::{OutputFormat, ReportArgs};

pub fn run(args: ReportArgs, cancel: &Token) -> Result<()> {
    let db = crate::resolve_existing_db(args.db)?;
    let db = Db::open_readonly(&db).context("open database (read-only)")?;
    // JSON never draws, so the bar only exists for human output.
    let draw = should_draw(
        args.progress,
        args.format == OutputFormat::Json,
        std::io::stderr().is_terminal(),
    );
    let mut bar = draw.then(ReportBar::new);
    let mut on = |ev: ReportEvent| {
        if let Some(bar) = bar.as_mut() {
            bar.on_event(ev);
        }
    };
    match args.format {
        OutputFormat::Json => {
            let report = Report::from_db_with(&db.conn, args.min_wasted, cancel, &mut on)
                .context("build report")?;
            println!("{}", report.to_json());
        }
        OutputFormat::Human => {
            // Groups stream to stdout as they are built; the bar stays on stderr.
            let mut out = BufWriter::new(std::io::stdout().lock());
            let streamed =
                Report::stream_human(&db.conn, args.min_wasted, cancel, &mut on, &mut out);
            if let Err(CoreError::Cancelled) = streamed {
                // The header is already on stdout, so flush it and mark the output as partial.
                let _ = out.flush();
                eprintln!("output incomplete (cancelled)");
            }
            streamed.context("build report")?;
        }
    }
    Ok(())
}
