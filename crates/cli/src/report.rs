//! `dedupe report`: read-only report from a completed database.

use std::fs::File;
use std::io::{BufWriter, IsTerminal, Write};

use anyhow::{Context, Result};
use dedupe_core::cancel::Token;
use dedupe_core::db::Db;
use dedupe_core::report::{Report, ReportEvent};
use dedupe_core::CoreError;

use crate::progress::{should_draw, ReportBar};
use crate::{ReportArgs, Resolved};

pub fn run(args: ReportArgs, cancel: &Token) -> Result<()> {
    // A file is never a terminal, so `auto` resolves to JSON when `--output` is set.
    let format = args.format.resolve(
        args.json,
        args.output.is_none() && std::io::stdout().is_terminal(),
    );
    let db = crate::resolve_existing_db(args.db, args.dir)?;
    let db = Db::open_readonly(&db).context("open database (read-only)")?;
    if let Some(run) = dedupe_core::report::unfinished_newer_run(&db.conn)? {
        eprintln!(
            "note: scan run {run} did not finish; files it touched are missing from this report \
             until `dedupe scan --resume` completes it"
        );
    }
    // JSON never draws, so the bar only exists for human output.
    let draw = should_draw(
        args.progress,
        format == Resolved::Json,
        std::io::stderr().is_terminal(),
    );
    let mut bar = draw.then(ReportBar::new);
    let mut on = |ev: ReportEvent| {
        if let Some(bar) = bar.as_mut() {
            bar.on_event(ev);
        }
    };
    let mut out: Box<dyn Write> = match &args.output {
        Some(path) => Box::new(BufWriter::new(
            File::create(path).with_context(|| format!("create {}", path.display()))?,
        )),
        None => Box::new(BufWriter::new(std::io::stdout().lock())),
    };
    match format {
        Resolved::Json => {
            let report = Report::from_db_with(&db.conn, args.min_wasted, cancel, &mut on)
                .context("build report")?;
            writeln!(out, "{}", report.to_json()).context("write report")?;
            out.flush().context("write report")?;
        }
        Resolved::Human => {
            // Groups stream out as they are built; the bar stays on stderr.
            let streamed =
                Report::stream_human(&db.conn, args.min_wasted, cancel, &mut on, &mut out);
            if let Err(CoreError::Cancelled) = streamed {
                // The header is already written, so flush it and mark the output as partial.
                let _ = out.flush();
                eprintln!("output incomplete (cancelled)");
            }
            streamed.context("build report")?;
        }
    }
    Ok(())
}
