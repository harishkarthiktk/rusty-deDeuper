//! `dupfind` command-line entry point. All logic lives in `core`; this is a thin
//! consumer that parses flags, builds a `Config`, and prints results.

mod db;
mod lookup;
mod report;
mod scan;

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use dupfind_core::cancel::Token;

#[derive(Parser, Debug)]
#[command(name = "dupfind", version, about = "Duplicate file finder")]
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
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Resume the latest interrupted run.
    #[arg(long)]
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
    #[arg(long)]
    pub db: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Hide groups whose logical waste is below this many bytes.
    #[arg(long, default_value_t = 0)]
    pub min_wasted: i64,
}

#[derive(Args, Debug)]
pub struct LookupArgs {
    #[arg(long)]
    pub db: PathBuf,

    pub file: PathBuf,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Persist on-demand hashes and, if applicable, the query row.
    #[arg(long)]
    pub insert: bool,
}

#[derive(Args, Debug)]
pub struct DbArgs {
    #[arg(long)]
    pub db: PathBuf,

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
