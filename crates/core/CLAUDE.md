# crates/core (`dedupe-core`)

All logic of deDupe: the two scan pipelines, the SQLite state store, lookup, reports, and the traits
that each backend implements. It depends on no other workspace crate. `dedupe-backend-generic`,
`dedupe-backend-windows`, `dedupe-cli` and `dedupe-tui` depend on it, and so does `bench/`. The
workspace layering and cross-crate rules are in the root CLAUDE.md.

## Module map
- `src/lib.rs`: `#![forbid(unsafe_code)]`, the module list, re-exports `CoreError` and `Result`.
- `src/traits.rs`: the OS boundary. `Enumerator`, `ContentReader` (with `FileHandle`, `ReadTarget`,
  `AccessPattern`), `Hasher`, `VolumeIdentity`, `PathCodec`, `StorageProbe`, plus `WalkItem` and
  `FileMeta`. `Action` is a placeholder trait (`describe` only) for the actions that land in Phase 7.
- `src/model.rs`: `Hash` (16 bytes), `State` (`files.state`), `Phase` (`run_volumes.phase_reached`),
  `is_legal_transition`, `attrs` bits, row structs (`FileEntry`, `FolderRow`, `ErrRow`), enums
  (`ErrorKind`, `SkipReason`, `FsType`, `UuidKind`), `now_unix_ns`.
- `src/config.rs`: `Config`, every tunable in one struct. `VerifyMode`. `prefix_total()` is F + L.
- `src/error.rs`: `CoreError`, the single error type (`thiserror`).
- `src/path.rs`: `RawPath`, lossless path bytes, with display and terminal escaping. `render(mask)` gives the
  masked string or the terminal-safe form. `json_fields(obj, mask)` writes `path` (and `path_bytes` when the
  bytes are not UTF-8) unmasked, and only the masked `path` when `mask` is `Some`.
- `src/time.rs`: FILETIME and `SystemTime` to signed nanoseconds since the Unix epoch.
- `src/hash.rs`: `HASH_ALGO` (`blake3-128`), `hash_prefix_target`, `hash_full_target`, `read_full`.
- `src/identity.rs`: `run_key` (naive hardlink key), `volume_tag`, `namespaced_file_id` (`--cross-mounts`).
- `src/exclude.rs`: `ExcludeSet`, the compiled `--exclude` globs and `--exclude-dir` directories.
- `src/cancel.rs`: `Token`, the cooperative cancel flag.
- `src/progress.rs`: `Sink` trait, `SharedSink`, `PhaseProgress`, `emit_*` helpers.
- `src/storage_profile.rs`: `Profile`, `StorageKind`, `ProfileChoice`, `resolve()`.
- `src/scheduler.rs`: `Scheduler`, which orders rows by first extent on HDD and leaves them alone on SSD.
- `src/report.rs`: `Report`, `Group`, `Member`, `ReportEvent`, JSON and human rendering, each with an optional
  `Masker` (see Masked output).
- `src/mask.rs`: `Masker` (per-export word and extension token maps), the vault schema (`exports`, `tokens`,
  `ext_tokens`), `ALLOW`, `ALLOW_PREFIX`, `EXT_ALLOW`, `MEDIA_EXT`, `WORD_ALLOW`, and the reverse side: `unmask`,
  `check_db_uuid`, `ext_map`. The vault is a separate SQLite file, not part of schema v2.
- `src/lookup.rs`: `lookup` and `lookup_report`, matching one query file against indexed rows.
- `src/db/mod.rs`: `Db::open` (read-write), `Db::open_readonly`, pragma policy.
- `src/db/schema.rs` and `src/db/schema/v2.sql`: schema, `user_version` check, no migrations.
- `src/db/ids.rs`: `IdAllocator`, pre-assigned file and folder ids.
- `src/db/lock.rs`: `<db>.lock` advisory lock (`acquire`, `acquire_with`).
- `src/db/writer.rs`: `Batch` enum and the writer thread that owns the read-write connection.
- `src/db/queries.rs`: typed SQL helpers (rows, upserts, runs, volumes, groups, path reconstruction).
- `src/pipeline/mod.rs`: `Services`, `Ctx`, `VolumeScan`, `PathCache`, `RESULT_CHUNK`, `send_chunk`.
- `src/pipeline/scan.rs`: `run_scan` (persistent orchestrator) and `run_passes`.
- `src/pipeline/resume.rs`: config guard, `init_meta`, `plan_resume` (volume UUID and mount checks).
- `src/pipeline/naive.rs`: `scan_naive`, the in-memory pipeline behind `scan --no-db`.
- `src/pipeline/pass_enumerate.rs`: walk, upsert folders and files, move detection, merge with prior rows.
- `src/pipeline/rescan.rs`: `delete_stale`, which queues `Batch::DeleteStale`.
- `src/pipeline/pass_size.rs`, `pass_prefix.rs`, `pass_full.rs`, `pass_verify.rs`: the hash passes.

## How it works
Persistent scan (`pipeline::scan::run_scan`):
1. Hold `<db>.lock` for the whole call. Open the DB read-write (pragmas, then schema). Run
   `check_config_guard`. On a fresh DB, `init_meta`.
2. New run: canonicalize roots, `upsert_volume` per volume, abandon overlapping running runs, insert
   `scan_runs`, `run_volumes`, `run_roots`. `--resume`: `plan_resume` takes the latest running run
   (global, not per volume).
3. Open a read-only connection for the passes, seed `IdAllocator` from `MAX(id)`, spawn the writer.
4. Per volume: `pass_enumerate`, `pass_size`, `pass_prefix`, `pass_full`, then `pass_verify` only with
   `VerifyMode::Byte`. Each pass ends with `PhaseComplete`. A pass whose stored phase is already
   reached is skipped.
5. Verify off: `PromotePending` moves leftover state-3 rows to 5, then `flush`.
6. After `writer.shutdown()`, and only when the run was not stopped early: in one IMMEDIATE
   transaction, `recompute_groups` for each volume below `GroupsDone`, set `GroupsDone`, `finish_run`.
   A stopped run stays `running` and can be resumed.

`ScanOptions::stop_after` compares against `Phase` values. The `Some(0)` case stops before enumeration.

Naive scan (`scan_naive`): walks roots into memory, groups by size, then prefix, then full hash, then
`Report::normalize`. No DB, no resume, `volume_id` is 0. It is the correctness reference. The `hasher`
argument is ignored. It canonicalizes each root, and it drops excluded directories and their subtrees
by tracking parents, so it does not depend on the enumerator pruning.

Lookup (`lookup_report`): stat the query file (symlink, non-regular, zero-length and placeholder files
are `Skipped`). Resolve its volume and row key so the file never matches itself. Probe candidates by
size among rows not in -1 or -2, hash on demand. With `insert`, write everything in one IMMEDIATE
transaction. The caller holds the lock.

Threads:
- `db-writer` owns the only read-write `Connection`.
- The pass thread owns `ctx.read` (opened `SQLITE_OPEN_NO_MUTEX`). It never leaves that thread.
- With `queue_depth > 1`, `pass_full` runs `hash_parallel` on `thread::scope` workers. Workers only
  read and stat. They return `WorkerResult` over an mpsc channel. The pass thread judges each row and
  sends the batches.

## Write path
- Passes send `Batch` values over a `sync_channel(4)`. Hash workers do not send batches. Their results
  go to the pass thread first, which judges each row and then sends the batch.
- The writer commits when pending rows reach `BATCH_ROWS` (50,000) or `BATCH_MS` (5 s) has elapsed. It
  also commits immediately on `PhaseComplete`, on `Flush` (the sender blocks until the commit lands), and
  on `Shutdown` (commit, then `wal_checkpoint(TRUNCATE)`).
- `flush()` is how the read connection sees a finished phase before the next pass selects rows.
- Stale-row deletion goes through the writer as `Batch::DeleteStale`.

## File state machine
`files.state`. Each pass consumes one state and writes the next.

| Value | State | Set by |
|---|---|---|
| 0 | Enumerated | `pass_enumerate` (every seen row; rescan resets to 0) |
| 1 | SizeCandidate | `pass_size`, size shared by more than one inode |
| 2 | PrefixHashed | `pass_prefix`, large file (size > F + L) |
| 3 | FullHashed | `pass_full` or small-file path, verify on (transient) |
| 4 | Verified | `pass_verify`, byte-equal |
| 5 | Done | small-file path or `pass_full` with verify off, `PromotePending` |
| -1 | Skipped | enumeration: zero-length, non-regular, placeholder |
| -2 | Error | any pass, with an `errors` row |
| -3 | UniqueSize | `pass_size` |
| -4 | UniquePrefix | `MarkPrefixSingletons` |

Transitions are forward only within a run. Two things to know:
- With `--verify byte`, verified rows end in 4. No code path moves 4 to 5, so the `Done` value does not
  appear. Reports and lookup accept 4, so this is harmless today. It differs from `is_legal_transition`
  (`Verified -> Done`).
- `is_legal_transition` is called only by unit tests and `props.rs`. The real guards are the SQL
  `WHERE` clauses in `writer.rs` (for example `Verified` only updates `state=3`).

`Phase` order: `None`, `Enumerated`, `SizeDone`, `PrefixDone`, `FullDone`, `Verified`, `GroupsDone`.

## Resume and rescan
- Resume restarts from `run_volumes.phase_reached`, not from a full re-scan. Enumeration is an
  idempotent upsert, so a crash loses at most one batch window.
- Config guard: `meta.hash_algo`, `prefix_first_bytes` and `prefix_last_bytes` must match, or
  `HashConfigMismatch`. With `cfg.force`, the guard sets every non-skipped row back to state 0 and their
  hashes to NULL, caps running runs' phase at `Enumerated`, and clears `dup_groups`. Queue depth, buffer size, profile and exclude are not part of the guard because
  they do not change hash output.
- Volume guard (`plan_resume`): each run root is identified. A UUID mismatch is `VolumeMismatch` unless
  forced. A moved mount is updated in place.
- Rescan (a new run over an indexed volume): a row is claimed by path first, then by move detection on
  `(volume_id, file_id)` plus size and mtime. A claimed row keeps its hashes only when
  `unchanged_since` holds: size, mtime, ctime (compared only when both sides have one) and file id.
- Stale deletion is scoped to this run's root folders with a recursive CTE. Folders the walker could
  not read are protected, so their rows survive. Rows under roots not scanned this time are untouched.

## Reading results
- `Report::from_db_with` reads `dup_groups`, which `recompute_groups` rewrites per run. Members are
  rows in states 3, 4 and 5 last seen by a finished run, so roots a later scan skipped remain listed.
- Header counts come from each volume's latest finished run (`completed_volumes`).
- `unfinished_newer_run` reports a running run newer than every finished one.
- `ReportEvent` is `Volume { volume_id, groups }`, `Group { done }`, then `Done`. Cancellation is checked before
  each group.
- Lookup considers every live row not in -1 or -2. Its freshness check (`is_fresh`) compares size,
  mtime, ctime and file id.

## Masked output
- `Masker::begin(vault, db_uuid)` opens or creates the vault (WAL, 5 s busy timeout), inserts an `exports` row
  and returns the masker plus the vault `Connection`. Tokens are per export: each distinct word gets `W<n>`
  and each media extension `E<n>`, counted from 1.
- `Masker::mask(&RawPath)` keeps the root marker (`X:`, `\\`, `\\?\`, `\\?\UNC\`), splits on the path's own
  separator and masks each segment. Allowlisted segments (`ALLOW`) stay readable whole. An `ALLOW_PREFIX`
  (`cmake-`, `.venv`) keeps only the prefix readable and word-masks the rest, so `cmake-AcmeClient` becomes
  `cmake-W<n>W<n>`. A final `.ext` in `EXT_ALLOW` stays readable, and a `MEDIA_EXT` suffix becomes `.E<n>`. Other segments split at
  ASCII non-alphanumerics (kept verbatim) and at lower-to-upper camelCase. Each word is readable only if it is
  in `WORD_ALLOW`, 1-4 digits, or `v` plus 1-3 digits; otherwise `W<n>`. The result is always valid UTF-8.
- Masked `Report::to_json(Some(m))` has no `full_hash`, only a masked `path` per member (never `path_bytes`),
  members sorted by masked string (`masked_members`), and a top-level `mask_export`. `to_human` starts with
  `# mask export <id>` and drops the hash from each group line.
- `Report::stream_human` takes `Option<(&mut Masker, &mut Connection)>`. When masked, each group is rendered
  into a buffer, the new tokens are flushed to the vault, and only then is the group written to `out`.
- `unmask(input, export_id, vault, in_dup_group)` is JSON only. It rebuilds every string that starts with a root
  marker, with or without `W<n>`/`E<n>` tokens, and scope-checks each one through `in_dup_group`, so a token-free
  real path is refused unless it is in a duplicate group. Other values are copied as is. It errors, listing all
  offenders, on unknown tokens, on rebuilt paths for which `in_dup_group` is false, and on non-UTF-8 paths outside a `path` key. Under
  a `path` key a non-UTF-8 result becomes a lossy `path` plus a `path_bytes` hex sibling. `check_db_uuid` is a
  separate call so callers without a DB can skip it.
- `queries::file_in_dup_group(conn, path)`: true when the path is an indexed file and another row shares its
  `full_hash`. It resolves the folder chain through each volume's stored mount, as `lookup` does. It is the
  guard that stops a spliced path from passing unmask.

## Progress and cancellation
- `progress.rs`: a pass calls `phase_started`, then `advance` (the sum of advances equals `total`), then
  `phase_finished`. Emit helpers ignore a poisoned lock. Core never prints. It logs through `tracing`.
- `cancel.rs`: `Token` is an `Arc<AtomicBool>`. `check()` returns `CoreError::Cancelled`. Passes check
  it per row or per batch. `run_scan` still joins the writer, so batches already sent are committed.
- The CLI sets the token from its Ctrl-C handler and the Windows `$MFT` path also reads it. Those
  details live in `crates/cli/CLAUDE.md` and `crates/backend-windows/CLAUDE.md`.

## Invariants and rules
- `src/` has no `unsafe` and no OS `cfg`. The only `cfg` in `src/` is `cfg(test)`. `xtask boundaries`
  checks `crates/core/src` and rejects `[target.` dependencies in `Cargo.toml`.
- Integration tests under `tests/` are outside that check. They use `cfg(unix)` and `cfg(windows)` for
  their in-test backends.
- OS access happens only through the traits in `traits.rs`, bundled in `Services`. Content reads go
  through `ContentReader`. The remaining `std::fs` uses are `canonicalize` (`scan.rs`, `lookup.rs`) and
  the lock file (`lock.rs`).
- Only the writer holds a read-write connection during a pass. Direct writes happen before the writer is
  spawned, after it shuts down (in `run_scan`), in `lookup --insert` under the lock, and in the CLI,
  which reopens the DB read-write after `run_scan` returns to set `scan_runs.enumerator`.
- `PhaseComplete` must be the last batch of its phase. Its commit carries everything queued before it.
  `MarkPrefixSingletons` precedes `PhaseComplete{PrefixDone}`. `DeleteStale` precedes
  `PhaseComplete{Enumerated}`. Keep these orders.
- Batches are idempotent per row id, so a partial commit is safe. Resume selects by state.
- A hash is kept only if a fresh stat still matches the row (`meta_matches`). A change during the read
  becomes `ChangedDuringRead`.
- Hashes are BLAKE3 truncated to 128 bits. Changing `HASH_ALGO` or the prefix sizes changes what stored
  hashes mean. The `meta` values and `check_config_guard` are the only protection, so keep them in step.
- One schema version (`EXPECTED_VERSION`, currently 2). Any other version is refused, with no migration.
  `open_readonly` also checks it.
- Errors are `CoreError` values through `Result`. Use `tracing` for logs.
- Scan invariant for masking: readable output text must never contain `[WE][0-9]`, or `unmask` could not find
  tokens by scanning. `allowed_exact` rejects an allowlisted segment that holds one, `allowed_prefix_len` only
  keeps the prefix of an `ALLOW_PREFIX` match readable, and `readable_word`
  admits only digits and `v<digits>`, never a `W` or `E` followed by a digit.
- Flush before output: `Masker::flush` must run before any masked text is written, so every token that
  reaches a reader is already in the vault. `stream_human` does this per group.

## Gotchas
- Only one process may write a database. `scan` and `lookup --insert` hold `db/lock.rs` (a `<db>.lock`
  sidecar, `File::try_lock`, so `rust-version` is 1.89). Read-only commands take no lock. If the
  filesystem reports file locking as unsupported (likely on WSL `/mnt/c`), `acquire_with` errors unless
  `--no-lock` is passed. The scan skips `-wal`, `-shm` and `.lock` siblings of its own DB, and the mask vault (`VAULT_NAME`, with `-wal` and `-shm`) in the DB folder.
- Enumeration ids are claimed per run in `pass_enumerate` (`Claim::Path` and `Claim::Move`). Folders
  resolve through an in-run overlay before the read snapshot. Do not look folders or files up on
  `ctx.read` alone.
- Hash passes stream results in `RESULT_CHUNK` batches. Unique prefixes are marked by one SQL statement
  (`Batch::MarkPrefixSingletons`) that commits with the `PrefixDone` marker. Stale deletion commits with
  the `Enumerated` marker. Keep both orderings.
- The walker emits `WalkItem::Error`. Unreadable directories are passed to `Batch::DeleteStale` as
  protected folders, so their old rows survive.
- `FileMeta.dev` is runtime only. With `--cross-mounts`, `DevScope` in `pass_enumerate.rs` rewrites the
  `file_id` of a file on another device to `namespaced_file_id(volume_tag(uuid), id)`, so ids from two
  volumes cannot collide. The tag comes from the foreign volume's uuid, so rescans keep their hashes.
  `DevScope` stays off when `metadata` reports no `dev`.
- `--exclude` and `--exclude-dir` matching lives in `exclude.rs`. Matching is on the full path with `/`
  separators and ignores case, for globs and directories alike. `--exclude-dir` rules are absolute
  normalized prefixes that match whole components (`/a/build` covers `/a/build/x`, not `/a/buildx`).
  `ExcludeSet::new` expects canonical absolute dirs; the CLI resolves them. An excluded directory is
  never registered, so `pass_enumerate` drops every file under it, including on the `$MFT` path, which
  cannot prune.
- Schema version 2 has no migrations. An older DB fails with "delete it and scan again".
- The vault (`.dedupe.mask.db`) is a secret: it holds the real word and extension bytes behind every token. Core
  never decides its location or permissions (the CLI does). Masking hides names, not sizes, depth, allowlisted
  folders, readable extensions or word frequency within one export.
- `Services::hasher` is set by callers but never read in core. Hashing calls `hash::hash_bytes` and BLAKE3
  directly. `naive.rs` ignores its `hasher` argument as well.
- `Config::mem_budget` is not read by any pass.
- Core has no `test-hooks` feature. The `dedupe` (cli) feature `test-hooks` gates `--stop-after-pass`;
  `ScanOptions::stop_after` always compiles.
- Two types named `PathCache` exist. `pipeline::PathCache` is per pass and cached. `db::queries::PathCache`
  is used by `report.rs`. Do not mix them up.

## Commands
```bash
cargo clippy -p dedupe-core --all-targets --all-features -- -D warnings   # lint, no approval needed
cargo test -p dedupe-core --lib                                           # unit tests (user-run only)
cargo test -p dedupe-core --test phase1                                   # one integration file
cargo test -p dedupe-core --test phase1 <name_filter>                     # one test in it
DEDUPE_SCALE_N=1000000 cargo test -p dedupe-core --test scale -- --ignored
```

## Tests
- Unit tests sit in `#[cfg(test)]` modules inside `src/` (for example `model.rs`, `hash.rs`,
  `db/writer.rs`, `db/schema.rs`, `scheduler.rs`, `storage_profile.rs`, `traits.rs`).
- `src/mask.rs` `#[cfg(test)]` module: byte-exact round trip (with `simplify_verbatim` as the expected bytes),
  token lookalikes, name hiding (`ALLOW_PREFIX` leak), unknown and foreign-export tokens, the scope gate for
  token-free paths, the DB uuid check, and non-UTF-8 and free-text handling.
- `tests/phase1.rs`: resume at every batch boundary, lookup (including insert), rescan, move detection,
  the volume-UUID and hash-config guards, an abandoned run, root-scoped rescan, non-UTF-8 round trip, and `scan_naive` dropping an `--exclude-dir` subtree.
- `tests/pool.rs`: `queue_depth` 4 gives the same results as 1.
- `tests/progress.rs`: balanced advances per pass, and the null sink.
- `tests/verify.rs`: byte verify sets state 4. A file changed after hashing is an error. Hardlinks
  collapse. Verify off promotes pending rows to 5. Short reads are not mismatches.
- `tests/props.rs`: proptest round trip of `FileEntry` through the DB (raw and wide names, NFC/NFD pairs),
  and transition-table properties (terminal states, random walks).
- `tests/scale.rs`: two `#[ignore = "scale: user-run"]` tests on a large tree. Default 1,000,000 files,
  set with `DEDUPE_SCALE_N`. They are not run by plain `cargo test`.
- Each integration file copies its own walker, codec, reader and fake volume. This is intentional.
  Do not extract a shared test module.
- `tests/fixtures/tree/` holds a small fixture tree.
