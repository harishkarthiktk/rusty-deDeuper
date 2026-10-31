# crates/cli (`dedupe`)

The `dedupe` binary, the only user-facing entry point. It parses flags, builds a `Config`, calls the
`core` pipelines and prints results. It depends on `dedupe-core` and `dedupe-backend-generic`, and on
`dedupe-backend-windows` on Windows only (through `platform/`). Nothing depends on this crate. Scan
logic does not belong here. See the root CLAUDE.md for layering and cross-crate rules.

## Module map
- `src/main.rs`: clap `Cli` (global `--log-level`), `Cmd` (`scan`, `report`, `lookup`, `db`) and their
  `*Args` structs, `OutputFormat` / `Resolved`, the Ctrl-C handler, and the DB path helpers
  (`scan_default_db`, `resolve_existing_db`, `insert_db_target`, `is_drive_relative`, `DEFAULT_DB_NAME`).
- `src/scan.rs`: `dedupe scan`. Chooses the DB or `--no-db` path, runs the locality check and the storage
  probe, builds the exclude list and `Config`, runs the scan, and writes `scan_runs.enumerator`. Holds
  `DEV_ARTIFACT_GLOBS` and `unknown_storage_notice`.
- `src/platform/mod.rs`: the only CLI file with OS `cfg`. Defines the `ScanExtras` trait, the `Backend`
  struct and `read_ahead_for`, and re-exports the cfg-selected `make_backend` and `Extras`.
- `src/platform/generic.rs`: `make_backend` for every non-Windows build. Label is always `walker`.
- `src/platform/windows.rs`: `make_backend` for Windows: `AutoEnumerator` (NTFS `$MFT` path with walker
  fallback), `HandleReader`, MFT cache config, the `$MFT` progress bar, and the enumerator label mapping.
- `src/progress.rs`: `ProgressMode`, `should_draw`, `BarSink` (implements `core::progress::Sink`) and
  `ReportBar`. Draws to stderr only.
- `src/report.rs`: `dedupe report`. Streams human output or writes JSON, with optional `--output`.
- `src/lookup.rs`: `dedupe lookup`. Read-only unless `--insert`, which takes the DB lock.
- `src/db.rs`: `dedupe db info | vacuum | drop`.
- `src/discover.rs`: `discover_db`, which finds the one dedupe DB directly inside a folder (not recursive).
- `src/size.rs`: parsers for byte-size flags (`parse_size`, `parse_size_i64`, `parse_buf_size`).

## How it works
1. `main` parses args and sends tracing to stderr. Only `scan` and `report` get a Ctrl-C handler
   (`install_ctrlc`): the first press cancels the `Token` and prints a notice; the second press
   exits with code 130. A failed handler install is ignored.
2. `scan` and `report` receive the token. A clean cancel (`CoreError::Cancelled` in the error
   chain, see `is_cancel`) prints the error and exits 130. `lookup` and `db` take no token and
   have no handler, so Ctrl-C ends them at once (`lookup --insert` is one transaction).
3. `scan::run`:
   1. Format: `--json` wins; `auto` prints human text only when stdout is a terminal.
   2. DB target: `--no-db` gives none, `--db` gives that path, otherwise `scan_default_db` gives
      `<first root, canonicalized>/.dedupe.db`.
   3. `check_db_locality` (see Gotchas).
   4. Storage probe on the first root, then `storage_profile::resolve`.
   5. Exclude list: `--exclude` globs plus `DEV_ARTIFACT_GLOBS` unless `--include-dev-artifacts`, and
      the `-E/--exclude-dir` paths resolved by `resolve_exclude_dirs`.
   6. `platform::make_backend`, then `Config`. With no DB, `scan_naive` runs. With a DB, `run_scan` runs,
      the CLI reopens the DB read-write to write the enumerator label, then reopens it read-only and
      builds `Report::from_db(conn, 0)`.
   7. Enumerator notices are drained to stderr as `note: ...`, after success and after failure.
4. `report`, `lookup` and `db` call `resolve_existing_db`: `--db` must exist. Otherwise `--dir` (or the
   current directory) is used as the file itself, or as a folder searched with `discover_db`.
5. stdout carries only the report. Progress, notes and errors go to stderr.

## Invariants and rules
- `main.rs` has `#![forbid(unsafe_code)]`, which covers the whole crate.
- OS `cfg` appears only in `src/platform/mod.rs`. `cargo run -p dedupe-xtask -- boundaries` enforces it
  (see `xtask/CLAUDE.md`). `#[cfg(feature = "test-hooks")]` is not an OS cfg and is allowed.
- Scan code takes its backend from `platform::make_backend`; `lookup.rs` takes its reader from
  `platform::make_reader` (profile resolved with `Auto`) and its codec and volume from
  `GenericBackend::new()`. The other exception is `scan.rs` (`GenericStorageProbe`, `fs_locality`).
- Clap enums (`VerifyArg`, `ProfileArg`) convert to core enums with `From`. Core must not depend on clap.
- With `--format json`, stdout is JSON only. `--progress always` still draws nothing in JSON mode.
- Clap `conflicts_with` attributes encode the flag rules (`--json` vs `--format`, `--db` vs `--no-db`,
  `--resume` vs `--no-db`, `--cross-mounts` vs `--one-file-system`). Keep them in the attributes.
- `DEV_ARTIFACT_GLOBS` keys only on artifact folder names and extensions. Never add a folder name that
  may hold media (such as `TurboC++`).
- The probe runs before the profile exists, so it cannot come from `make_backend`. A failed probe is
  dropped on purpose and maps to unknown storage.

## Gotchas
- `scan` and `lookup --insert` hold the database lock (`crates/core/src/db/lock.rs`: a `<db>.lock` sidecar,
  `File::try_lock`, so `rust-version` is 1.89). `report`, `lookup` without `--insert`, and `db` take no lock.
  `db vacuum` and `db drop` also take no lock, so do not run them during a scan. If the filesystem reports
  file locking as unsupported (likely on WSL `/mnt/c`), the lock errors unless `--no-lock` is passed
  (`scan` and `lookup --insert`). `db drop` removes `<db>`, `-wal` and `-shm`, not `<db>.lock`.
- `scan` refuses a default DB on a network filesystem (`backend-generic::fs_locality`, `Remote`) and
  prints a note for WSL 9p (`Virtualized`). With `--db` it prints a note and continues. FUSE counts as
  remote, so a local ntfs-3g mount needs an explicit `--db`.
- `scan_runs.enumerator` is written by the CLI after `run_scan` returns, because the enumerator is only
  known once the walk ends. Labels: `mft`, `walker`, `walker-forced` (`--no-mft`) or `walker-fallback`.
  The schema version is core's concern.
- `scan --exclude <glob>` (repeatable, `crates/core/src/exclude.rs`) matches the full path with `/` separators
  and ignores case. The walker prunes excluded directories; `pass_enumerate` drops them for the `$MFT` path
  too, and `scan_naive` (`--no-db`) drops them on every path. The CLI appends the fixed `DEV_ARTIFACT_GLOBS`
  list by default (`--include-dev-artifacts` turns it off; core `Config` has no default excludes).
- `scan -E/--exclude-dir <PATH>` (repeatable) skips one directory and everything under it. Matching ignores
  case. `resolve_exclude_dirs` (`scan.rs`) canonicalizes each path (relative paths resolve from the cwd) and
  fails when the path is missing or contains a scan root. Core receives absolute paths only.
- The NTFS fast path runs only when the process is elevated and the scan root is on NTFS (`AutoEnumerator`).
  Otherwise, or when the fast path fails partway, it falls back to the walker and the CLI prints a
  `note:` on stderr. `--no-mft` forces the walker and prints no note. Items the fast path already
  yielded are yielded again by the walker, which is safe because enumeration is an idempotent upsert.
- `Config.profile` is resolved here, not in core. The probe (Linux sysfs, macOS `diskutil`, Windows
  seek-penalty ioctl, not yet verified on hardware; network volumes report unknown) runs even with
  `--no-db`. `scan_naive` never reads the profile, so only the Unknown note is skipped there. For
  Unknown, core keeps the SSD buffer and ordering but caps the full-hash queue depth at
  `UNKNOWN_QUEUE_DEPTH` (2, a judgment value, not measured). The CLI prints a stderr note for Unknown
  unless `--queue-depth` is set, and only on the DB path.
- Default DB location differs by command: `scan` uses `<first root>/.dedupe.db`. `report`, `lookup` and
  `db` search `--dir` (default cwd) with `discover_db` for the one dedupe DB, whatever its name. Only
  `lookup --insert` falls back to creating `<dir>/.dedupe.db` when none exists.
- `report` prints a `note:` when a newer scan run did not finish. With a cancel, it prints
  `output incomplete (cancelled)` after the partial header.
- `report --output <file>` with `--format auto` writes JSON, because a file is not a terminal.
- `parse_size` uses decimal units for KB, MB, GB, TB and binary units for KiB, MiB, GiB, TiB. A bare
  number is bytes. `--buf-size` accepts 1 byte to 64 MiB.
- `lookup` prints the `--no-lock` note itself when the lock reports it is not held.
- `db drop` requires `--yes`. Without it the command fails before touching any file.

## Commands
```bash
cargo clippy -p dedupe --all-targets --all-features -- -D warnings   # lint, no approval needed
cargo run -p dedupe --features test-hooks -- scan --stop-after-pass <N> <root>   # test hook, needs the feature
cargo test -p dedupe --test kill_fuzz -- --ignored                  # user-run only, see Tests
```
`--stop-after-pass` exists only with `--features test-hooks` on `dedupe`. It stops right after committing
phase N. It is a test hook, not a supported option.

## Tests
Run only when the user asks. `cargo test -p dedupe` runs everything below except the ignored test.
- In-module unit tests: `main.rs` (queue depth 0 rejected, `--no-mft` and `--no-mft-cache` parsing),
  `scan.rs` (`unknown_storage_notice` text and conditions), `platform/mod.rs` (`read_ahead_for`, which
  compiles and runs on every OS), `progress.rs` (`should_draw` truth table), `size.rs` (size parsing).
- `tests/db_info.rs`: scans the core fixture tree (`crates/core/tests/fixtures/tree`) into a temp DB,
  then checks the labels and counts printed by `db info`.
- `tests/progress_cli.rs`: JSON scan with `--progress always`. Stdout must parse as JSON, and stderr must
  contain no bar text or ANSI escapes.
- `tests/kill_fuzz.rs`: `#[ignore]`, nightly only. 20 iterations: start a scan, kill it after 100 to 299 ms,
  run `--resume`, and compare the JSON report with an uninterrupted scan. Writes about 576 MiB to a temp
  dir. The doc comment says Unix-only, but the code uses only `Child::kill`, so it is portable.
