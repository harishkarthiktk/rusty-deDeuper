# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

This is the general guide: what the project is, workspace commands, how the crates fit together, and rules that cross crates. Each crate has its own `CLAUDE.md` with its module map, runtime flow, invariants, gotchas, crate-specific commands and tests. Read the crate file before editing that crate.

## What this is

deDupe is a duplicate-file finder in Rust. This directory is the Cargo workspace root; the CLI binary is `dedupe` (package `dedupe`, in `crates/cli`). It groups files by content in progressive passes (size, then BLAKE3 over the first 16 KiB plus last 4 KiB, then full BLAKE3) and persists state in SQLite, so a scan can resume after `kill -9` and new files can be checked against an existing index (`lookup`). v1 is scan/report/lookup only. Nothing deletes files.

The engineering plan is `.overall_plan.md` (in this directory, which is the repo root). Its "Status index" and "Task ledger" are the source of truth for what is done and pending. Do not restate status in this file.

## Commands

Run from this directory. The toolchain is pinned to Rust 1.92.0 in `rust-toolchain.toml`. Crate-specific commands (single test files, ignored tests, self-checks, benchmarks) are in each crate's `CLAUDE.md`.

```bash
# Build (only the binary and its target's dependencies; --workspace also builds every member)
cargo build -p dedupe
cargo build -p dedupe --target x86_64-pc-windows-gnu
cargo build --workspace --all-targets

# Lint and format (there is no CI; run these by hand before a commit)
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings

# OS boundary check (project script; needs approval like other project scripts)
cargo run -p dedupe-xtask -- boundaries

# Windows cross-check from macOS/Linux (compile and lint only; see "Windows cross-check")
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo check --workspace --all-targets --locked --target x86_64-pc-windows-gnu
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo clippy --workspace --all-targets --all-features --locked --target x86_64-pc-windows-gnu -- -D warnings

# Dependency policy (manual; needs cargo-deny installed locally)
cargo deny check

# Run the CLI
cargo run -p dedupe -- scan <root>            # persists to <root>/.dedupe.db
cargo run -p dedupe -- scan --no-db <root>    # naive pipeline, no DB
cargo run -p dedupe -- report                 # finds the DB in --dir (default cwd)
cargo run -p dedupe -- lookup <file>
cargo run -p dedupe -- db info

# Tests (run only when the user asks; see Working conventions)
cargo test --workspace --all-targets
```

### Windows cross-check

Compile and lint the Windows targets from a macOS or Linux host. This proves the code type-checks and passes clippy for `x86_64-pc-windows-gnu`. It runs nothing.

One-time setup:

```bash
rustup target add x86_64-pc-windows-gnu --toolchain 1.92.0
brew install mingw-w64   # provides x86_64-w64-mingw32-gcc and x86_64-w64-mingw32-ar
```

Run the two Commands lines marked "Windows cross-check" after any edit that touches Windows-reachable code: `backend-generic`, `backend-windows`, `win-sys`, or `cli`.

## Architecture

### Crate map

| Crate | Package | Role | Guide |
|---|---|---|---|
| `crates/core` | `dedupe-core` | All scan logic: pipelines, passes, DB, resume, report, lookup index. No OS code. | `crates/core/CLAUDE.md` |
| `crates/backend-generic` | `dedupe-backend-generic` | Portable backend: walker, positional reads, volume identity, path codec, storage probe. Per-OS code under `src/sys/`. | `crates/backend-generic/CLAUDE.md` |
| `crates/backend-windows` | `dedupe-backend-windows` | NTFS `$MFT` fast path, open-by-id reads, overlapped read-ahead. Linked only into Windows builds. | `crates/backend-windows/CLAUDE.md` |
| `crates/win-sys` | `dedupe-win-sys` | Shared Win32 handle and path FFI used by both backends. | `crates/win-sys/CLAUDE.md` |
| `crates/cli` | `dedupe` | Thin consumer: args, progress, output, storage-profile resolution, backend wiring. | `crates/cli/CLAUDE.md` |
| `xtask` | `dedupe-xtask` | Dev tasks: `boundaries` check, `schema` stub. | `xtask/CLAUDE.md` |
| `bench` | `dedupe-bench` | Standalone benchmark binary, user-run only. | `bench/CLAUDE.md` |
| `crates/tui` | `dedupe-tui` | Stub. | - |

### How the crates connect

- `core` reaches the OS only through the traits in `crates/core/src/traits.rs` (`Enumerator`, `ContentReader`, `PathCodec`, `VolumeIdentity`, `Hasher`, `StorageProbe`), bundled per call in `pipeline::Services`. Backends implement them; the CLI picks the implementations and passes them in.
- `crates/cli/src/platform/` builds the scan backend (`make_backend`): `windows.rs` on Windows (`AutoEnumerator`, `HandleReader`), `generic.rs` elsewhere. Two other CLI paths use `backend-generic` types directly: `scan.rs` probes storage with `GenericStorageProbe` before the backend exists, and `lookup.rs` uses `GenericBackend::new()` on every OS.
- Per target, only that target's code is compiled. A Linux or macOS build of `dedupe` pulls in `core` and `backend-generic` only; a Windows build adds `backend-windows`, `win-sys` and `windows-sys`. Inside `backend-generic`, only one `sys/<os>/` module compiles.
- A new platform is a new `crates/backend-generic/src/sys/<os>/` implementing the `Platform` trait plus a cfg arm in `sys/mod.rs`, or a new `backend-*` crate implementing the core traits.

### OS code boundaries

- `core`: `#![forbid(unsafe_code)]`, no OS `cfg`.
- `cli`: `#![forbid(unsafe_code)]`; OS `cfg` only in `src/platform/mod.rs`.
- `backend-generic`: `#![deny(unsafe_code)]` with local opt-in; OS `cfg` only under `src/sys/` and `src/parse/`.
- `backend-windows` and `win-sys`: `#![deny(unsafe_code)]` with local opt-in in their Win32 modules.
- `cargo run -p dedupe-xtask -- boundaries` enforces the `cfg` rules (substring match; comment lines are skipped).

## Cross-crate gotchas

- `FileMeta.dev` (Unix `st_dev`, Windows low 32 bits of the volume serial) is runtime only. With `--cross-mounts`, `pass_enumerate::DevScope` namespaces the `file_id` of files on another device as `blake3(foreign uuid)[..8] || id`. The Windows walker, `ContentReader::metadata` (through `meta::file_meta_at`) and the `$MFT` path (`tree.rs` `file_meta_for`) all set `dev` on Windows when the handle open succeeds. Volumes whose serials match in the low 32 bits can still merge ids in `scan --no-db` (T4.2 full 64-bit id is open).
- File identity on Windows: the walker opens each regular file once (`dedupe_win_sys::file_info`) and takes `file_id`, `nlink`, size, times and attributes from that open. NTFS ids are 8 bytes and are meant to equal the `$MFT` file references, but this is unverified on hardware, so assume switching between walker and `$MFT` re-hashes files. ReFS 128-bit ids are stored as 16 bytes (not truncated) and are never opened by id. Unix reads the inode directly.
- Schema version is 2 and there are no migrations: an older DB fails with a "delete it and scan again" error.
- The hash is BLAKE3 truncated to 128 bits (`blake3-128`). `meta.hash_algo` and the prefix sizes must match the DB.
- Build matrix: lint and `cargo check` run on the Linux host and for `x86_64-pc-windows-gnu` (mingw). Nothing compiles the macOS code (`crates/backend-generic/src/sys/macos/`, `crates/backend-generic/src/sys/other.rs`) or MSVC or aarch64 from this host; check those on a Mac or Windows machine. Win32 code is compile-checked only; no test on this host executes it.
- `scale.rs` (core) and `kill_fuzz.rs` (cli) tests are `#[ignore]`. Plain `cargo test` skips them.

## Working conventions
ALWAYS use 'rust-best-practises' skill while working on rust code and systems.

- Lint and format checks can run without asking. Builds and tests run only when the user asks for them. When they do run, use a subagent.
- Benchmarks are deferred to the end of the plan and run by the user on reference hardware. Do not run them as verification.
- Plan status and task IDs live in `.overall_plan.md`. Read that file before starting work on any task ID.
- When a change moves or renames code, update the affected crate's `CLAUDE.md` in the same change, and this file only if a cross-crate fact changed.

<!-- rtk-instructions v2 -->
# RTK (Rust Token Killer) - Token-Optimized Commands

## Golden Rule

**Always prefix commands with `rtk`**. If RTK has a dedicated filter, it uses it. If not, it passes through unchanged. This means RTK is always safe to use.

**Important**: Even in command chains with `&&`, use `rtk`:
```bash
# ❌ Wrong
git add . && git commit -m "msg" && git push

# ✅ Correct
rtk git add . && rtk git commit -m "msg" && rtk git push
```

## RTK Commands by Workflow

### Build & Compile (80-90% savings)
```bash
rtk cargo build         # Cargo build output
rtk cargo check         # Cargo check output
rtk cargo clippy        # Clippy warnings grouped by file (80%)
rtk tsc                 # TypeScript errors grouped by file/code (83%)
rtk lint                # ESLint/Biome violations grouped (84%)
rtk prettier --check    # Files needing format only (70%)
rtk next build          # Next.js build with route metrics (87%)
```

### Test (60-99% savings)
```bash
rtk cargo test          # Cargo test failures only (90%)
rtk go test             # Go test failures only (90%)
rtk jest                # Jest failures only (99.5%)
rtk vitest              # Vitest failures only (99.5%)
rtk playwright test     # Playwright failures only (94%)
rtk pytest              # Python test failures only (90%)
rtk rake test           # Ruby test failures only (90%)
rtk rspec               # RSpec test failures only (60%)
rtk test <cmd>          # Generic test wrapper - failures only
```

### Git (59-80% savings)
```bash
rtk git status          # Compact status
rtk git log             # Compact log (works with all git flags)
rtk git diff            # Compact diff (80%)
rtk git show            # Compact show (80%)
rtk git add             # Ultra-compact confirmations (59%)
rtk git commit          # Ultra-compact confirmations (59%)
rtk git push            # Ultra-compact confirmations
rtk git pull            # Ultra-compact confirmations
rtk git branch          # Compact branch list
rtk git fetch           # Compact fetch
rtk git stash           # Compact stash
rtk git worktree        # Compact worktree
```

Note: Git passthrough works for ALL subcommands, even those not explicitly listed.

### GitHub (26-87% savings)
```bash
rtk gh pr view <num>    # Compact PR view (87%)
rtk gh pr checks        # Compact PR checks (79%)
rtk gh run list         # Compact workflow runs (82%)
rtk gh issue list       # Compact issue list (80%)
rtk gh api              # Compact API responses (26%)
```

### JavaScript/TypeScript Tooling (70-90% savings)
```bash
rtk pnpm list           # Compact dependency tree (70%)
rtk pnpm outdated       # Compact outdated packages (80%)
rtk pnpm install        # Compact install output (90%)
rtk npm run <script>    # Compact npm script output
rtk npx <cmd>           # Compact npx command output
rtk prisma              # Prisma without ASCII art (88%)
```

### Files & Search (60-75% savings)
```bash
rtk ls <path>           # Tree format, compact (65%)
rtk read <file>         # Code reading with filtering (60%)
rtk grep <pattern>      # Search grouped by file (75%). Format flags (-c, -l, -L, -o, -Z) run raw.
rtk find <pattern>      # Find grouped by directory (70%)
```

### Analysis & Debug (70-90% savings)
```bash
rtk err <cmd>           # Filter errors only from any command
rtk log <file>          # Deduplicated logs with counts
rtk json <file>         # JSON structure without values
rtk deps                # Dependency overview
rtk env                 # Environment variables compact
rtk summary <cmd>       # Smart summary of command output
rtk diff                # Ultra-compact diffs
```

### Infrastructure (85% savings)
```bash
rtk docker ps           # Compact container list
rtk docker images       # Compact image list
rtk docker logs <c>     # Deduplicated logs
rtk kubectl get         # Compact resource list
rtk kubectl logs        # Deduplicated pod logs
```

### Network (65-70% savings)
```bash
rtk curl <url>          # Compact HTTP responses (70%)
rtk wget <url>          # Compact download output (65%)
```

### Meta Commands
```bash
rtk gain                # View token savings statistics
rtk gain --history      # View command history with savings
rtk discover            # Analyze Claude Code sessions for missed RTK usage
rtk proxy <cmd>         # Run command without filtering (for debugging)
rtk init                # Add RTK instructions to CLAUDE.md
rtk init --global       # Add RTK to ~/.claude/CLAUDE.md
```

## Token Savings Overview

| Category | Commands | Typical Savings |
|----------|----------|-----------------|
| Tests | vitest, playwright, cargo test | 90-99% |
| Build | next, tsc, lint, prettier | 70-87% |
| Git | status, log, diff, add, commit | 59-80% |
| GitHub | gh pr, gh run, gh issue | 26-87% |
| Package Managers | pnpm, npm, npx | 70-90% |
| Files | ls, read, grep, find | 60-75% |
| Infrastructure | docker, kubectl | 85% |
| Network | curl, wget | 65-70% |

Overall average: **60-90% token reduction** on common development operations.
<!-- /rtk-instructions -->