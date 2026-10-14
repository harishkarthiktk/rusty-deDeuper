# Phase 1 — Database, resume, rescan, lookup

Source of truth for behavior is `.plan.md` sections 4.3–4.13. This document
describes what shipped and the two places where the implementation extends the
plan (see "Deviations").

## Layout

```
dupfind/
  crates/core/            # safe-only: #![forbid(unsafe_code)], no cfg
  crates/backend-generic/ # std::fs walk + pread, path codec, volume identity
  crates/cli/             # dupfind binary
  crates/tui/             # stub (Phase 7)
  bench/                  # synthetic DB benchmark
  xtask/                  # dev tasks
```

All OS-specific logic and `unsafe` live in `backend-*`. `core` consumes only the
safe traits in `core::traits`.

## Schema (v1)

`PRAGMA user_version` is the only schema version source; `core::db::schema`
creates v1 on a fresh DB and refuses a newer version. Tables: `meta`, `volumes`,
`folders`, `files`, `dup_groups`, `scan_runs`, `run_volumes`, `run_roots`,
`errors`.

Paths are stored as `BLOB` raw bytes: `folders.name` and `files.name` are single
components; `volumes.mount` and `run_roots.root` are full paths. Nothing is
normalized on write. `RawPath::to_display()` lossily decodes to UTF-8 and
normalizes to NFC for display only.

Two indexes are partial and carry semantics: `idx_files_size` excludes
`SKIPPED`/`ERROR`, and `idx_files_prefix`/`idx_files_full` exclude NULL hashes.
`idx_files_full` is volume-agnostic so `lookup` can span volumes.

## State machine

```
 0 ENUMERATED   1 SIZE_CANDIDATE   2 PREFIX_HASHED   3 FULL_HASHED   4 VERIFIED   5 DONE
-1 SKIPPED     -2 ERROR    -3 UNIQUE_SIZE    -4 UNIQUE_PREFIX
```

Passes: `pass_size` consumes 0, `pass_prefix` consumes 1, `pass_full` consumes 2.
Without `--verify`, `pass_full` writes 5 directly. Small files
(`size <= prefix_first_bytes + prefix_last_bytes`) get both hashes in one
`Batch::Hashed` and skip `pass_full`. Hardlinks (same `(volume_id, file_id)`)
collapse: size grouping counts distinct inodes, so a hardlink-only pair is
`UNIQUE_SIZE`, not a duplicate. `attrs.hardlink` is set at enumeration.

`run_volumes.phase_reached` records the last fully-committed phase; a phase marker
is written in the same transaction as the rows it covers (`Batch::PhaseComplete`).

## Writer

A single writer thread owns the `rusqlite::Connection`. Workers send `Batch`
messages over a bounded channel. Rows commit when `BATCH_ROWS = 50_000` or
`BATCH_MS = 5000` elapse; `PhaseComplete` forces a commit including all rows
received before it. All writes are idempotent (upsert + `UPDATE ... WHERE id`).
On `Shutdown` the writer commits and runs `wal_checkpoint(TRUNCATE)`.

WAL is enabled at open and the *returned* `journal_mode` is checked; anything
other than `wal` fails with `WalUnavailable`.

## Resume (4.7)

`scan --resume`:

1. `meta.hash_algo` and the prefix config must match; otherwise refuse unless
   `--force` (which wipes hash columns and resets files to state 0).
2. The latest `scan_runs` row with `status='running'` is loaded; none →
   `NothingToResume`.
3. Each recorded root is identified with the backend `VolumeIdentity`; a UUID
   mismatch refuses unless `--force`. A matching UUID with a moved mount updates
   `volumes.mount`.
4. Each volume resumes at the pass after `phase_reached`.
5. At the end, `dup_groups` is recomputed for the volume, `phase_reached` is set
   to `GroupsDone`, and the run is marked finished.

An interrupted run is safe to resume because the DB only ever holds committed
transactions; the uncommitted batch window is redone.

## Rescan (4.7a)

A fresh `scan` (no `--resume`) marks any `running` run sharing a volume as
`abandoned`, then starts a new run. Enumeration merges with existing rows:

- found by `(volume_id, folder_id, name)`, unchanged (`size`, `mtime_ns`,
  `file_id`) → keep hashes, reset to state 0, bump `last_seen_run_id`;
- found but changed → null both hashes, reset to state 0;
- not found by path → try `(volume_id, file_id)` among rows not yet seen this
  run; a match counts as a move only if `size` and `mtime_ns` match;
- otherwise insert a new row.

After enumeration commits, rows not seen this run **whose folder descends from
one of this run's roots** (recursive CTE over `folders`) are deleted. Each pass
reuses a stored hash when `prefix_bytes` matches the current config, so an
unchanged rescan performs zero content reads.

## Lookup (4.8)

`dupfind lookup --db PATH FILE [--format json] [--insert]`. Read-only by default:

1. `stat`; non-regular / zero-length / placeholder → `Skipped`.
2. Candidates: same size, `state NOT IN (-1,-2)`, across every volume.
3. Compute the query prefix (small-file rule applies).
4. Candidates without a stored prefix are hashed on demand; if that fails they
   are reported `Unmounted`.
5. Full hash filters the survivors; candidates without a stored full hash are
   hashed on demand.
6. Each returned match is re-`stat`ed and labeled `Fresh` / `Stale` / `Missing`
   / `Unmounted`.

`--insert` persists on-demand hashes and, when the query file lives on an indexed
volume with an existing folder chain, inserts it as a row.

## CLI (4.13)

```
dupfind scan <ROOT>... [--db PATH] [--resume] [--force]
                        [--one-file-system | --cross-mounts]   # default one-file-system
                        [--include-placeholders] [--mem-budget BYTES]
                        [--stop-after-pass N]                  # cargo feature `test-hooks`
dupfind report --db PATH [--format json|human] [--min-wasted BYTES]
dupfind lookup --db PATH FILE [--format json] [--insert]
dupfind db     --db PATH info|vacuum|drop --yes
```

Ctrl-C sets the shared cancel token; long scans return `Cancelled`. Logs go to
stderr so `--format json` stays machine-parseable.

## Tests

- `crates/core` unit tests: schema apply/idempotence/version gate, query helpers,
  writer flush/replay, `IdAllocator`, index maps, small-file boundaries, state
  transitions, unicode round-trip.
- `crates/core/tests/phase1.rs`: resume at every phase boundary, lookup
  (basic/mutation/singleton/freshness), rescan (zero reads, modified, deleted),
  move detection, hardlinks, FIFO/socket skip, change-during-read, volume guard,
  hash-config guard, abandoned runs.
- `crates/core/tests/props.rs`: `FileEntry -> DB -> FileEntry` round-trip.
- `crates/cli/tests/kill_fuzz.rs`: kill -9 mid-scan then `--resume`, 20 iterations,
  normalized convergence.

Test runs are user-triggered.

## Deviations from `.plan.md`

Two `Batch` control messages were added beyond the 4.5 enum, both surfaced rather
than improvised:

- `Batch::Flush(ack)` — commit pending rows and acknowledge. Required because the
  pipeline's read-only connection must observe a committed phase before selecting
  the next pass's input rows (the plan's async writer has no barrier otherwise).
- `Batch::DeleteStale { run_id, volume_id, root_folder_ids }` — stale-row deletion
  (4.7a step 2) must run on the writer thread, which owns the single SQLite write
  connection.

Additionally, folder upserts always bump `last_seen_run_id` for reused folders;
without this, stale deletion would remove folders still referenced by files.

## Gates

- `core` compiles with `#![forbid(unsafe_code)]`.
- `cargo fmt --check`, `cargo clippy --all-targets -D warnings`, and
  `cargo deny check` (see `deny.toml`) are the CI gates.
