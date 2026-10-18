# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

deDupe is a duplicate-file finder in Rust. This directory is the Cargo workspace root; the CLI binary is `dedupe` (package `dedupe`, in `crates/cli`). It groups files by content in progressive passes (size, then BLAKE3 over the first 16 KiB plus last 4 KiB, then full BLAKE3) and persists state in SQLite, so a scan can resume after `kill -9` and new files can be checked against an existing index (`lookup`). v1 is scan/report/lookup only. Nothing deletes files.

The engineering plan is `../.overall_plan.md` (the git repo root is one level up). Its "Status index" and "Task ledger" are the source of truth for what is done and pending. Do not restate status in this file.

## Commands

Run from this directory. The toolchain is pinned to Rust 1.92.0 in `rust-toolchain.toml`.

```bash
# Build
cargo build --workspace --all-targets

# Lint and format (CI runs these with RUSTFLAGS=-D warnings)
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# Windows cross-check from macOS/Linux (compile and lint only; see "Windows cross-check")
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo check --workspace --all-targets --locked --target x86_64-pc-windows-gnu
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo clippy --workspace --all-targets --all-features --locked --target x86_64-pc-windows-gnu -- -D warnings

# Dependency policy (CI job; needs cargo-deny installed locally)
cargo deny check

# Run the CLI
cargo run -p dedupe -- scan <root>            # persists to <root>/.dedupe.db
cargo run -p dedupe -- scan --no-db <root>    # naive pipeline, no DB
cargo run -p dedupe -- report                 # reads <cwd>/.dedupe.db
cargo run -p dedupe -- lookup <file>
cargo run -p dedupe -- db info

# Tests (run only when the user asks; see Working conventions)
cargo test --workspace --all-targets
cargo test -p dedupe-core --test phase1                 # one integration-test file
cargo test -p dedupe-core --test phase1 <name_filter>   # one test within it
cargo test -p dedupe --test kill_fuzz -- --ignored      # kill -9 fuzz (nightly CI)
# Windows self-check: elevated PowerShell on an NTFS volume only (cannot run on macOS/Linux)
cargo test -p dedupe-backend-windows --test mft_vs_walk -- --ignored
DEDUPE_SCALE_N=1000000 cargo test -p dedupe-core --test scale -- --ignored   # 1M-file scale tests

# Benchmarks (user-run on reference hardware; deferred to the end of the plan)
cargo run -p dedupe-bench --release -- [rows]
```

`--stop-after-pass` exists only when built with `--features test-hooks` on the `dedupe` package.

### Windows cross-check

Compile and lint the Windows targets from a macOS or Linux host. This proves the code type-checks and passes clippy for `x86_64-pc-windows-gnu`. It runs nothing.

One-time setup:

```bash
rustup target add x86_64-pc-windows-gnu --toolchain 1.92.0
brew install mingw-w64   # provides x86_64-w64-mingw32-gcc and x86_64-w64-mingw32-ar
```

Run the two Commands lines marked "Windows cross-check" after any edit that touches Windows-reachable code: `backend-generic`, `backend-windows`, or `cli`.

## Architecture

### Crate layering

- `crates/core` (`dedupe-core`) holds all logic. It has `#![forbid(unsafe_code)]` and no `cfg(target_os)`. It reaches the OS only through the traits in `core/src/traits.rs` (`Enumerator`, `ContentReader`, `PathCodec`, `VolumeIdentity`, `Hasher`), which are bundled per call in `pipeline::Services`.
- `crates/backend-generic` is the portable backend: walkdir enumeration, positional reads, volume identity, and the path codec. Its Windows storage probe and extent lookup sit behind `cfg(windows)`.
- `crates/backend-windows` holds the NTFS `$MFT` enumerator (`AutoEnumerator`), open-by-id reads (`HandleReader`), and overlapped read-ahead. Its parsing, path, and scheduling modules are portable and unit-tested on every OS. Win32 calls are confined to `cfg(windows)` items in `caps`, `volume_io`, `handle`, `overlapped`, and `enumerator`.
- Any `unsafe` or platform `cfg` belongs in one of the two backend crates.
- `crates/cli` is a thin consumer: argument parsing, progress bar, output formatting, and the storage probe that resolves `Config.profile`. Scan logic does not belong here. `make_backend` in `cli/src/scan.rs` is the only place that picks backend types.
- `crates/tui` is a stub. `bench/` is a standalone benchmark binary. `xtask/` is a stub.
- Linux and macOS use `backend-generic` directly. A new platform gets its own `backend-*` crate implementing the same traits.

### Two scan pipelines

- `pipeline::naive::scan_naive` runs in memory with no DB. It backs `scan --no-db` and serves as the correctness reference.
- `pipeline::scan::run_scan` is the default persistent path. It:
  1. Opens the DB read-write (pragmas, then migrations) and checks the hash-config guard (`resume::check_config_guard`).
  2. Either inserts a new `scan_runs` row (abandoning overlapping running runs) or, with `--resume`, builds a plan with `resume::plan_resume`.
  3. Spawns the single writer thread and opens a separate read-only connection for the passes.
  4. For each volume, runs `pass_enumerate`, `pass_size`, `pass_prefix`, `pass_full`, then `pass_verify` (only with `VerifyMode::Byte`, set by `scan --verify byte`). Each pass advances `run_volumes.phase_reached`.
  5. After the writer shuts down, recomputes `dup_groups` per volume and marks the run finished.

### Write path

- One thread owns the only read-write `rusqlite::Connection` (`db/writer.rs`). Passes and hash workers send `Batch` messages over a bounded channel. The writer commits every 50k rows or 5 seconds.
- `db/ids.rs` (`IdAllocator`) pre-assigns file and folder ids, so passes can reference rows before the writer commits them.
- Stale-row deletion also goes through the writer (`Batch::DeleteStale`).

### File state machine

`files.state` is an integer. Each pass consumes one state and writes the next: 0 enumerated, 1 size candidate, 2 prefix hashed, 3 full hashed, 4 verified, 5 done. Negative values are terminal: -1 skipped, -2 error, -3 unique by size, -4 unique by prefix. Transition rules are in plan P1.4. Two rules matter most: transitions only move forward within a run, and a rescan resets rows to 0 but reuses stored hashes when size, mtime, ctime, and file id are unchanged.

### Resume and rescan

- Resume is driven by `run_volumes.phase_reached` (the `Phase` enum in `core/src/model.rs`), not by a full re-scan. Enumeration is an idempotent upsert, so a crash loses at most one batch.
- A rescan (a new run over an already-indexed volume) keeps hashes for unchanged files and detects moves by `(volume_id, file_id)` plus size and mtime. It deletes stale rows only under this run's roots. Rows under roots not scanned this time are left alone.
- `meta.hash_algo` and the prefix sizes must match the DB. A mismatch aborts resume unless `--force` is passed.

### Reading results

- `report` and `lookup` read the DB, not in-memory state. Report counts only states 3, 4, and 5 for the latest completed run. Lookup considers every live row not in states -1 or -2.
- Default DB location differs by command: `scan` uses `<first root>/.dedupe.db`, while `report`, `lookup`, and `db` use `<cwd>/.dedupe.db`.

### Progress and cancellation

- `core/src/progress.rs` defines the `Sink` trait and `SharedSink`. The CLI supplies a bar sink (`cli/src/progress.rs`) or `null_sink()` for JSON output and non-TTY stderr. Core never prints.
- `core/src/cancel.rs` holds the cancel token. The CLI cancels it from a Ctrl-C handler. On Windows the same token is handed to `AutoEnumerator`, so Ctrl-C also stops the `$MFT` read phase (a cancel returns `Cancelled` and never triggers the fallback walker).

## Gotchas

- Do not add `unsafe` or `cfg(target_os)` to `core`. Put platform code in a backend crate behind a trait.
- Several source comments point to `.plan.md` (7 references in 5 files). That file was renamed to `../.overall_plan.md`, and the section numbers in those comments may not match the current plan.
- `file_id` is `None` in the Windows walker fallback, so there is no hardlink collapse or move detection in that mode. The NTFS `$MFT` path yields the 8-byte file reference and `nlink`, so both work there. Rescans reuse hashes only when file ids match, so switching modes re-hashes files.
- The NTFS fast path runs only when the process is elevated and the scan root is on NTFS (`AutoEnumerator`). Otherwise, or when the fast path fails partway, it falls back to the walker and the CLI prints a `note:` on stderr. `--no-mft` forces the walker and prints no note. Items the fast path already yielded are yielded again by the walker, which is safe because enumeration is an idempotent upsert.
- Win32 code is compile-checked and linted from macOS only. No test executes it. Before trusting it, run the `mft_vs_walk` self-check on a Windows machine. Still unverified there: the raw `$MFT` read, how the `ntfs` crate handles unused or corrupt records, the reparse tags that mark cloud placeholders, and the admin requirement.
- `scan --verify <off|byte>` sets `Config.verify` (plan P2.7). `pass_verify` moves state 3 -> 4 and a mismatch to -2. Byte verify has no automated end-to-end test yet.
- `Config.profile` is resolved in the CLI (`cli/src/scan.rs`), not in core: the CLI probes the first root with `GenericStorageProbe` (Linux sysfs, macOS `diskutil`, Windows seek-penalty ioctl, not yet verified on hardware; network volumes report unknown), then calls `core::storage_profile::resolve`. `StorageKind::Unknown` keeps the SSD buffer and ordering but caps the full-hash queue depth at `UNKNOWN_QUEUE_DEPTH` (2, a judgment value, not measured). The CLI prints a stderr note for Unknown unless `--queue-depth` is set, and only on the DB path. `--no-db` (naive) does not read the profile.
- `scale.rs` and `kill_fuzz.rs` tests are `#[ignore]`. Plain `cargo test` skips them.
- The hash is BLAKE3 truncated to 128 bits (`blake3-128`).

## Working conventions
ALWAYS use 'rust-best-practises' skill while working on rust code and systems.

- Lint and format checks can run without asking. Builds and tests run only when the user asks for them. When they do run, use a subagent.
- Benchmarks are deferred to the end of the plan and run by the user on reference hardware. Do not run them as verification.
- Plan status and task IDs live in `../.overall_plan.md`. Read that file before starting work on any task ID.
