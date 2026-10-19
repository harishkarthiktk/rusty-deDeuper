//! `dedupe` command-line entry point. All logic lives in `core`; this is a thin
//! consumer that parses flags, builds a `Config`, and prints results.

mod db;
mod discover;
mod lookup;
mod progress;
mod report;
mod scan;
mod size;

use std::path::{Path, PathBuf};

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

/// `--format` values. `Auto` prints human text on a terminal and JSON when stdout is piped.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Auto,
    Human,
    Json,
}

/// The concrete format a command prints, after `OutputFormat::resolve`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    Human,
    Json,
}

impl OutputFormat {
    /// `--json` wins over `--format`. `Auto` picks human output only when stdout is a terminal.
    pub fn resolve(self, json_flag: bool, stdout_is_tty: bool) -> Resolved {
        if json_flag {
            return Resolved::Json;
        }
        match self {
            OutputFormat::Human => Resolved::Human,
            OutputFormat::Json => Resolved::Json,
            OutputFormat::Auto if stdout_is_tty => Resolved::Human,
            OutputFormat::Auto => Resolved::Json,
        }
    }
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

    /// Run without the database lock when the filesystem does not support locking.
    #[arg(long)]
    pub no_lock: bool,

    /// Do not descend across mount points (default).
    #[arg(long, conflicts_with = "cross_mounts")]
    pub one_file_system: bool,

    /// Descend across mount points.
    #[arg(long)]
    pub cross_mounts: bool,

    /// Include cloud placeholders instead of skipping them.
    #[arg(long)]
    pub include_placeholders: bool,

    /// Skip paths matching this glob, and everything under a matching directory. Repeatable.
    /// Matched against the full path with `/` separators, case-sensitive, for example
    /// `--exclude '**/target' --exclude '**/node_modules'`. Rows indexed earlier under an
    /// excluded path are removed by the next scan of that root.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Windows only: skip the NTFS $MFT fast path and use the standard directory walker. Ignored on other systems.
    #[arg(long)]
    pub no_mft: bool,

    /// Windows only: do not read or write the MFT cache for this scan. The cache lists every
    /// filename on the volume and lives in %LOCALAPPDATA%\dedupe\mft (or DEDUPE_CACHE_DIR).
    /// Applies only to the NTFS fast path. Ignored on other systems.
    #[arg(long, conflicts_with = "rebuild_mft_cache")]
    pub no_mft_cache: bool,

    /// Windows only: ignore any existing MFT cache, read the whole volume, and write a fresh
    /// cache. The cache lists every filename on the volume and lives in %LOCALAPPDATA%\dedupe\mft
    /// (or DEDUPE_CACHE_DIR). Applies only to the NTFS fast path. Ignored on other systems.
    #[arg(long)]
    pub rebuild_mft_cache: bool,

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

    /// Output format. `auto` prints human text on a terminal and JSON when stdout is piped.
    #[arg(long, short = 'f', value_enum, default_value_t = OutputFormat::Auto)]
    pub format: OutputFormat,

    /// Shorthand for `--format json`.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

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
    /// SQLite database to read. Only needed when the folder holds several databases.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Folder holding the database; a file path is used as the database itself.
    /// Defaults to the current directory.
    #[arg(conflicts_with = "db")]
    pub dir: Option<PathBuf>,

    /// Output format. `auto` prints human text on a terminal and JSON when stdout is piped.
    #[arg(long, short = 'f', value_enum, default_value_t = OutputFormat::Auto)]
    pub format: OutputFormat,

    /// Shorthand for `--format json`.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

    /// Hide groups whose logical waste is below this size (bytes, or e.g. 10MB, 1.5GiB).
    #[arg(long, default_value_t = 0, value_parser = size::parse_size_i64)]
    pub min_wasted: i64,

    /// Write the report to this file as UTF-8 instead of stdout. With `--format auto` the
    /// file gets JSON. Avoids the UTF-16 files that PowerShell's `>` produces.
    #[arg(long, short = 'o')]
    pub output: Option<PathBuf>,

    /// Draw progress bars on stderr: auto (TTY only), always, or never.
    #[arg(long, value_enum, default_value_t = progress::ProgressMode::Auto)]
    pub progress: progress::ProgressMode,
}

#[derive(Args, Debug)]
pub struct LookupArgs {
    /// SQLite database to query. Only needed when the folder holds several databases.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Folder holding the database; a file path is used as the database itself.
    /// Defaults to the current directory.
    #[arg(long, conflicts_with = "db")]
    pub dir: Option<PathBuf>,

    pub file: PathBuf,

    /// Output format. `auto` prints human text on a terminal and JSON when stdout is piped.
    #[arg(long, short = 'f', value_enum, default_value_t = OutputFormat::Auto)]
    pub format: OutputFormat,

    /// Shorthand for `--format json`.
    #[arg(long, conflicts_with = "format")]
    pub json: bool,

    /// Persist on-demand hashes and, if applicable, the query row.
    #[arg(long)]
    pub insert: bool,

    /// Run without the database lock when the filesystem does not support locking (with --insert).
    #[arg(long)]
    pub no_lock: bool,
}

#[derive(Args, Debug)]
pub struct DbArgs {
    /// SQLite database to maintain. Only needed when the folder holds several databases.
    #[arg(long)]
    pub db: Option<PathBuf>,

    /// Folder holding the database; a file path is used as the database itself.
    /// Defaults to the current directory.
    #[arg(long, conflicts_with = "db")]
    pub dir: Option<PathBuf>,

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

/// True when `p` is a Windows drive-relative path such as `c:` or `c:foo`. Those resolve
/// against the current folder on that drive, not the drive root. `c:\` and `c:/` are not.
/// Pure string check, so it behaves the same on every OS.
pub fn is_drive_relative(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let b = s.as_bytes();
    b.len() >= 2
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && (b.len() == 2 || !matches!(b[2], b'\\' | b'/'))
}

/// Folder for `report` / `lookup` / `db`: `--dir` or the current directory.
fn folder_or_cwd(dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    match dir {
        Some(dir) if is_drive_relative(&dir) => {
            let arg = dir.to_string_lossy().into_owned();
            let letter = &arg[..1];
            bail!(
                "'{arg}' is relative to the current folder on that drive; use '{letter}:\\' for the drive root or give a full path"
            )
        }
        Some(dir) => Ok(dir),
        None => std::env::current_dir().context("resolve current directory"),
    }
}

/// Resolve the database for `report` / `lookup` / `db`. `--db` wins and must exist.
/// Otherwise `dir` (default: current directory) is either the database file itself or a
/// folder that holds exactly one dedupe database.
pub fn resolve_existing_db(db: Option<PathBuf>, dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let path = if let Some(path) = db {
        if !path.exists() {
            bail!(
                "no database at {}; run `dedupe scan <root>` first or pass --db <PATH>",
                path.display()
            );
        }
        path
    } else {
        let dir = folder_or_cwd(dir)?;
        if dir.is_file() {
            dir
        } else {
            discover::discover_db(&dir)?
        }
    };
    eprintln!("using database: {}", full_path(&path).display());
    Ok(path)
}

/// Absolute form of `path` for display only. Falls back to `path` if it cannot be made absolute.
fn full_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Database target for `lookup --insert`. `--db` wins. Otherwise use the one dedupe
/// database in the folder, or a new `.dedupe.db` there when the folder has none.
pub fn insert_db_target(db: Option<PathBuf>, dir: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = db {
        return Ok(path);
    }
    let dir = folder_or_cwd(dir)?;
    if dir.is_file() {
        return Ok(dir);
    }
    match discover::discover_db(&dir) {
        Ok(path) => {
            eprintln!("using database: {}", full_path(&path).display());
            Ok(path)
        }
        Err(discover::DiscoverError::NotFound(_)) => Ok(dir.join(DEFAULT_DB_NAME)),
        Err(err) => Err(err.into()),
    }
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

    use super::{parse_queue_depth, Cli, Cmd, OutputFormat};

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

    #[test]
    fn scan_should_accept_no_mft_cache_flag() {
        let cli = Cli::try_parse_from(["dedupe", "scan", "--no-mft-cache", "/data"])
            .expect("`scan --no-mft-cache <root>` should parse on every platform");
        let Cmd::Scan(args) = cli.cmd else {
            panic!("`scan` should parse to the scan subcommand");
        };
        assert!(
            args.no_mft_cache,
            "--no-mft-cache should set no_mft_cache, got {args:?}"
        );
    }

    #[test]
    fn scan_should_accept_rebuild_mft_cache_flag() {
        let cli = Cli::try_parse_from(["dedupe", "scan", "--rebuild-mft-cache", "/data"])
            .expect("`scan --rebuild-mft-cache <root>` should parse on every platform");
        let Cmd::Scan(args) = cli.cmd else {
            panic!("`scan` should parse to the scan subcommand");
        };
        assert!(
            args.rebuild_mft_cache,
            "--rebuild-mft-cache should set rebuild_mft_cache, got {args:?}"
        );
    }

    #[test]
    fn scan_should_reject_when_both_mft_cache_flags_given() {
        let result = Cli::try_parse_from([
            "dedupe",
            "scan",
            "--no-mft-cache",
            "--rebuild-mft-cache",
            "/data",
        ]);
        assert!(
            result.is_err(),
            "--no-mft-cache and --rebuild-mft-cache should conflict"
        );
    }

    #[test]
    fn scan_should_default_both_cache_flags_to_false() {
        let cli = Cli::try_parse_from(["dedupe", "scan", "/data"])
            .expect("`scan <root>` should parse on every platform");
        let Cmd::Scan(args) = cli.cmd else {
            panic!("`scan` should parse to the scan subcommand");
        };
        assert!(
            !args.no_mft_cache,
            "no_mft_cache should default to false, got {args:?}"
        );
        assert!(
            !args.rebuild_mft_cache,
            "rebuild_mft_cache should default to false, got {args:?}"
        );
    }

    #[test]
    fn scan_should_accept_short_format_flag() {
        let cli = Cli::try_parse_from(["dedupe", "scan", "-f", "json", "/data"])
            .expect("`scan -f json <root>` should parse on every platform");
        let Cmd::Scan(args) = cli.cmd else {
            panic!("`scan` should parse to the scan subcommand");
        };
        assert_eq!(
            args.format,
            OutputFormat::Json,
            "-f json should set format, got {args:?}"
        );
    }

    #[test]
    fn scan_should_reject_json_flag_with_format_flag() {
        let result =
            Cli::try_parse_from(["dedupe", "scan", "--json", "--format", "human", "/data"]);
        assert!(result.is_err(), "--json and --format should conflict");
    }

    #[test]
    fn report_should_accept_json_flag() {
        let cli = Cli::try_parse_from(["dedupe", "report", "--json"])
            .expect("`report --json` should parse on every platform");
        let Cmd::Report(args) = cli.cmd else {
            panic!("`report` should parse to the report subcommand");
        };
        assert!(args.json, "--json should set json, got {args:?}");
    }

    #[test]
    fn report_should_accept_positional_dir() {
        let cli = Cli::try_parse_from(["dedupe", "report", "/data"])
            .expect("`report <dir>` should parse on every platform");
        let Cmd::Report(args) = cli.cmd else {
            panic!("`report` should parse to the report subcommand");
        };
        assert_eq!(
            args.dir.as_deref(),
            Some(std::path::Path::new("/data")),
            "positional dir should be captured, got {args:?}"
        );
        assert!(args.db.is_none(), "db should stay unset, got {args:?}");
    }

    #[test]
    fn report_should_reject_dir_with_db_flag() {
        let result = Cli::try_parse_from(["dedupe", "report", "/data", "--db", "x.db"]);
        assert!(result.is_err(), "positional dir and --db should conflict");
    }

    #[test]
    fn lookup_should_accept_dir_flag() {
        let cli = Cli::try_parse_from(["dedupe", "lookup", "--dir", "/data", "file.bin"])
            .expect("`lookup --dir <dir> <file>` should parse on every platform");
        let Cmd::Lookup(args) = cli.cmd else {
            panic!("`lookup` should parse to the lookup subcommand");
        };
        assert_eq!(
            args.dir.as_deref(),
            Some(std::path::Path::new("/data")),
            "--dir should be captured, got {args:?}"
        );
        assert_eq!(args.file, std::path::PathBuf::from("file.bin"));
    }

    #[test]
    fn is_drive_relative_should_flag_bare_drive_and_drive_prefixed_names() {
        use std::path::Path;

        use super::is_drive_relative;

        for p in ["c:", "C:", "c:foo", "D:foo\\bar"] {
            assert!(
                is_drive_relative(Path::new(p)),
                "{p:?} should be drive-relative"
            );
        }
    }

    #[test]
    fn is_drive_relative_should_not_flag_drive_root_or_plain_paths() {
        use std::path::Path;

        use super::is_drive_relative;

        for p in ["c:\\", "c:/", "foo", "/tmp/x", "C:\\Users"] {
            assert!(
                !is_drive_relative(Path::new(p)),
                "{p:?} should not be drive-relative"
            );
        }
    }
}
