# bench (`dedupe-bench`)

Standalone benchmark binary, one file (`src/main.rs`). It depends on `dedupe-core` and
`dedupe-backend-generic`, plus `rusqlite` and `tempfile`. Nothing depends on it, and it has no tests.

## What it measures
It builds a synthetic DB in a temp dir (dropped on exit) with one volume and one folder, then prints:
- insert throughput (rows/s) for `--rows` synthetic file rows
- `queries::recompute_groups` time and group count
- `queries::size_candidates` time
- DB-only lookup proxy: 2,000 `COUNT(*)` queries by size, p50 and p99
- resume proxy: time to read the done rows, size candidates and prefix rows
- real lookup: `core::lookup` against a sparse query file (`set_len`), with 4 matching and 4 same-size
  non-matching candidates pre-hashed. 20 timed calls, p50 and p99. The doc comment expects each call to
  return exactly the 4 matches.

Defaults: 1,000,000 rows, 10 GiB sparse query file.

## How to run (user only)
```bash
cargo run -p dedupe-bench --release -- [rows]
cargo run -p dedupe-bench --release -- --rows N --lookup-file-bytes N
```
The doc comment names `-- 10000000` as the 10 M-row Definition-of-Done run.
Before a real run, `[profile.release]` in the root `Cargo.toml` has a shortcut comment: set
`lto = "thin"` and `codegen-units = 1`. Do not change it without the user.

## Rules
- Benchmarks are user-run on reference hardware, at the end of the plan (AUX-1 in `.overall_plan.md`).
- Never run `dedupe-bench` as verification of a change, and never run it before every other task is done.
- Lint and type checks on this crate are fine without approval: `cargo clippy -p dedupe-bench --all-targets -- -D warnings`.
- Output is `println!` only. Nothing is written outside the temp dir.
