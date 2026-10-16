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
DEDUPE_SCALE_N=1000000 cargo test -p dedupe-core --test scale -- --ignored   # 1M-file scale tests

# Benchmarks (user-run on reference hardware; deferred to the end of the plan)
cargo run -p dedupe-bench --release -- [rows]
```

`--stop-after-pass` exists only when built with `--features test-hooks` on the `dedupe` package.

## Architecture

### Crate layering

- `crates/core` (`dedupe-core`) holds all logic. It has `#![forbid(unsafe_code)]` and no `cfg(target_os)`. It reaches the OS only through the traits in `core/src/traits.rs` (`Enumerator`, `ContentReader`, `PathCodec`, `VolumeIdentity`, `Hasher`), which are bundled per call in `pipeline::Services`.
- `crates/backend-generic` is the only backend today: walkdir enumeration, positional reads, volume identity, and the path codec. Any `unsafe` or platform `cfg` belongs here.
- `crates/cli` is a thin consumer: argument parsing, progress bar, output formatting. Scan logic does not belong here.
- `crates/tui` is a stub. `bench/` is a standalone benchmark binary. `xtask/` is a stub.
- The planned Windows, Linux, and macOS backends are not created yet. Each will be a new `backend-*` crate implementing the same traits.

### Two scan pipelines

- `pipeline::naive::scan_naive` runs in memory with no DB. It backs `scan --no-db` and serves as the correctness reference.
- `pipeline::scan::run_scan` is the default persistent path. It:
  1. Opens the DB read-write (pragmas, then migrations) and checks the hash-config guard (`resume::check_config_guard`).
  2. Either inserts a new `scan_runs` row (abandoning overlapping running runs) or, with `--resume`, builds a plan with `resume::plan_resume`.
  3. Spawns the single writer thread and opens a separate read-only connection for the passes.
  4. For each volume, runs `pass_enumerate`, `pass_size`, `pass_prefix`, `pass_full`, then `pass_verify` (only with `VerifyMode::Byte`, which is currently inert). Each pass advances `run_volumes.phase_reached`.
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
- `core/src/cancel.rs` holds the cancel token. The CLI cancels it from a Ctrl-C handler.

## Gotchas

- Do not add `unsafe` or `cfg(target_os)` to `core`. Put platform code in a backend crate behind a trait.
- Several source comments point to `.plan.md` (7 references in 5 files). That file was renamed to `../.overall_plan.md`, and the section numbers in those comments may not match the current plan.
- `file_id` is `None` on Windows until the Windows backend lands, so there is no hardlink collapse or move detection there.
- `--verify` is not wired to the CLI yet (plan P2.7). `pass_verify` is implemented (core only, enabled via `Config.verify = Byte`; untested): state 3 -> 4, mismatch -> -2.
- `scale.rs` and `kill_fuzz.rs` tests are `#[ignore]`. Plain `cargo test` skips them.
- The hash is BLAKE3 truncated to 128 bits (`blake3-128`).

## Working conventions

- Lint and format checks can run without asking. Builds and tests run only when the user asks for them. When they do run, use a subagent.
- Benchmarks are deferred to the end of the plan and run by the user on reference hardware. Do not run them as verification.
- Plan status and task IDs live in `../.overall_plan.md`. Read that file before starting work on any task ID.
