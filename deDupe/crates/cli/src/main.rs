//! `dedupe` command-line entry point. All logic lives in `core`; this is a thin
//! consumer that parses flags, builds a `Config`, and prints results.

mod db;
mod lookup;
mod report;
mod scan;

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand, ValueEnum};
use dedupe_core::cancel::Token;

#[derive(Parser, Debug)]
#[command(name = "dedupe", version, about = "Duplicate file finder")]
struct Cli {
    #[arg(long, global = true, value_enum, default_value_t = LogLevel::Info)]
    log_level: LogLevel,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Json,
    Human,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Scan one or more roots for duplicate files.
    Scan(ScanArgs),
    /// Print duplicate groups from a completed database.
    Report(ReportArgs),
    /// Check whether a file duplicates anything already indexed.
    Lookup(LookupArgs),
    /// Database maintenance.
    Db(DbArgs),
}

#[derive(Args, Debug)]
pub struct ScanArgs {
    /// Roots to scan.
    #[arg(required = true)]
    pub roots: Vec<PathBuf>,

    /// Persist state to this SQLite database (enables resume/report/lookup).
    /// Defaults to `<first root>/.dedupe.db` when omitted.
    #[arg(long, conflicts_with = "no_db")]
    pub db: Option<PathBuf>,

    /// Do not persist any state; print the report only.
    #[arg(long, conflicts_with_all = ["db", "resume"])]
    pub no_db: bool,

    /// Resume the latest interrupted run (uses the default DB when --db is omitted).
    #[arg(long, conflicts_with = "no_db")]
    pub resume: bool,

    /// Override the hash-config / volume-UUID guards.
    #[arg(long)]
    pub force: bool,

    /// Do not descend across mount points (default).
    #[arg(long, conflicts_with = "cross_mounts")]
    pub one_file_system: bool,

    /// Descend across mount points.
    #[arg(long)]
    pub cross_mounts: bool,

    /// Include cloud placeholders instead of skipping them.
    #[arg(long)]
    pub include_placeholders: bool,

    /// Budget for the in-memory maps, in bytes.
    #[arg(long)]
    pub mem_budget: Option<u64>,

    /// Test hook: stop right after committing phase N.
    #[cfg(feature = "test-hooks")]
    #[arg(long)]
    pub stop_after_pass: Option<u8>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// SQLite database to read. Defaults to `<CWD>/.dedupe.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Hide groups whose logical waste is below this many bytes.
    #[arg(long, default_value_t = 0)]
    pub min_wasted: i64,
}

#[derive(Args, Debug)]
pub struct LookupArgs {
    /// SQLite database to query. Defaults to `<CWD>/.dedupe.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    pub file: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Persist on-demand hashes and, if applicable, the query row.
    #[arg(long)]
    pub insert: bool,
}

#[derive(Args, Debug)]
pub struct DbArgs {
    /// SQLite database to maintain. Defaults to `<CWD>/.dedupe.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    #[arg(value_enum)]
    pub action: DbAction,

    /// Required to actually drop the database.
    #[arg(long)]
    pub yes: bool,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum DbAction {
    Info,
    Vacuum,
    Drop,
}

/// Default database filename, resolved relative to an anchor directory.
pub const DEFAULT_DB_NAME: &str = ".dedupe.db";

/// `scan`'s default DB: `<first root>/.dedupe.db` (root canonicalized).
pub fn scan_default_db(roots: &[PathBuf]) -> anyhow::Result<PathBuf> {
    let first = roots.first().context("scan requires at least one root")?;
    let canon = std::fs::canonicalize(first)
        .with_context(|| format!("canonicalize first root {}", first.display()))?;
    Ok(canon.join(DEFAULT_DB_NAME))
}

/// `report` / `lookup` / `db` default DB: `<CWD>/.dedupe.db`.
pub fn cwd_default_db() -> anyhow::Result<PathBuf> {
    Ok(std::env::current_dir()
        .context("resolve current directory")?
        .join(DEFAULT_DB_NAME))
}

/// Resolve `--db` (or the CWD default) and require the file to exist.
pub fn resolve_existing_db(db: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let path = match db {
        Some(p) => p,
        None => cwd_default_db()?,
    };
    if !path.exists() {
        bail!(
            "no database at {}; run `dedupe scan <root>` first or pass --db <PATH>",
            path.display()
        );
    }
    Ok(path)
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.log_level);

    // Ctrl-C sets the shared cancel token; long scans return `Cancelled`.
    let token = Token::new();
    {
        let token = token.clone();
        let _ = ctrlc::set_handler(move || token.cancel());
    }

    match cli.cmd {
        Cmd::Scan(args) => scan::run(args, &token),
        Cmd::Report(args) => report::run(args),
        Cmd::Lookup(args) => lookup::run(args),
        Cmd::Db(args) => db::run(args),
    }
}

fn init_tracing(level: LogLevel) {
    let filter = match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
