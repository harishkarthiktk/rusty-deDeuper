# backend-windows (`dedupe-backend-windows`)

Windows backend for the NTFS fast path. It reads the `$MFT` directly instead of walking directories, serves content reads by stored NTFS file id, and runs overlapped read-ahead. The only caller is `crates/cli/src/platform/windows.rs`, which builds `AutoEnumerator` and `HandleReader`. It depends on `core`, `backend-generic` (the walker fallback, `FsContentReader`, `GenericPathCodec`), `win-sys` (Win32 handle and path helpers, Windows only), and `ntfs` pinned to `=0.4.0`. For layering and cross-crate rules, see the root CLAUDE.md; the two scan pipelines are in `crates/core/CLAUDE.md`.

## Module map

Portable means no `cfg(windows)` and no Win32 types: it builds and unit-tests on every OS. Win32 means `cfg(windows)` only.

Portable:
- `caps`: `Capabilities`, `Mode`, `FallbackReason`, and `choose_mode` (pure rule). `probe_capabilities` is Win32.
- `filetime`: `mtime_ns` (0 when out of range) and `ctime_ns` (`None` when out of range), over `dedupe_core::time::filetime_to_unix_ns`.
- `handle`: `plan_open` (sync or overlapped) and `file_id_to_frn` (8-byte ids only). Declared under `any(windows, test)`. `HandleReader` is Win32.
- `mft_cache`: the cache file format, `encode`/`decode`, `validate`, `write_atomic_stream`, `read` (size-checked), `cache_dir`, `cache_path`, `apply_delta`, and `max_touched`. Plain `std::fs` I/O.
- `mft_walk`: `walk_mft`, `read_tree`, `emit_tree`, the `RecordSource` trait, `NtfsRecordSource<R: Read + Seek>`, `MftEvent` and `MftObserver` (progress callback, re-exported from `lib.rs`), `MftError`, and `VolumeGeometry`. Tests use in-memory sources.
- `readahead`: `ReadAhead<B: AsyncBackend>`, the window logic. Tested with a fake backend.
- `records`: reads one `$MFT` record through `ntfs` into `RecordFacts`. The only module that reads records through `Ntfs::file`. `mft_walk.rs` also constructs the `Ntfs` root.
- `sector`: `SectorReader`, an aligned chunk cache that serves unaligned reads over any `Read + Seek`.
- `tree`: `VolumeTree`, `RecordFacts`, `NameFact`, path building, hard links, `kind_of` (reparse classification), `file_meta_of`.
- `usn`: `parse_usn_buffer`, `parse_journal_data`, `read_request_bytes`, `UsnError` (has `Cancelled`). `query_journal` and `read_journal` are Win32 (`cfg(windows)`).
- `volume_io`: `raw_volume_path`, `guid_volume_prefix`, `geometry_from_volume_data`, `volume_serial_from_volume_data`, `record_number_of`. Declared on every OS.

Win32 (`cfg(windows)` only):
- `caps` `win32::probe_capabilities`: elevation token, NTFS check, raw-volume open probe.
- `enumerator`: whole module. `AutoEnumerator`, `EnumNotices`, `CacheConfig`.
- `handle` `win32`: `HandleReader`, `SHARE_ALL`, `file_len`, `VolumeHint`.
- `overlapped`: `OverlappedFile` (overlapped `ReadFile`, events, `CancelIoEx`). Doc-hidden re-export for the tests.
- `volume_io` `win32`: `RawVolume`, `open_raw_volume`, `ntfs_volume_data`, `ntfs_volume_serial`, `frn_of`, `resolve_root`.

## How it works

Enumeration (`AutoEnumerator::walk`, the `Enumerator` trait):
1. `probe_capabilities(root)` and `choose_mode(caps, no_mft)` pick `Mft` or `Fallback(reason)`.
2. `Fallback`: push a notice (silent for `Disabled`), mark the walker used, run `WalkdirEnumerator`.
3. `Mft`: `walk_mft_volume` runs.
   a. `resolve_root` opens the root once by handle. It yields the record number, volume serial, and GUID volume path. The opened volume's serial must match the root's low 32 bits, or the scan falls back.
   b. `open_raw_volume`, `ntfs_volume_data`, `ntfs_volume_serial`.
   c. If `CacheConfig.dir` is set, load the cache (refused when the file is larger than `cfg.mem_budget`), `validate` it against the live volume and journal, and read the USN entries since its `next_usn`. This happens before any record is read. The USN read polls cancel at the start of every round.
   d. `obtain_tree`: a delta (`refresh_cached` + `apply_delta`) or a full `read_tree`. Any delta refusal becomes a notice, then a full read. A cancel in the USN read or the delta is not a refusal: it ends as `MftError::Cancelled` and stops the scan.
   e. `check_root`, then `write_atomic_stream` the cache. Skipped when the delta had zero entries or any record was skipped.
   f. `emit_tree` yields `WalkItem`s. Before the first item, `reach_directories` charges every directory path against the budget left by the records; over it, `EmitError::Budget` becomes `MftError::BudgetExceeded` and the walker runs. Paths start with the root exactly as the caller passed it.
4. Errors go through `classify`. A visitor error or cancel stops the scan with no fallback. Every other `MftError` (`BudgetExceeded`, `RecordFailed`, `RootUnusable`) falls back to the walker.

Content reads (`HandleReader::open_target`, a different path):
1. A stored 8-byte file id is opened with `OpenFileById` on a per-volume NTFS hint handle. The opened file's index must equal the id, otherwise the path is used. Other id lengths (ReFS) go straight to the path. A path open on an NTFS volume (hint present) with an 8-byte id is checked against that id too; a mismatch fails with `InvalidData` ("file changed since enumeration"), which the core hash passes record as a per-file `Io` error row.
2. The file is opened sync first, which gives its length. A sequential read of at least two chunks with at least two slots is reopened with `ReOpenFile` as overlapped, then read through `OverlappedFile` and `ReadAhead`. Otherwise it is a sync handle read with `seek_read`.

Threads: this crate spawns none. `HandleReader` keeps its volume hints behind an `RwLock` and its open count in an `AtomicU64`, so one instance can serve several threads.

## Invariants and rules

- Portable modules take no `cfg(windows)`, no `unsafe`, and no Win32 types. New Win32 code goes in the `cfg(windows)` items listed above.
- The crate root has `#![deny(unsafe_code)]`. Win32 modules opt in with `#![expect(unsafe_code, reason = ...)]`. Every `unsafe` block carries a `SAFETY:` comment that states its invariant.
- Fail closed. Any record error fails the whole `$MFT` read. A partial tree is never used and never cached.
- `VolumeTree::push` checks the memory budget (`cfg.mem_budget`). `emit_items` then charges each directory path plus `DIR_NODE_OVERHEAD_BYTES` (64) against what the records leave, the root prefix included. Exceeding either falls back to the walker. `emit_items` returns `EmitError::Budget` before it visits anything, so a fallback never repeats items.
- Delta limit: `max_touched(n)` = `max(100_000, n / 5)` distinct records. Above it the delta is refused and a full read runs.
- Cache format: `FORMAT_VERSION` is 2 (`MAGIC` `DDMFTC01`). Bump it for any change to what a stored field means, not only to the layout. Version 2 changed `ctime_ft` from creation time to change time.
- Cache writes go to `<name>.<pid>.tmp` (so two scans never share a temp file), `sync_all`, then `rename`, which replaces the target on Windows. The temp file is removed on any failure. Before each write, `sweep_stale_temps` removes sibling `<name>.*.tmp` files older than `STALE_TEMP_AGE` (10 minutes), which a killed scan leaves behind. It is age-based and best effort, and it skips this process's own temp file.
- `records.rs` checks the namespace byte before calling `NtfsFileName::namespace`, which unwraps. Keep the check first.
- `ntfs` is pinned exactly (`=0.4.0`) because it has no documented whole-MFT iterator. Upgrade on purpose.
- `dev` is the low 32 bits of the volume serial, the same value the walker reports as `FileMeta.dev`. Both paths must agree.
- Cancel is polled every 4096 (`CANCEL_CHECK_INTERVAL`) records and items, directories dequeued in `reach_directories`, records decoded in the cache read, and records written in the cache write. It is also checked before the cache file is read, at the start of every USN read round, after `VolumeTree::from_facts` in the refresh, and every 4096 records in `apply_delta`. A cancel in cache read, cache write or `reach_directories` ends as `MftError::Cancelled` (`CacheError::Cancelled`, `EmitError::Cancelled`) and stops the scan with no fallback.
- `mft_cache::read(path, max_bytes, cancelled)` checks the file length first and returns `CacheError::TooLarge` over `max_bytes`. That follows the normal cache-discarded notice path.

## Gotchas

- The `$MFT` path fails closed: a record that fails twice, a bad root, or an `$ATTRIBUTE_LIST` on record 0 falls back to the walker, and a partial tree is never cached. Only name-surrogate reparse tags count as links (cloud folders are walked). An unreadable `$REPARSE_POINT` tag fails the record, and a cached record with the reparse bit but no tag is rejected. `SectorReader` pins the chunk holding record 0 (`Ntfs::file` re-reads it per record). Throughput is unmeasured.
- The NTFS fast path runs only when the process is elevated and the scan root is on NTFS (`AutoEnumerator`). Otherwise, or when the fast path fails partway, it falls back to the walker and the CLI prints a `note:` on stderr. `--no-mft` forces the walker and prints no note. Items the fast path already yielded are yielded again by the walker, which is safe because enumeration is an idempotent upsert.
- Win32 code is compile-checked and linted from a Linux or macOS host only (mingw cross-check). No test executes it. Before trusting it, run the `mft_vs_walk` self-check on a Windows machine. Still unverified there: the raw `$MFT` read, how the `ntfs` crate handles unused or corrupt records, the reparse tags that mark cloud placeholders, and the admin requirement.
- The `$MFT` cache is at format version 2, so older caches are rejected and trigger a full scan.
- The cache is off unless `CacheConfig.dir` is set. The CLI sets it from `DEDUPE_CACHE_DIR`, else `%LOCALAPPDATA%\dedupe\mft`. `--no-mft-cache` turns it off, and `--rebuild-mft-cache` ignores the existing file but still writes a new one. The file is `{serial:016x}.cache` in that directory.
- Cache problems never fail a scan. Each one becomes a notice in `EnumNotices` (cache off, discarded, written, or a write error). The CLI drains the notices.
- `ntfs` 0.4.0 behavior that `records.rs` depends on: `Ntfs::file` fails with `InvalidFileSignature` for a never-written record and with `InvalidFileRecordNumber` past the end of `$MFT`. Its `file_attributes` drops the cloud RECALL bits, so the raw attribute word is read instead.
- The `$MFT` path yields paths in the caller's spelling and never canonicalizes them to `\\?\`. The pipeline matches folders by prefix.
- `HandleReader` opens by id only on NTFS volumes. The hint handle is opened only when `GetVolumeInformationByHandleW` reports NTFS.

## Commands

Crate-scoped only. Workspace commands live in the root CLAUDE.md.

```bash
# Lint the Windows target from macOS or Linux (compile and lint only, runs nothing)
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo clippy -p dedupe-backend-windows --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings

# Portable unit tests, every OS (run only when the user asks)
cargo test -p dedupe-backend-windows --lib

# Windows-only integration tests, on a Windows machine
cargo test -p dedupe-backend-windows --test handle_reader
cargo test -p dedupe-backend-windows --test mft_vs_walk -- --ignored
```

## Tests

- Unit tests live in `#[cfg(test)]` modules in `caps`, `filetime`, `handle`, `mft_cache`, `mft_walk`, `readahead`, `records`, `sector`, `tree`, `usn`, and `volume_io`. They run on every OS. `usn` and `volume_io` also have tests under `cfg(all(test, windows))`.
- `tests/handle_reader.rs` is `#![cfg(windows)]` and not ignored. It has 8 tests that open real files in the temp directory. The file-id tests need that directory on NTFS, which is the default for a Windows profile.
- `tests/mft_vs_walk.rs` is `#![cfg(windows)]` with one `#[ignore]` test. It compares the `$MFT` tree with the walker's tree. Run it from an elevated PowerShell, with the scratch directory on NTFS. A size or mtime mismatch on very recently written files is a known risk to investigate, not proof of a bug.
