//! `dedupe` command-line entry point. All logic lives in `core`; this is a thin
//! consumer that parses flags, builds a `Config`, and prints results.

mod db;
mod lookup;
mod progress;
mod report;
mod scan;
mod size;

use std::path::PathBuf;

use anyhow::{bail, Context};
use clap::{Args, Parser, Subcommand, ValueEnum};
use dedupe_core::cancel::Token;
use dedupe_core::config::VerifyMode;
use dedupe_core::storage_profile::ProfileChoice;

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

    /// Windows only: skip the NTFS $MFT fast path and use the standard directory walker. Ignored on other systems.
    #[arg(long)]
    pub no_mft: bool,

    /// Budget for the in-memory maps (bytes, or e.g. 512MiB, 1GiB).
    #[arg(long, value_parser = size::parse_size)]
    pub mem_budget: Option<u64>,

    /// Compare duplicate candidates byte for byte after hashing, as a check against
    /// hash collisions. Costs extra reads. Ignored with --no-db.
    #[arg(long, value_enum, default_value_t = VerifyArg::Off)]
    pub verify: VerifyArg,

    /// Storage policy. `auto` probes the first root: rotational media get the HDD
    /// policy and solid-state media the SSD policy. If the type cannot be detected
    /// (Windows, network volumes, some USB bridges) it reads 2 files at a time and
    /// prints a note. `hdd` or `ssd` overrides the probe. Ignored with --no-db.
    #[arg(long, value_enum, default_value_t = ProfileArg::Auto)]
    pub profile: ProfileArg,

    /// Number of full-file hashes to run in parallel. Default: CPU count, capped at 8
    /// on SSD; 1 on HDD; 2 when the storage type is unknown. Ignored with --no-db.
    #[arg(long, value_name = "N", value_parser = parse_queue_depth)]
    pub queue_depth: Option<usize>,

    /// Read buffer for hashing, such as 64KiB or 1MiB. Must be at least 1 byte and at
    /// most 64MiB. Default: 64KiB on SSD, 1MiB on HDD. Ignored with --no-db.
    #[arg(long, value_name = "SIZE", value_parser = size::parse_buf_size)]
    pub buf_size: Option<u64>,

    /// Test hook: stop right after committing phase N.
    #[cfg(feature = "test-hooks")]
    #[arg(long)]
    pub stop_after_pass: Option<u8>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Draw progress bars on stderr: auto (TTY only), always, or never.
    #[arg(long, value_enum, default_value_t = progress::ProgressMode::Auto)]
    pub progress: progress::ProgressMode,
}

/// `--verify` values. Kept out of core so core does not depend on clap.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum VerifyArg {
    Off,
    Byte,
}

/// `--profile` values. Kept out of core so core does not depend on clap.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProfileArg {
    Auto,
    Hdd,
    Ssd,
}

impl From<VerifyArg> for VerifyMode {
    fn from(arg: VerifyArg) -> Self {
        match arg {
            VerifyArg::Off => VerifyMode::Off,
            VerifyArg::Byte => VerifyMode::Byte,
        }
    }
}

impl From<ProfileArg> for ProfileChoice {
    fn from(arg: ProfileArg) -> Self {
        match arg {
            ProfileArg::Auto => ProfileChoice::Auto,
            ProfileArg::Hdd => ProfileChoice::Hdd,
            ProfileArg::Ssd => ProfileChoice::Ssd,
        }
    }
}

/// clap value_parser for `--queue-depth`. Zero is rejected because depth 1 already
/// selects the sequential path, so there is no valid meaning for 0.
fn parse_queue_depth(s: &str) -> Result<usize, String> {
    let depth: usize = s
        .parse()
        .map_err(|err| format!("invalid queue depth '{s}': {err}"))?;
    if depth == 0 {
        return Err("queue depth must be at least 1".to_string());
    }
    Ok(depth)
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    /// SQLite database to read. Defaults to `<CWD>/.dedupe.db`.
    #[arg(long)]
    pub db: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub format: OutputFormat,

    /// Hide groups whose logical waste is below this size (bytes, or e.g. 10MB, 1.5GiB).
    #[arg(long, default_value_t = 0, value_parser = size::parse_size_i64)]
    pub min_wasted: i64,

    /// Draw progress bars on stderr: auto (TTY only), always, or never.
    #[arg(long, value_enum, default_value_t = progress::ProgressMode::Auto)]
    pub progress: progress::ProgressMode,
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
        Cmd::Report(args) => report::run(args, &token),
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
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .try_init();
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{parse_queue_depth, Cli, Cmd};

    #[test]
    fn parse_queue_depth_should_reject_zero() {
        let result = parse_queue_depth("0");
        assert!(
            result.is_err(),
            "queue depth '0' should be rejected, got {result:?}"
        );
    }

    #[test]
    fn scan_should_accept_no_mft_flag() {
        let cli = Cli::try_parse_from(["dedupe", "scan", "--no-mft", "/data"])
            .expect("`scan --no-mft <root>` should parse on every platform");
        let Cmd::Scan(args) = cli.cmd else {
            panic!("`scan` should parse to the scan subcommand");
        };
        assert!(args.no_mft, "--no-mft should set no_mft, got {args:?}");
    }

    #[test]
    fn scan_should_default_no_mft_to_false_when_flag_is_absent() {
        let cli = Cli::try_parse_from(["dedupe", "scan", "/data"])
            .expect("`scan <root>` should parse on every platform");
        let Cmd::Scan(args) = cli.cmd else {
            panic!("`scan` should parse to the scan subcommand");
        };
        assert!(!args.no_mft, "no_mft should default to false, got {args:?}");
    }
}
