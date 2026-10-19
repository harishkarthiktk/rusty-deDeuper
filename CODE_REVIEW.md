# deDupe code review

Review date: 2026-10-19. Scope: the whole workspace (about 22k lines of Rust in crates/core, crates/backend-generic, crates/backend-windows, crates/cli, bench, xtask, tui and the workspace config).

Method: 7 parallel read-only reviews, one per slice of the codebase. Then the duplicates were merged, the findings regrouped by priority, and 9 topic agents re-verified each finding and wrote the fixes. Nothing was built or run. Every finding cites file:line in the code as it stood at review time; the Windows code paths were checked by reasoning only.

How to read this file:

- Part 1 lists every finding grouped by priority (P0 to P3). Each finding names its fix topic (T1 to T9).
- Part 2 has the solution for each topic, with a re-verification verdict for each finding and boilerplate code.
- When two or three slices reported the same defect, they are merged into one entry under the primary ID, and the other IDs are listed as `Also reported as`. The entry takes the highest severity of the merged reports.
- A Confidence of Low means the claim depends on OS or runtime behaviour that could not be executed here. Part 2 re-checks those claims.

## Summary

There are 138 raw findings. After merging 13 duplicates, 125 unique findings remain: P0 2, P1 21, P2 44, P3 58.

| Topic | Name | P0 | P1 | P2 | P3 |
|---|---|---|---|---|---|
| T1 | [Report correctness and duplicate-group statistics](#t1-report-correctness-and-duplicate-group-statistics) | 1 | 1 | 0 | 2 |
| T2 | [Enumeration, id allocation, move detection, rescan rules](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules) | 1 | 2 | 2 | 3 |
| T3 | [Crash safety, resume, writer lifecycle, locking, cancellation](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation) | 0 | 3 | 5 | 3 |
| T4 | [File and volume identity across platforms](#t4-file-identity-and-volume-identity-across-platforms) | 0 | 5 | 4 | 9 |
| T5 | [Lookup command and lookup --insert](#t5-the-lookup-command-and-lookup---insert) | 0 | 2 | 4 | 1 |
| T6 | [Windows $MFT fast path, MFT cache, Win32 I/O hygiene](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene) | 0 | 5 | 7 | 12 |
| T7 | [CLI safety, path handling and output](#t7-cli-safety-path-handling-and-output) | 0 | 3 | 7 | 8 |
| T8 | [Performance and memory at scale](#t8-performance-and-memory-at-scale-1m-10m-files) | 0 | 0 | 9 | 3 |
| T9 | [Build config, error types, dead code, docs, test gaps](#t9-build-config-error-types-dead-code-docs-and-test-gaps) | 0 | 0 | 6 | 17 |
| | **Total** | **2** | **21** | **44** | **58** |

### P0 and P1 at a glance

- **P0 DB-01** (T1): Report pairs groups with members by hash, but list_groups is ordered by wasted bytes
- **P0 DB-02, PIPE-01** (T2): Folder ids resolved against the read-only snapshot create orphan folder chains on multi-root first scans
- **P1 CORE-01, DB-05, PIPE-07** (T1): Report hides live duplicates from roots not covered by the latest finished run, and group totals disagree with listed members
- **P1 DB-03, PIPE-02** (T2): Enumerated upsert with ON CONFLICT(id) lets a stale-snapshot lookup overwrite a moved file's row
- **P1 PIPE-03** (T2): Rescan "unchanged" check leaves out ctime, against plan P1.7.1
- **P1 DB-04** (T3): No inter-process lock: concurrent scans share id ranges and overwrite each other's rows
- **P1 PIPE-04** (T3): Hash passes keep the whole pass in memory and send it to the writer only at the end, so kill -9 or Ctrl-C loses the whole pass
- **P1 PIPE-05** (T3): `--force` hash-config wipe resets file states but not `run_volumes.phase_reached`, so `scan --resume --force` finishes with no results
- **P1 GEN-01, DB-08** (T4): file_id carries no device or volume component, so inode numbers collide across filesystems
- **P1 GEN-02** (T4): macOS volume UUID lookup can never succeed, so every macOS volume uses the unstable fsid fallback
- **P1 GEN-03** (T4): Windows identify fails outright on network shares, mapped drives and subst drives; serial fallback is unreachable
- **P1 GEN-04** (T4): Linux fsid fallback can be all-zero (FUSE and others), merging unrelated volumes under one uuid
- **P1 WIO-01** (T4): Open-by-id result is never checked against the path, so a colliding or reused id hashes another file's content
- **P1 CORE-02** (T5): `lookup --insert` on a file that is already indexed wipes its hashes and resets it to state 0
- **P1 CORE-03** (T5): Lookup reports the query file itself (and its hardlinks) as a duplicate
- **P1 MFT-01** (T6): Every record read re-reads $MFT record 0 and thrashes the single 1 MiB SectorReader chunk
- **P1 MFT-02** (T6): $MFT with an $ATTRIBUTE_LIST: records beyond the base-record runlist fail and up to 5% are silently dropped
- **P1 MFT-03** (T6): Per-record errors are tolerated silently up to 5% and the gaps are persisted into the cache
- **P1 MFT-04** (T6): Directories with any reparse tag (cloud/OneDrive) are never descended, unlike the walker
- **P1 MFT-05** (T6): Scan root that is a junction/symlink: root FRN and volume come from different places
- **P1 CLI-01** (T7): `std::env::args()` panics when any argument is not valid Unicode
- **P1 CLI-02** (T7): Filenames are printed raw to the terminal (escape-sequence injection)
- **P1 CLI-03** (T7): JSON paths are lossy and NFC-normalized, so they do not round-trip to real files

### Status audit of P0 and P1 (2026-10-19)

Method: a read-only Haiku subagent compared each P0 and P1 entry with the current code. A second pass has not re-checked its verdicts, so treat them as a first audit, not proof. Windows and macOS items rest on code reading only ("code only" means not executed on that OS). P2 and P3 findings were not audited.

Result: 2 of 2 P0 fixed. Of 21 P1: 18 fixed (7 of them code only), 3 partial (CORE-01, GEN-01 with its merged DB-08, MFT-01), 0 open.

| ID | Status | Evidence | Note |
|---|---|---|---|
| DB-01 | Fixed | report.rs:225-255, queries.rs:720 | One member query ordered by hash; stats recomputed from folded rows |
| DB-02 | Fixed | pass_enumerate.rs:309-335 | In-run folder overlay is checked before the DB |
| CORE-01 | Partial | report.rs:195, queries.rs:611-625 | Totals now match listed rows. Roots outside the latest run are still hidden by design (run scope). A killed rescan is only mitigated by a note |
| DB-03 | Fixed | pass_enumerate.rs:362-367, queries.rs:242-245 | Claim check before reuse; zero-row upsert raises IdConflict |
| PIPE-03 | Fixed | pass_enumerate.rs:427-432, pass_prefix.rs:233-238 | ctime is in the unchanged predicate |
| DB-04 | Fixed | db/lock.rs:27-43, queries.rs:755-760 | Lock sidecar plus `finish_run` status guard. See new defect 3 |
| PIPE-04 | Fixed | pass_prefix.rs:76-96, pass_full.rs:55-103 | Results stream in RESULT_CHUNK batches |
| PIPE-05 | Fixed | resume.rs:36-58 | Forced wipe resets `phase_reached` |
| GEN-01, DB-08 | Partial | identity.rs:8-20, pass_enumerate.rs:277-300 | Unix and `--cross-mounts` fixed. Windows `scan --no-db` still keys on bare file index (T4.2 open) |
| GEN-02 | Fixed (code only) | volume.rs:282-316 | Not run on macOS |
| GEN-03 | Fixed (code only) | volume.rs:354-398 | Serial then none fallback. Not run on Windows |
| GEN-04 | Fixed | volume.rs:136-153 | Zero fsid yields `none:fstype:source` |
| WIO-01 | Fixed (code only) | handle.rs:149-184, :276, :291 | NTFS-only hints plus post-open index check |
| CORE-02 | Fixed | lookup.rs:462-560 | Existing rows updated by id; no blind upsert |
| CORE-03 | Fixed, with caveat | lookup.rs:200, :336-344 | Filter is off if key resolution fails. See new defect 2 |
| MFT-01 | Partial | mft_walk.rs:101, sector.rs:57-59 | Chunk pinned, so I/O thrash is gone. `ntfs.file` still re-parses record 0 per record (records.rs:129) |
| MFT-02 | Fixed (code only) | records.rs:98-112, mft_walk.rs:108 | `$ATTRIBUTE_LIST` on `$MFT` falls back to the walker |
| MFT-03 | Fixed (code only) | mft_walk.rs:215-288, enumerator.rs:250-253 | Retry once, then abort to walker; partial tree never cached |
| MFT-04 | Fixed (code only) | tree.rs:390-402 | Only name-surrogate tags are links. See new defect 1 |
| MFT-05 | Fixed (code only) | enumerator.rs:208-218, volume_io.rs:289-300 | Root and volume come from one handle; serial compared |
| CLI-01 | Fixed | scan.rs:52-62 | `args_os`, Debug-escaped |
| CLI-02 | Fixed | path.rs:42-48, :144-170 | Controls and bidi overrides escaped on human output |
| CLI-03 | Fixed | path.rs:50-66 | Exact UTF-8, no NFC; `path_bytes` hex for invalid bytes |

New defects found by the audit (unconfirmed):

1. tree.rs:395-398 with records.rs:257-272: a directory with the reparse bit but no readable tag becomes a Symlink and is not descended, with no notice. A transient attribute read failure drops the subtree, and a rescan deletes its rows. Same class as MFT-04.
2. lookup.rs:156-161 and :336-338: if read-only query-key resolution fails, the self and hardlink filter is disabled, so the CORE-03 false match can return.
3. db/lock.rs:39-41: on `Unsupported` the lock falls back to running unlocked with only a warning, so DB-04 protection disappears on filesystems without lock support.

---

# Part 1. Findings by priority

## P0 - Critical

_Wrong duplicate results or index corruption on common paths. Fix before any further feature work._

### DB-01. Report pairs groups with members by hash, but list_groups is ordered by wasted bytes

- Topic: T1 (Report correctness and duplicate-group statistics); fix in Part 2, section [T1](#t1-report-correctness-and-duplicate-group-statistics)
- Reviewed slice: core db/index
- Severity: Critical
- Category: correctness
- Confidence: High
- Location: db/queries.rs:615-619 (list_groups ORDER BY), db/queries.rs:692-697 (volume_group_members ORDER BY), report.rs:210-235 (merge loop)
- Problem: `list_groups` orders by `wasted_logical DESC, full_hash`. `volume_group_members` orders by `g.full_hash, f.id`. `for_each_group` walks both with a single forward peekable merge and the comment "Both lists are ordered by full_hash, so one forward pass pairs them" is false. For each group it skips member rows with `r.0 < g.full_hash`, which discards members of groups that come later in wasted order but earlier in hash order.
- Impact: Volume with groups G1 (hash 0x90.., wasted 10 MB) and G2 (hash 0x10.., wasted 1 MB). Iteration takes G1 first, skips all rows below 0x90 (G2's members are dropped), emits G1. G2 then finds no rows, `members.len() < 2`, and is silently omitted. Most groups disappear from `report` whenever wasted order differs from hash order, while the header (from `group_totals`) still counts them, so header and body disagree.
- Suggested direction: Make the two orders agree (order members by `g.wasted_logical DESC, g.full_hash, f.id`, or group members into a `HashMap<Hash, Vec<_>>` before iterating metas). Add a test with at least two groups whose wasted order is the reverse of hash order.

### DB-02. Folder ids resolved against the read-only snapshot create orphan folder chains on multi-root first scans

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core db/index
- Also reported as: PIPE-01 (core pipeline, Critical)
- Severity: Critical
- Category: data-integrity
- Confidence: High
- Location: pipeline/pass_enumerate.rs:65-76 and 182-200 (ensure_folder), db/ids.rs:30-36, db/writer.rs:211-226 (Folders upsert)
- Problem: `ensure_folder` looks up `(volume, parent, name)` on `ctx.read`, which only sees committed rows, and otherwise allocates a new id. Folder rows sit in `folders_out` (flushed only at 5,000 rows or end of volume) and then in the writer (committed at 50k rows, 5 s or flush). For the second root the chain loop does `let id = ensure_folder(...)`, `dir_ids.entry(acc).or_insert(id)`, `cur = id`, so `cur` becomes the freshly allocated duplicate id B even though `dir_ids` keeps the first id A. In the writer, B's insert conflicts on `UNIQUE(volume_id,parent_id,name)` and becomes an update of A, so row B never exists, but children are inserted with `parent_id = B` (folders.parent_id has no FK).
- Impact: First `scan /data/a /data/b` on a volume mounted at `/` (root a small enough that nothing commits before root b's chain is built). Folder "b" is stored with parent B (missing). `reconstruct_path` and `PathCache::walk` stop at the missing parent, so every file under /data/b is reported as `/b/...` by report and lookup. `root_folder_ids` for root b is the orphan id, so `DeleteStale` from that root matches nothing (the CTE anchor `SELECT id FROM folders WHERE id IN (B)` is empty). On the next rescan the correct chain resolves to a different folder id, files are moved by inode, and the orphan folder rows are never cleaned up; rows without `file_id` (Windows walker failures) stay behind as permanent stale rows that `lookup` still returns.
- Suggested direction: Use the `dir_ids` cache (or a per-run `HashMap<(parent, name), id>` of pending folders) before querying the DB, and assign `cur` from the cached id, not the newly allocated one. Same check belongs in the `WalkItem::Dir` branch.
- PIPE-01 adds:
  - Location: crates/core/src/pipeline/pass_enumerate.rs:65-76, :182-201; crates/core/src/db/writer.rs (Batch::Folders upsert `ON CONFLICT(volume_id,parent_id,name) DO UPDATE SET last_seen_run_id`); crates/core/src/db/queries.rs:461-491 (reconstruct_path)
  - Problem: the root chain does `let id = ensure_folder(ctx, v.volume_id, cur, ...); dir_ids.entry(acc.clone()).or_insert(id); cur = id;`. `ensure_folder` looks the folder up via `ctx.read`, which sees only committed rows. Folder rows from root 1 are still in `folders_out` (flushed only at 5000 rows) or uncommitted in the writer (commit at 50k rows or 5 s idle). So for root 2 the shared ancestor misses, gets a new id, and `cur` takes that new id, not the one already in `dir_ids`. At write time the duplicate `(volume, parent, name)` row is dropped by the upsert, but its child is inserted with `parent_id` pointing at the dropped id. `folders.parent_id` has no FK (schema v2.sql:26), so nothing fails.

## P1 - High

_Wrong behaviour in realistic edge cases, crash/panic paths, security-relevant output, resume/rescan bugs._

### CORE-01. Report hides live duplicates from roots not covered by the latest finished run, and group totals disagree with listed members

- Topic: T1 (Report correctness and duplicate-group statistics); fix in Part 2, section [T1](#t1-report-correctness-and-duplicate-group-statistics)
- Reviewed slice: core model/lookup/report
- Also reported as: DB-05 (core db/index, Medium), PIPE-07 (core pipeline, Medium)
- Severity: High
- Category: correctness
- Confidence: High
- Location: crates/core/src/report.rs:187-195, crates/core/src/report.rs:201-248, crates/core/src/db/queries.rs:590-605 (recompute_groups), crates/core/src/db/queries.rs:615-637 (list_groups), crates/core/src/db/queries.rs:686-712 (volume_group_members), crates/core/src/db/queries.rs:840-851 (latest_finished_run)
- Problem: `recompute_groups` builds `dup_groups` from every live row on the volume (`state IN (3,4,5)`, no run filter), so `member_count`, `distinct_inodes` and `wasted_logical` count all rows. The report then lists members only where `f.last_seen_run_id = <latest finished run>`. Rows under roots that the latest run did not scan keep their older `last_seen_run_id` (CLAUDE.md: "Rows under roots not scanned this time are left alone"), so the report drops them.
- Impact: (a) Scan roots A and B on one volume, then rescan only A (the exact setup of `rescan_root_scoping_leaves_other_roots_untouched`, phase1.rs:797). Every A/B duplicate pair disappears from `report`: the group has 1 member from run 2 and fails the `members.len() >= 2` guard. Every duplicate group wholly inside B also disappears. (b) A group with 2 copies in A and 1 in B prints `x2 (3 inode(s), wasted 2*size)`: the header and `wasted_logical` count a file that is not listed. (c) After a rescan is killed mid-enumeration, rows already re-upserted carry the unfinished run's id, so `report` silently loses them until the run is resumed. Confidence on (c) is Medium: it assumes the enumerate upsert writes `last_seen_run_id` before the run finishes, as `upsert_file` does.
- Suggested direction: Report every live row on the volume (or every row whose last run finished), not only rows stamped with one run id. At minimum, compute `distinct_inodes` and `wasted_logical` from the same row set the members come from.
- DB-05 adds:
  - Location: db/queries.rs:590-606 (recompute_groups), db/queries.rs:639-655 (group_totals), report.rs:226-235
  - Problem: `recompute_groups` counts every row in states 3/4/5 regardless of `last_seen_run_id`, including rows kept under roots not scanned this run (by design they are not deleted). `group_totals` and `volume_group_members` only show rows with `last_seen_run_id = run_id`, and the report shows a group when it has 2 such rows, using the stored `distinct_inodes` and `wasted_logical`.
- PIPE-07 adds:
  - Location: crates/core/src/pipeline/scan.rs:249-254; crates/core/src/db/queries.rs:590-612 (no `last_seen_run_id` filter) versus :640-712 (`group_totals`, `group_members`, `volume_group_members` filter `last_seen_run_id=run`)
  - Problem: `dup_groups.member_count`, `distinct_inodes` and `wasted_logical` count state 3/4/5 rows left from earlier runs (roots not scanned this run, and stale rows from PIPE-06). The report then lists only latest-run members of those groups.

### DB-03. Enumerated upsert with ON CONFLICT(id) lets a stale-snapshot lookup overwrite a moved file's row

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core db/index
- Also reported as: PIPE-02 (core pipeline, High)
- Severity: High
- Category: data-integrity
- Confidence: High
- Location: db/writer.rs:228-247, pipeline/pass_enumerate.rs:216-233 (path lookup ignores seen_ids)
- Problem: Step 1 of `build_entry` (`find_by_path` on `ctx.read`) does not check `seen_ids`, so it can return a row id that move detection already claimed for another path in the same run. Both entries carry the same id. The writer's second `ON CONFLICT(id) DO UPDATE SET folder_id=..., name=...` then rewrites that row to the other path.
- Impact: Rename-and-recreate (log rotation: `app.log` -> `app.log.1`, new `app.log`). If the walker yields `app.log.1` first, move detection reuses row R (old app.log). Then `app.log` is found by path in the committed snapshot as R too. Writer applies R -> app.log.1, then R -> app.log with the new file's metadata. The moved file `app.log.1` has no row at all and is never hashed or reported. The same double-ON-CONFLICT path can also raise a UNIQUE(volume_id,folder_id,name) error inside the id-conflict update, which kills the writer and the whole scan.
- Suggested direction: In step 1, if `existing.id` is already in `seen_ids`, treat the path as new (allocate an id). Consider dropping `ON CONFLICT(id) DO UPDATE` and failing loudly on id collisions instead of silently rewriting another row.
- PIPE-02 adds:
  - Location: crates/core/src/pipeline/pass_enumerate.rs:216-235 (step 1), :237-258 (step 2); crates/core/src/db/writer.rs Batch::Enumerated (double ON CONFLICT upsert)
  - Problem: step 2 claims an old row (id X) for a new path and records X in `seen_ids`. Step 1 never checks `seen_ids`: when the old path is enumerated later and the claim is not committed yet (`find_by_path` reads `ctx.read`), it returns the same row X for the old path. Two entries now carry id X. The upsert moves row X to the new path, then the `ON CONFLICT(id)` clause moves it back to the old path.

### PIPE-03. Rescan "unchanged" check leaves out ctime, against plan P1.7.1

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core pipeline
- Related: GEN-11
- Severity: High
- Category: correctness
- Confidence: High
- Location: crates/core/src/pipeline/pass_enumerate.rs:219-222; .overall_plan.md:1336
- Problem: `unchanged = skip.is_none() && existing.size == meta.size && existing.mtime_ns == meta.mtime_ns && existing.file_id == meta.file_id;`. The plan and CLAUDE.md both say `(size, mtime_ns, ctime_ns, file_id)`. Move detection (step 2) also uses only size and mtime, which the plan allows.
- Impact: a file rewritten in place with the same size and its mtime restored (`touch -r`, `cp -p` over the file, archive extraction, some sync tools) keeps its old prefix and full hashes. `pass_prefix`/`pass_full` then reuse them without reading the file (pass_prefix.rs:496-506, pass_full.rs:354-360), so the report can list a false duplicate or miss a real one. On Unix, ctime would have caught this.
- Suggested direction: add the ctime comparison with the same "both Some" rule as `meta_matches` (pass_prefix.rs:570-576).

### DB-04. No inter-process lock: concurrent scans share id ranges and overwrite each other's rows

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core db/index
- Related: CORE-05
- Severity: High
- Category: concurrency
- Confidence: High
- Location: db/ids.rs:16-29, db/writer.rs:240-246, pipeline/scan.rs:61-65 and 75-77, db/queries.rs:730-736 (finish_run)
- Problem: `IdAllocator::seed` reads `MAX(id)+1` once per process. Nothing (no lock file, no `BEGIN IMMEDIATE` guard, no `locking_mode=EXCLUSIVE`) stops a second `scan` from opening the same DB. The second process marks the first run `abandoned` but cannot stop it, and both allocate from the same id range. `finish_run` does not check `status`, so the abandoned run later flips itself back to `finished`.
- Impact: Two terminals scanning different roots on one DB: process B's `Enumerated` batch with id 1001 hits `ON CONFLICT(id) DO UPDATE` and moves process A's file 1001 to B's path. Folder id collisions have no `ON CONFLICT(id)` and abort the writer with a UNIQUE error. `lookup` (lookup.rs:269) also seeds a private allocator, so a lookup run during a scan gets an id the scan writer later overwrites.
- Suggested direction: Take an exclusive advisory lock (lock file next to the DB, or a `meta` row claimed inside `BEGIN IMMEDIATE`) for the duration of a scan; make `finish_run` conditional on `status='running'`.

### PIPE-04. Hash passes keep the whole pass in memory and send it to the writer only at the end, so kill -9 or Ctrl-C loses the whole pass

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core pipeline
- Severity: High
- Category: data-integrity
- Confidence: High
- Location: crates/core/src/pipeline/pass_prefix.rs:375-460; pass_full.rs:83-103, :170-175; pass_verify.rs:62-111; .overall_plan.md:518
- Problem: `small`, `large`, `errors` (prefix), `hashed`/`errors` (full) and `verified` are plain Vecs sent once after every row is processed. The pooled path also turns a cancel into `Err(Cancelled)` (pass_full.rs:174) and drops all finished results. Plan line 518 says "Kill -9 and restart must lose at most one batch window."
- Impact: on a multi-TB volume, a crash or Ctrl-C at 99% of `pass_full` throws away every full hash. Resume re-reads all the data. The same applies to `pass_prefix` over 1M files. Memory also grows with row count (see PIPE-10).
- Suggested direction: send `FullHashed`/`PrefixHashed`/`Error` in bounded chunks (for example 5k rows) as results come in. State transitions are idempotent per id, so partial commits are safe. Classification of unique prefixes can stay at the end of the pass.

### PIPE-05. `--force` hash-config wipe resets file states but not `run_volumes.phase_reached`, so `scan --resume --force` finishes with no results

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core pipeline
- Severity: High
- Category: correctness
- Confidence: High
- Location: crates/core/src/pipeline/resume.rs:299-306; crates/core/src/pipeline/scan.rs:122, :127-129, :182-237
- Problem: the wipe runs `UPDATE files SET prefix_hash=NULL, full_hash=NULL, prefix_bytes=NULL, state=0` for every row, but `plan_resume` still reads the old phases (resume.rs:353). With phase >= SizeDone, no pass consumes state 0.
- Impact: when the prefix config changed and the user runs `scan --resume --force` on a run that had reached SizeDone or later, `pass_full` finds no state-2 rows and `recompute_groups` finds no state 3/4/5 rows. The run is marked finished and the report is empty, with no error.
- Suggested direction: on a forced wipe, reset `phase_reached` to `None` for running runs (or refuse `--resume` together with a config wipe). Also clear `dup_groups`.

### GEN-01. file_id carries no device or volume component, so inode numbers collide across filesystems

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Also reported as: DB-08 (core db/index, High)
- Severity: High
- Category: correctness
- Confidence: High
- Location: crates/backend-generic/src/meta.rs:105 (Unix `md.ino()` only), crates/backend-generic/src/lib.rs:139-142 (Windows file index only, `dwVolumeSerialNumber` discarded), crates/backend-generic/src/walk.rs:24-27; consumers crates/core/src/pipeline/naive.rs:38-55,109, crates/core/src/db/queries.rs:308,596
- Problem: `FileMeta.file_id` is the bare inode (Unix) or 64-bit file index (Windows). `FileMeta` has no device field, and core treats `file_id` as the hardlink identity: `distinct_count` in the naive pipeline keys on `file_id` with `volume_id: 0` for every root, and the DB queries use `COUNT(DISTINCT COALESCE(file_id, id))` per volume. Files from two filesystems with the same inode number are counted as one inode.
- Impact: (a) `scan --no-db /mnt/a /mnt/b` on two ext4 or NTFS disks where `/mnt/b` is a fresh copy of `/mnt/a`: inode or file-index allocation on fresh filesystems is often sequential, so an original and its copy can share both inode number and size. `distinct_count` then returns 1 and the real duplicate is dropped at the size pass (false negative). (b) `scan --cross-mounts /` on any OS (and macOS firmlinked `/Users`, which sits on the Data volume): files from nested mounts are attributed to the root's volume id, so the same collision suppresses groups. Move detection (`find_move_candidate` by `(volume_id, file_id)` plus size and mtime) can also adopt the wrong row and its stored hash.
- Suggested direction: fold the device (`st_dev`, or the Windows volume serial from `BY_HANDLE_FILE_INFORMATION`) into the `file_id` bytes, or add a `dev` field to `FileMeta` and key on it in core. Coordinate with backend-windows so the `$MFT` path uses the same encoding.
- DB-08 adds:
  - Location: db/queries.rs:303-315 (size_candidates), db/queries.rs:590-603 (recompute_groups), crates/backend-generic/src/meta.rs:50, crates/cli/src/scan.rs:123
  - Problem: Hard-link de-duplication uses `COUNT(DISTINCT COALESCE(file_id, id))` scoped by `volume_id`. On Unix `file_id` is `ino` only (no `st_dev`). With `--cross-mounts` (`one_file_system=false`), files from other mounted filesystems are stored under the root's `volume_id`, and inode numbers from different devices collide routinely (btrfs subvolumes all start at 256/257).

### GEN-02. macOS volume UUID lookup can never succeed, so every macOS volume uses the unstable fsid fallback

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: High
- Category: cross-platform
- Confidence: Medium
- Location: crates/backend-generic/src/volume.rs:408-434, 385-396
- Problem: Two bugs in `get_volume_uuid`. (1) `volattr: libc::ATTR_VOL_UUID` without `ATTR_VOL_INFO` (0x80000000, present in libc 0.2.155). getattrlist(2) requires `ATTR_VOL_INFO` whenever any volume attribute is requested, otherwise it fails with EINVAL. (2) The comment says the reply starts with a 20-byte `attribute_set_t`, but that is only returned when `ATTR_CMN_RETURNED_ATTRS` is requested (commonattr is 0 here). The reply is `u32 length` followed by the 16-byte `uuid_t`, so the UUID is at `buf[4..20]`, not `buf[20..36]`. Even if the call succeeded, `buf[20..36]` is zero, and the "all zeros" guard at line 386 sends it to the fallback. `buf.len() < 36` at line 428 is always false (dead check).
- Impact: On every macOS scan `uuid_kind` is `FsidFallback` and a warning is logged. `f_fsid.val[0]` on macOS is the `dev_t` of the device node, which depends on attach order. Unplugging and replugging an external disk, or rebooting with different disks attached, gives a new identity: the volume is indexed again from scratch and `lookup` misses it. Two different disks that get the same `/dev/diskN` at different times can share an identity.
- Suggested direction: request `ATTR_VOL_INFO | ATTR_VOL_UUID`, parse `length` and read the uuid at offset 4, and call it on the real volume root (`statfs.f_mntonname`, see GEN-14). To confirm: print `rc`, `errno` and the first 36 bytes of `buf` on a Mac.

### GEN-03. Windows identify fails outright on network shares, mapped drives and subst drives; serial fallback is unreachable

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: High
- Category: cross-platform
- Confidence: Medium
- Location: crates/backend-generic/src/volume.rs:489-514; called with `?` from crates/core/src/pipeline/scan.rs:47 and crates/core/src/lookup.rs:259
- Problem: `GetVolumeNameForVolumeMountPointW` only resolves local volume mount points. For `\\server\share\`, a mapped drive letter or a `subst` drive it returns 0, and `identify` returns `Err`. The `VolumeSerial` branch only runs when the call succeeds with an empty GUID, which does not happen in practice, so the fallback the comment describes (plan 4.10) never runs.
- Impact: `dedupe scan Z:\photos` (mapped SMB drive) or `scan \\nas\share` aborts with an IO error before enumeration. `lookup` on such a path also errors. The storage probe handles this case (returns None), so only identity blocks it.
- Suggested direction: on failure, fall back to `GetVolumeInformationW` serial (`VolumeSerial`), and to `UuidKind::None` with the UNC root when that also fails. To confirm: run `identify` against a mapped drive on Windows.

### GEN-04. Linux fsid fallback can be all-zero (FUSE and others), merging unrelated volumes under one uuid

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: High
- Category: data-integrity
- Confidence: Medium
- Location: crates/backend-generic/src/volume.rs:330-341
- Problem: When `/dev/disk/by-uuid` has no match (btrfs, FUSE such as sshfs or rclone, NFS, CIFS, overlay, tmpfs), the id is `fsid:{val0}{val1}` from `statfs`. Several Linux filesystems do not fill `f_fsid`. FUSE's statfs conversion does not set it, so it is 0. Every such mount then maps to `fsid:0000000000000000`, and core upserts volumes by uuid. The Linux implementation never returns `UuidKind::None`, although the module doc says it should synthesize `none:<fs_type>:<mount>`.
- Impact: Scanning two sshfs mounts, or one now and another later, puts both under one `volumes` row with the first mount path. In scan.rs:45-57 both roots end up in one `VolumeScan`, and `relative_components` builds the second root's folder chain against the wrong mount. Inode numbers from the two mounts also collide (see GEN-01), which can reuse a stored hash for a different file.
- Suggested direction: treat an all-zero fsid as unknown and return `UuidKind::None` keyed on the fs type and mount path (and `st_dev`). To confirm: `stat -f -c %i` on a FUSE mount.

### WIO-01. Open-by-id result is never checked against the path, so a colliding or reused id hashes another file's content

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-windows Win32 I/O
- Severity: High
- Category: data-integrity
- Confidence: Low
- Location: crates/backend-windows/src/handle.rs:224-245 (open_handle_for), 206-221 (hint_for), 146-175 (open_by_id); crates/core/src/pipeline/pass_prefix.rs:256 (`unchanged` compares size, mtime, ctime by path only); crates/core/src/pipeline/pass_verify.rs:258-270
- Problem: any 8-byte `file_id` is passed to `OpenFileById(FileIdType)` on whatever volume `GetVolumePathNameW(path)` names, with no check of the file system type and no check that the opened handle is the file at `path`. On NTFS the 64-bit reference includes the sequence number, so a stale id fails and falls back to the path, which is correct. But walker-sourced ids on ReFS are a truncated 64-bit view of a 128-bit id (documented in CLAUDE.md as "may collide" for identity only), and nothing stops them reaching `OpenFileById`. The post-hash guard `unchanged()` re-stats by path, so it cannot see that the content came from a different file. Byte verify (`pass_verify`) opens by the same ids, so it reads the same wrong file and cannot catch it either.
- Impact: if `OpenFileById` on ReFS (or another file system that accepts `FileIdType`) resolves a truncated id to a different file, that file's content is hashed and recorded under the row's path. The result is wrong duplicate groups, and verify confirms them. The CLAUDE.md gotcha covers only re-hashing and identity, not reading the wrong content. To confirm: on a ReFS volume, compare `GetFileInformationByHandle` nFileIndex with `FileIdInfo` for files that share the low 64 bits, then call `OpenFileById` with the 64-bit value.
- Suggested direction: allow open-by-id only when the hint volume reports `NTFS` (cache the fs name with the hint), or, after opening, compare `GetFileInformationByHandle(handle)` size and index with the expected row and fall back to the path on a mismatch.

### CORE-02. `lookup --insert` on a file that is already indexed wipes its hashes and resets it to state 0

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core model/lookup/report
- Severity: High
- Category: data-integrity
- Confidence: High
- Location: crates/core/src/lookup.rs:269-284, crates/core/src/db/queries.rs:199-231
- Problem: `persist` builds `FileEntry::enumerated(...)` (prefix_hash/full_hash `None`, state `Enumerated`) and calls `queries::upsert_file`. That function's `ON CONFLICT(volume_id,folder_id,name) DO UPDATE` overwrites `prefix_hash`, `full_hash`, `state` and `last_seen_run_id` with the excluded values. Nothing checks whether the row already exists.
- Impact: `dedupe lookup --insert /indexed/a.txt` matches itself (see CORE-03), so `persist` runs. The row for a.txt then gets `full_hash=NULL, state=0`. `report` (states 3,4,5 only) drops a.txt from its group, and the partner copy can drop out with it under the 2-member guard. The data comes back only after a full rescan re-hashes the file. The query's own `q_prefix`/`q_full` are already computed but never stored, even for a genuinely new row.
- Suggested direction: When `find_by_path` returns an existing row, leave it alone or update only its metadata. For a new row, store the query's computed hashes with an appropriate state.

### CORE-03. Lookup reports the query file itself (and its hardlinks) as a duplicate

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core model/lookup/report
- Related: PIPE-11
- Severity: High
- Category: correctness
- Confidence: High
- Location: crates/core/src/lookup.rs:85-182, crates/core/src/lookup.rs:206-229, crates/cli/src/lookup.rs:38-52
- Problem: Candidates are every row with the same size (`size_candidates`). Nothing excludes the row that is the query path, or rows with the same `(volume, file_id)` as the query's `meta.file_id`. The CLI does not filter them either.
- Impact: `dedupe lookup <any already-scanned file>` returns `1 match(es) [fresh] <the same path>`, a false duplicate. A hardlink of an indexed file is likewise reported as a duplicate, while the report itself collapses hardlinks (`COUNT(DISTINCT COALESCE(file_id,id))`). Scripts that treat a match as "safe to delete" would delete the only copy. Combined with `--insert`, it triggers CORE-02.
- Suggested direction: Drop candidates whose reconstructed path equals the canonical query path, or whose volume and file_id equal the query's. Optionally report them as "self" or "hardlink" rather than as matches.

### MFT-01. Every record read re-reads $MFT record 0 and thrashes the single 1 MiB SectorReader chunk

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: High
- Category: performance
- Confidence: High (mechanism), Medium (wall-clock magnitude)
- Location: crates/backend-windows/src/records.rs:108; crates/backend-windows/src/mft_walk.rs:23,90-95,115-116; crates/backend-windows/src/sector.rs:14-24,48-85; ntfs-0.4.0/src/ntfs.rs:85-110
- Problem: `extract_record` calls `ntfs.file(fs, record)`. In ntfs 0.4.0 that function first does `NtfsFile::new(self, fs, self.mft_position..., 0)` (reads, fixes up and parses $MFT record 0), decodes its $DATA runlist, then reads the target record. `SectorReader` keeps exactly one cached chunk (`cache`, `cache_start`, `cache_end`) of `READ_CHUNK_BYTES = 1 << 20`. Once the walk passes the first ~1 MiB of $MFT, every record alternates between the chunk holding record 0 and the chunk holding record N, so each record costs two 1 MiB `fill` calls plus a full parse of record 0. Attribute-list files call `ntfs.file` again per extension record.
- Impact: 10M-record volume -> about 20M 1 MiB reads (about 20 TB copied through the volume handle; buffered handle at volume_io.rs:181 so mostly cache-manager memcpy, but still syscalls and copies per record). The "fast path" may be slower than the walker it replaces. No test measures throughput.
- Suggested direction: Resolve the $MFT runlist once (read record 0 yourself, decode data runs into a `Vec<(vcn, lcn, len)>`), then map record N to a disk offset and read it with `NtfsFile`-free parsing, or at least give SectorReader a second pinned slot for the record-0 chunk. Read $MFT sequentially in large runs rather than per-record random calls.

### MFT-02. $MFT with an $ATTRIBUTE_LIST: records beyond the base-record runlist fail and up to 5% are silently dropped

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: High
- Category: correctness
- Confidence: Medium
- Location: ntfs-0.4.0/src/ntfs.rs:93-107 (comment: "this code assumes that the MFT does not have an Attribute List!"); crates/backend-windows/src/records.rs:108-112; crates/backend-windows/src/mft_walk.rs:257-270,307-309
- Problem: `Ntfs::file` locates records using only the $DATA attribute found in $MFT's base record. When $MFT is fragmented enough that its runlist spills into extension records (an $ATTRIBUTE_LIST on record 0), seeking past the last run in the base record leaves `data_position()` empty and the crate returns `InvalidFileRecordNumber`. `extract_record` maps that to `RecordError::Ntfs`, which `read_tree` counts and skips. The walk only aborts when errors exceed `max(record_count/20, 1000)`.
- Impact: Large, long-lived volume with a heavily fragmented $MFT whose tail (under 5% of slots) lies in runs listed in an extension record -> every file in those records, and every descendant of any directory among them, is missing from the scan with no notice. Previously indexed rows under the scan root are then deleted as stale by the rescan logic.
- Suggested direction: Detect an attribute list on record 0 at `NtfsRecordSource::open` and either fall back to the walker or assemble the full runlist yourself. Confirm with a volume where `fsutil file layout C:\$MFT` shows an $ATTRIBUTE_LIST.

### MFT-03. Per-record errors are tolerated silently up to 5% and the gaps are persisted into the cache

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: High
- Category: data-integrity
- Confidence: High
- Location: crates/backend-windows/src/mft_walk.rs:257-270,303-309; crates/backend-windows/src/enumerator.rs:161-167,238-242,404-409; crates/backend-windows/src/mft_cache.rs:576-597
- Problem: Any `RecordError` (I/O, fixup mismatch from a torn read of a live record, missing $STANDARD_INFORMATION, one bad $FILE_NAME via `?` at records.rs:259-265) skips the whole record. The only signal is `tracing::debug!` with `skipped = stats.records_skipped_errors`; no `EnumNotices` entry. A skipped directory silently drops its whole subtree (tree.rs:233-238 skips names with no reached parent). The tree is then written to the cache (`store_cache`), and `apply_delta` only re-reads records named by journal entries, so a record that failed once stays missing in every later cached scan until that file changes or the journal resets.
- Impact: 10M-record volume -> up to 500,000 record failures are accepted as a "successful" MFT walk. A transient read error on one directory record hides its subtree from report/lookup, and core's rescan deletes those previously indexed rows as stale under the scan root. With the cache enabled the loss persists across scans.
- Suggested direction: Surface `records_skipped_errors > 0` as a notice; do not write the cache (or record the failed record numbers in it and re-read them next time) when any record failed; consider retrying a failed record once (torn live reads) and treating a failed directory record as fatal for the subtree it covers.

### MFT-04. Directories with any reparse tag (cloud/OneDrive) are never descended, unlike the walker

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: High
- Category: correctness
- Confidence: Medium
- Location: crates/backend-windows/src/tree.rs:368-380, 193, 208-210; crates/backend-generic/src/walk.rs:24,37
- Problem: `kind_of` returns `Symlink` for `(is_dir=true, has_reparse_point=true, _)` regardless of tag. The fallback walker uses walkdir with `follow_links(false)` and std `FileType`; on Windows std's `is_symlink()` is true only for name-surrogate tags (bit 0x2000_0000, e.g. SYMLINK, MOUNT_POINT, LX_SYMLINK), so a directory with IO_REPARSE_TAG_CLOUD_* (0x9000_x01A, not a name surrogate) is a plain directory to the walker and is descended. Conversely a file with IO_REPARSE_TAG_LX_SYMLINK (0xA000_001D, name surrogate) is a symlink to the walker but `RegularFile` here (tree.rs:377-378 only match SYMLINK and MOUNT_POINT).
- Impact: Scanning C:\Users\me (elevated) where OneDrive Files On-Demand folders carry cloud reparse tags -> the whole OneDrive tree is emitted as one 0-byte symlink item via the MFT path, but fully enumerated via `--no-mft`. If the scan root itself is such a folder, `emit_items` returns Ok with zero items (tree.rs:208-210) and the scan "succeeds" empty. Confirm with `fsutil reparsepoint query <OneDrive folder>`.
- Suggested direction: Classify by the name-surrogate bit (tag & 0x2000_0000) for both files and directories to match std/walkdir, and treat a non-directory root kind as a fallback reason rather than an empty success.

### MFT-05. Scan root that is a junction/symlink: root FRN and volume come from different places

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: High
- Category: correctness
- Confidence: Medium
- Location: crates/backend-windows/src/enumerator.rs:208-215; crates/backend-windows/src/volume_io.rs:221-229; crates/backend-generic/src/lib.rs:156-167
- Problem: `frn_of` opens the root with `FILE_FLAG_BACKUP_SEMANTICS` only, no `FILE_FLAG_OPEN_REPARSE_POINT`, so it follows a junction/symlink and returns the target's file index. The volume is chosen by `GetVolumePathNameW` on the unresolved path string, which (to my understanding) only honours volume mount points, not junctions. Nothing compares `dwVolumeSerialNumber` from `GetFileInformationByHandle` with the opened raw volume.
- Impact: `dedupe scan C:\link` where `C:\link` is a junction to `D:\data` -> record number of D:\data is looked up in C:'s $MFT, enumerating an unrelated C: directory (or nothing) under paths prefixed `C:\link`. Even a same-volume junction root becomes a Symlink root record if opened with OPEN_REPARSE_POINT, hitting MFT-04's empty-success path. Confirm with a junction root test on Windows.
- Suggested direction: Check that the root handle's volume serial equals the raw volume's serial (fallback otherwise), and decide explicitly whether a reparse root is followed; mirror walkdir's `follow_root_links` behaviour.

### CLI-01. `std::env::args()` panics when any argument is not valid Unicode

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: High
- Category: correctness
- Confidence: High
- Location: crates/cli/src/scan.rs:166
- Problem: `roots` are `PathBuf` (clap's `PathBufValueParser` accepts any `OsStr`), so a non-UTF-8 root parses. The DB path then builds `cli_args: std::env::args().collect::<Vec<_>>().join(" ")`. `std::env::args()` panics during iteration if any argument is not valid Unicode. The join also drops quoting, so `cli_args` cannot be replayed when a path contains spaces.
- Impact: on Linux, `dedupe scan $'/data/\xff-legacy'` (Latin-1 names from old archives or SMB shares) panics with exit 101 before the scan starts. This happens on the default persistent path. `--no-db` does not hit it.
- Suggested direction: use `std::env::args_os()` and `to_string_lossy()` each argument, or quote them with `{:?}`.

### CLI-02. Filenames are printed raw to the terminal (escape-sequence injection)

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: High
- Category: security
- Confidence: High
- Location: crates/core/src/report.rs:274 (via `report` human output and `scan` `emit`, crates/cli/src/scan.rs:288); crates/cli/src/lookup.rs:93; crates/cli/src/db.rs:53; crates/core/src/path.rs:36-38
- Problem: human output writes `m.path.to_display()` directly. `to_display` is `from_utf8_lossy` plus NFC. It does not escape C0 or C1 controls, ESC, CR or LF. Unix filenames can contain any byte except `/` and NUL.
- Impact: scanning an untrusted tree (Downloads, an extracted archive, a shared NAS) with a file named `a\x1b[2K\x1b[1A...` lets that file erase or rewrite earlier report lines, hide groups, or spoof paths. Some terminals also accept OSC 52 (clipboard write) or OSC 8 (hyperlinks). A `\n` in a name splits one member into two fake lines, which breaks line-based post-processing of the human output.
- Suggested direction: escape control characters in human output (for example with `char::escape_debug` for controls only, or quote the path) at a single display chokepoint. JSON is already safe because serde escapes controls.

### CLI-03. JSON paths are lossy and NFC-normalized, so they do not round-trip to real files

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: High
- Category: data-integrity
- Confidence: High
- Location: crates/core/src/report.rs:86; crates/cli/src/lookup.rs:75; crates/core/src/path.rs:36-38
- Problem: the `path` field in report and lookup JSON is `RawPath::to_display()`. It replaces invalid UTF-8 with U+FFFD and applies NFC normalization. JSON is the scripting interface (`--format auto` picks it whenever stdout is piped), but on Linux, and on any filesystem that does not normalize names, the NFC string is a different byte sequence from the name on disk.
- Impact: (a) A file whose name is NFD (common when files are copied from macOS to ext4 or a NAS) appears in JSON under a path that does not exist, so `jq -r '.groups[].members[].path' | xargs ...` fails or acts on a different file. (b) Two different non-UTF-8 names map to the same U+FFFD string, so a consumer cannot tell group members apart. The README says acting on the report is up to the user, and this output makes that unsafe.
- Suggested direction: keep `to_display` for human output. In JSON, add an exact field (for example `path_bytes` as base64 or hex, or a non-normalized `path` plus a `path_lossy: bool` flag).

## P2 - Medium

_Robustness, performance at scale, misleading errors, doc/config drift that will bite soon._

### GEN-05. Walk errors are silently discarded; unreadable subtrees vanish and their index rows are deleted as stale

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: backend-generic
- Severity: Medium
- Category: error-handling
- Confidence: High (silent drop), Medium (stale-row consequence)
- Location: crates/backend-generic/src/walk.rs:35 (`walker.filter_map(|e| e.ok())`), walk.rs:50 (`Err(_) => continue`); crates/core/src/traits.rs:14-17 (`WalkItem` has no error variant); crates/core/src/pipeline/pass_enumerate.rs:161
- Problem: EACCES on a directory, EIO, a dropped network share mid-walk, or an lstat race are all dropped with no log, counter or `errors` row. The trait has no way to report them.
- Impact: The user sees a clean scan that skipped an entire subtree. On rescan, a directory that is temporarily unreadable (permission change, flaky NAS) looks deleted. `delete_stale` drops its rows under the scan root, so the stored hashes are lost and the next run hashes everything again.
- Suggested direction: add `WalkItem::Error { path, err }` (or a separate callback), record it in `errors`, and exclude the failed directory's subtree from stale deletion.

### PIPE-15. Root-to-mount mapping falls back silently, and files without a registered parent are dropped with no log

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core pipeline
- Severity: Medium
- Category: error-handling
- Confidence: Low
- Location: crates/core/src/pipeline/pass_enumerate.rs:302-319 (`relative_components`), :91-94, :104-107; crates/core/src/pipeline/scan.rs:46
- Problem: if `root` does not byte-start with `mount`, `relative_components` treats the whole root as relative, drive or verbatim prefix included. It also does a prefix match without a component-boundary check, and it splits on `\` on Unix, where `\` is a legal filename byte. Separately, any Dir/File whose parent is not in `dir_ids` returns `Ok(())` with no trace or warn and no error row. Core also calls `std::fs::canonicalize` directly (scan.rs:46), which bypasses the `PathCodec`/`VolumeIdentity` layering described in CLAUDE.md.
- Impact: if mount and root ever differ in form (verbatim vs plain, drive-letter case on Windows, a root dir name containing `\` on Linux), the folder chain keys never match the walker's parent paths. Every file under that root is dropped and the scan reports 0 files with exit 0. Whether `GetVolumePathNameW` returns the same verbatim form as `canonicalize` on Windows is unverified, so a run on Windows of a scan whose root is below the drive root would confirm or rule this out.
- Suggested direction: normalize root and mount through the codec, error out when root is not under mount, and count or warn on files dropped for a missing parent.

### CLI-05. Global Ctrl-C handler makes `lookup` and `db vacuum` impossible to interrupt, and a second Ctrl-C does nothing

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: error-handling
- Confidence: High
- Location: crates/cli/src/main.rs:406-417; crates/cli/src/lookup.rs:14,38; crates/cli/src/db.rs:14-17
- Problem: `ctrlc::set_handler` replaces the default SIGINT and console-control action for every subcommand. `lookup` and `db` never receive the token, and `core::lookup::lookup` has no cancel parameter. The handler only sets a flag, with no second-press escalation such as `std::process::exit(130)` on a repeat.
- Impact: `dedupe lookup bigfile.iso` (which full-hashes a multi-GB file) or `dedupe db vacuum` on a large index ignores Ctrl-C until it finishes. During `scan`, any stretch that does not poll the token (writer drain, `dup_groups` recompute, a long SQLite statement) also ignores Ctrl-C, and pressing it again does not help. The user has to fall back to `kill -9` or Task Manager.
- Suggested direction: install the handler only for `scan` and `report`, or have it exit on the second press. Thread the token into `lookup`.

### DB-06. Phase marker commits before follow-up batches, so a crash skips them on resume

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core db/index
- Also reported as: PIPE-06 (core pipeline, Medium)
- Severity: Medium
- Category: data-integrity
- Confidence: High
- Location: pipeline/pass_enumerate.rs:143-161, pipeline/rescan.rs:15-20, pipeline/pass_prefix.rs:108-121, db/writer.rs:165-170
- Problem: `PhaseComplete{Enumerated}` is committed and flushed (pass_enumerate.rs:148) before `DeleteStale` is sent (line 161). In the over-budget prefix path, `PhaseComplete{PrefixDone}` is flushed before `UniquePrefix` is sent. Resume is driven only by `phase_reached`, so the trailing batch is never re-sent.
- Impact: `kill -9` between the two flushes: resume skips enumeration, stale rows (files deleted since the last scan) are never removed. `lookup` considers every row not in -1/-2, so it keeps reporting deleted files as matches; `recompute_groups` counts them (see DB-05). In the prefix case, singletons stay in state 2 and are full-hashed needlessly.
- Suggested direction: Send `DeleteStale` (and `UniquePrefix`) before the `PhaseComplete` so they share its transaction, or add a separate phase for stale deletion.
- PIPE-06 adds:
  - Location: crates/core/src/pipeline/pass_enumerate.rs:142-161; crates/core/src/pipeline/scan.rs:189-196; .overall_plan.md P1.4.2 ("Enumerated = pass_enumerate done (incl. rescan merge + stale deletion)")
  - Problem: `PhaseComplete(Enumerated)` is committed and flushed (line 148) before `rescan::delete_stale` sends `DeleteStale`. `delete_stale` is called only from `pass_enumerate::run`, and resume skips that pass when `phase >= Enumerated`.

### DB-07. Writer failures surface as a bare WriterDied and the writer is not joined on error paths

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core db/index
- Severity: Medium
- Category: error-handling
- Confidence: High
- Location: db/writer.rs:85-99 and 151-163, pipeline/scan.rs:95-158
- Problem: `send` maps any failure to `WriterDied`; the Flush branch acks `Err(CoreError::WriterDied)` instead of the real error; `shutdown` maps a panic payload to `WriterDied`. In `run_scan`, any `?` inside the volume loop (including `Cancelled` from Ctrl-C and `WriterDied` itself) returns before `writer.shutdown()`, so the `JoinHandle` is dropped, the thread detaches, and the real `CoreError` (SQLITE_FULL, constraint violation, I/O error) is only in a tracing log line.
- Impact: Disk full during a scan: the user gets "db writer thread died" with no cause unless log level shows errors. On Ctrl-C, batches already sent are left to a detached thread that the process exit may kill mid-commit, so a graceful cancel loses the same work as a crash and skips the final WAL checkpoint.
- Suggested direction: On any error in the loop, call `writer.shutdown()` and prefer its error over `WriterDied`; ack Flush with the real error (CoreError may need to be cloned or stringified).

### PIPE-08. Cancellation latency is bounded by one whole-file read; the naive walk ignores cancel

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core pipeline
- Severity: Medium
- Category: concurrency
- Confidence: High
- Location: crates/core/src/hash.rs:144-153 (hash_full_target loop); pass_verify.rs:309-320 (verify_pair loop); pass_full.rs:279 (checked only between jobs); naive.rs:432-470 (walk closure has no `cancel.check()`)
- Problem: the token is checked between files, never inside the per-chunk read loops. In `scan_naive` it is checked once per root, not per walked item (`pass_enumerate.rs:80` does check per item).
- Impact: Ctrl-C while hashing or byte-verifying a 100 GB file (or a slow network file) waits for the whole read. Ctrl-C during a `--no-db` walk of a large tree waits until the walk finishes.
- Suggested direction: pass `&Token` into `hash_full_target`/`verify_pair` and check it every N chunks. Add `cancel.check()?` at the top of the naive walk closure.

### PIPE-09. A panicking hash worker crashes the scan only after every remaining job has been hashed

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core pipeline
- Severity: Medium
- Category: concurrency
- Confidence: High
- Location: crates/core/src/pipeline/pass_full.rs:244-264, :269-291
- Problem: workers run inside `thread::scope`. A panic in a backend reader (for example the Windows overlapped code, which no test runs) is not caught. The other workers keep claiming jobs, the `rx` loop drains them, and then `thread::scope` re-panics on the calling thread. The panic is not turned into a per-file error, and `abort` is set only when a spawn fails.
- Impact: one bad file brings down the whole process after possibly hours of further hashing, and all of that pass's results are lost (PIPE-04). The writer handle is dropped without `shutdown`.
- Suggested direction: wrap `hash_job` in `catch_unwind` (map the panic to an `ErrRow` and set `abort`), or join the handles explicitly and turn a panic into `CoreError`.

### GEN-07. ContentReader::metadata on Windows omits file_id and nlink, unlike the walker

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Medium
- Category: correctness
- Confidence: High
- Location: crates/backend-generic/src/reader.rs:239-242; consumers crates/core/src/lookup.rs:69,277-281, crates/core/src/pipeline/pass_enumerate.rs:219-222
- Problem: `metadata()` calls `file_meta_from` but not `fill_file_identity`. On Windows it returns `file_id: None, nlink: 1`, while the walker returns `Some(index)` and the true link count for the same file. The trait describes `file_id` as stable identity, and the two backend paths disagree.
- Impact: Windows `lookup --insert` writes rows with `file_id NULL`. On the next rescan `existing.file_id == meta.file_id` (pass_enumerate.rs:222) is `None != Some`, so the row counts as changed and is hashed again. A hardlinked file added through lookup counts as a distinct inode in `COUNT(DISTINCT COALESCE(file_id, id))`, which inflates wasted-space figures.
- Suggested direction: call `fill_file_identity` in `metadata()` for regular files, or move it into `file_meta_from` so every path gets it.

### GEN-09. Windows identify mount vs canonicalized root may not share the `\\?\` prefix (unverified)

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Medium
- Category: cross-platform
- Confidence: Low
- Location: crates/backend-generic/src/volume.rs:474-522; caller crates/core/src/pipeline/scan.rs:46-47; crates/core/src/pipeline/pass_enumerate.rs:302-311; also lib.rs:150-160 used by extents.rs:178
- Problem: Scan passes `std::fs::canonicalize(root)`, which on Windows yields `\\?\C:\...`. The mount is whatever `GetVolumePathNameW` returns for that input, converted lossily. If it returns `C:\` (no verbatim prefix), `relative_components` fails the `starts_with` and splits the whole root into components such as `?` and `C:`. That builds a wrong folder chain under the mount folder. Whether the prefix is kept is not documented, and it is not verified here.
- Impact: Possible wrong folder rows and failed `find_folder_by_path` in lookup on Windows. Worst case, the same volume gets different folder chains depending on how a root was spelled.
- Suggested direction: add a Windows test or one-off check that prints `canonicalize(root)` and `info.mount`. Normalize both through one function (strip or add `\\?\` consistently) before storing.

### PIPE-11. Hardlinks are counted per path when classifying prefixes, and pass_full does not collapse them

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: core pipeline
- Severity: Medium
- Category: performance
- Confidence: High
- Location: crates/core/src/pipeline/pass_prefix.rs:444-450 (`index.count(h) > 1` counts rows); queries.rs:368-377 (`singleton_prefixes` uses `COUNT(*)=1`); pass_full.rs:58-115 (no hardlink grouping); plan P1.4.1 rule 3
- Problem: siblings get their representative's prefix hash, so two links to one inode count as 2 and are "partnered" even when no other inode shares the prefix. `pass_full` then reads every sibling separately. The plan says partners count distinct inodes.
- Impact: a 50 GB hardlinked file plus any unrelated file of the same size is fully read twice (once per link), and `recompute_groups` then drops it anyway (distinct_inodes = 1). The naive pipeline counts distinct inodes (naive.rs:408-414, :505), so the two pipelines differ in I/O.
- Suggested direction: classify partners by distinct `COALESCE(file_id,id)`, and in `pass_full` hash one representative per `file_id` and copy the result to its siblings, as `pass_prefix` already does.

### WIO-02. Overlapped path opens the target twice; the second open can resolve to a different file, and only one open is counted

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Medium
- Category: correctness
- Confidence: Medium
- Location: crates/backend-windows/src/handle.rs:281-301 (open_target), 248-251 (open_overlapped)
- Problem: `open_target` opens a sync handle (by id, else path) only to learn the length, then calls `open_overlapped`, which repeats the whole id-then-path resolution. The length passed to `OverlappedFile` comes from the first handle, but the reads go through the second. If the id open succeeded the first time and fails the second time (the file was deleted or replaced in between), the second open silently falls back to the path and reads a different file, bounded by the first file's length. `self.opens` is incremented once for two opens.
- Impact: a rare race yields a hash of the wrong file truncated or padded to the wrong length. The `unchanged()` path re-stat catches most cases but not all, for example an equal-size replacement with preserved timestamps. The open count used by "no I/O on rescan" style checks is also understated.
- Suggested direction: derive the overlapped handle from the first one with `ReOpenFile(handle, GENERIC_READ, SHARE_ALL, FILE_FLAG_OVERLAPPED | pattern)`. That keeps one file object, needs no second resolution, and makes the count accurate.

### CORE-04. Any read failure is labelled `Unmounted`, so deleted or unreadable candidates are reported as matches with no hash evidence

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core model/lookup/report
- Severity: Medium
- Category: correctness
- Confidence: High
- Location: crates/core/src/lookup.rs:106-120, crates/core/src/lookup.rs:141-170
- Problem: Plan P1.8 steps 4 and 6 hash on demand when the volume is mounted, and otherwise keep the candidate as `Unmounted`. The code never checks mount state. Any `hash_prefix`/`hash_full` error (ENOENT, EACCES, sharing violation) becomes `None`, and then a `MatchRow { freshness: Unmounted }`.
- Impact: A state -3 row (no stored prefix) whose file was deleted after the scan, on a mounted volume, comes back as a "match" labelled `unmounted` purely because the size is equal. P1.8 step 7 says a Missing file must never be reported as a plain match. For a common size (for example 4096), every unreadable same-size file shows up as a match.
- Suggested direction: Run `reader.metadata` (or `vi.identify`) on hash failure. Map NotFound to `Missing` and exclude it, and use `Unmounted` only when the volume root is absent. Consider not counting a size-only candidate as a match at all.

### CORE-05. `lookup --insert` writes outside the single writer: no transaction, racy id allocation, and the state machine is bypassed

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core model/lookup/report
- Related: DB-04
- Severity: Medium
- Category: concurrency
- Confidence: Medium
- Location: crates/core/src/lookup.rs:240-256, crates/core/src/lookup.rs:269, crates/core/src/db/ids.rs:16-28
- Problem: `persist` runs several `UPDATE`s plus an `INSERT` on the caller's connection, with no transaction. It sets `state=5` unconditionally, including from -3/-4/0/1/2. That breaks the forward-only, within-run rule, and `dup_groups` is not recomputed. It allocates a file id with `IdAllocator::seed(conn)?.file()` (`MAX(id)+1`) while a concurrent `scan` may hold uncommitted pre-assigned ids.
- Impact: If `lookup --insert` runs during a scan, the id can collide with an id the scan's allocator already handed out. The writer's later INSERT then hits a primary-key conflict: `upsert_file`'s `ON CONFLICT` targets only `(volume_id,folder_id,name)`, so the conflict raises an error and causes `WriterDied`. If one statement fails, some on-demand hashes stay persisted and the rest do not. A state -3 row promoted to 5 is still absent from `dup_groups` until the next scan. Confidence is Medium because it depends on SQLite letting the lookup write slip between writer commits, which WAL plus busy_timeout would allow.
- Suggested direction: Wrap persist in one transaction. Refuse to run, or skip the insert, while `latest_running_run` exists. Use `INSERT ... RETURNING id` or AUTOINCREMENT instead of seeding from MAX(id).

### CORE-06. `lookup --insert` silently skips the query row on Windows paths and whenever there is no match

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core model/lookup/report
- Severity: Medium
- Category: cross-platform
- Confidence: Medium
- Location: crates/core/src/lookup.rs:173-179, crates/core/src/lookup.rs:263-268, crates/core/src/pipeline/pass_enumerate.rs:302-311
- Problem: (1) `persist` runs only when `matches` is non-empty, so a new unique file is never inserted. Plan P1.8 says `--insert` persists "the query file as a new row if it lives on an indexed volume". (2) `std::fs::canonicalize` on Windows returns `\\?\C:\...`, while the stored mount is typically `C:\`. `relative_components` uses a byte `starts_with`, so the mount prefix is not stripped and the components become `["?", "C:", ...]`. `find_folder_by_path` returns `None`, and the insert is skipped with `Ok(())`. The comparison is also case-sensitive (`c:\` vs `C:\`).
- Impact: On Windows, `--insert` never inserts the query row, and on every OS it never inserts a non-matching file. Both cases return success with no message. The Windows half is inferred from std's documented canonicalize behaviour and the stored mount form, and no test exercises it, hence Medium confidence. Confirm with a Windows run, or by checking which mount string `win_sys` and the volume code store.
- Suggested direction: Persist the query row independent of the match outcome. Strip verbatim prefixes and compare mount prefixes component-wise, case-insensitively on Windows. Return or log a reason when the insert is skipped.

### CORE-07. Lookup reconstructs a path for every same-size candidate before the prefix filter, and fully hashes the query even with no confirmable candidate

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core model/lookup/report
- Severity: Medium
- Category: performance
- Confidence: Medium
- Location: crates/core/src/lookup.rs:100-105, crates/core/src/lookup.rs:127-132, crates/core/src/db/queries.rs:468-476
- Problem: `queries::reconstruct_file_path` (a fresh `conn.prepare` plus a recursive CTE per call) runs for every candidate, including those whose stored `prefix_hash` will be rejected. When `survivors` is empty but `unmounted` is not, the code still runs `hash_full` over the whole query file.
- Impact: A common size (4 KiB, 1 MiB chunks) can have 10^5 candidates in a 10M-row DB, which means 10^5 CTE queries even when nothing matches. That works against the P1.8 target of under 5 s. Looking up a 10 GB file whose only candidates are unconfirmable reads the full 10 GB anyway.
- Suggested direction: Filter on the stored prefix first and build paths only for survivors and on-demand rows, through `PathCache`. Skip `hash_full` when `survivors` is empty.

### MFT-06. VolumeTree budget estimate undercounts real heap use by roughly 1.5-2x, and dir-path cache is unbudgeted

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Medium
- Category: performance
- Confidence: Medium
- Location: crates/backend-windows/src/tree.rs:27-28,104,278-283,296-310,317-360; crates/backend-windows/src/records.rs:256-271
- Problem: `record_cost` = 128 + per name (2*len + size_of::<NameFact>() = 40). Actual: `RecordFacts` is about 80 bytes inline in a BTreeMap node (plus node slack, up to about 1.5x at random insert, fill is better for sorted insert), `names: Vec<NameFact>` grows from `Vec::new()` via `push`, so the first push allocates capacity 4 (4*40 = 160 bytes) for the common one-name record, plus `retain` (tree.rs:142) never shrinks it, plus per-allocation allocator overhead for two heap blocks. Emission then builds `index_dir_children` (HashMap of Vec per dir), `dirs` (HashMap with an owned full `RawPath` per directory) and `others` (Vec of refs) on top of the budgeted records, explicitly uncounted (tree.rs:188-189).
- Impact: default `mem_budget` 1 GiB (core/src/config.rs:51) -> real peak can reach about 2 GiB plus directory paths on a volume the budget admitted. Also the budget counts the whole volume even when the scan root is a small subdirectory (push does not scope to root; the struct doc at tree.rs:98 "Records under one root" is inaccurate), so a small subtree scan on a large volume falls back with BudgetExceeded.
- Suggested direction: Build `names` with `Vec::with_capacity(n)` / `shrink_to_fit`, store records in a sorted `Vec<RecordFacts>` (records arrive in ascending order) or a compact arena of names, and include the dir-path map in the estimate. Fix the struct doc.

### MFT-07. Stale parent references: $FILE_NAME parent sequence number is discarded

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Medium
- Category: correctness
- Confidence: Medium
- Location: crates/backend-windows/src/records.rs:267; crates/backend-windows/src/tree.rs:53-58,235,302-306
- Problem: `NameFact.parent` stores `file_record_number()` only. The parent reference's 16-bit sequence is dropped, so a name whose parent record was freed and reused (by a different directory) attaches to the new occupant. The raw $MFT is read live over minutes (not a snapshot), so delete/recreate races during the read, plus corruption, can produce such stale references.
- Impact: A file whose parent directory was deleted and whose record was reused by another directory mid-walk -> file emitted under the wrong path (wrong lookup/report paths) instead of being treated as an orphan.
- Suggested direction: Keep the parent sequence in `NameFact` and require it to equal the parent record's `sequence` in `index_dir_children` and the file phase; treat a mismatch as an orphan.

### MFT-08. "BAAD" and other non-FILE signatures are treated as free space, not errors

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Medium
- Category: error-handling
- Confidence: High
- Location: crates/backend-windows/src/records.rs:108-112; ntfs-0.4.0/src/file.rs:509-521
- Problem: `Err(NtfsError::InvalidFileSignature { .. }) => return Ok(None)` covers both a never-written record (zeros) and a record NTFS marked "BAAD" after a multi-sector transfer failure, or any garbage. Only the all-zero case is free space.
- Impact: A damaged in-use record -> file silently absent, not counted in `records_skipped_errors`, so it also escapes the 5% sanity limit and the (future) notice proposed in MFT-03.
- Suggested direction: Inspect `actual` in the error: return `Ok(None)` only for an all-zero signature, `Err(Corrupt(...))` otherwise.

### MFT-09. Cache trust relies on the USN journal, which offline or non-Windows writers do not update

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Medium
- Category: data-integrity
- Confidence: Medium
- Location: crates/backend-windows/src/mft_cache.rs:456-487; crates/backend-windows/src/enumerator.rs:305-340
- Problem: `validate` accepts the cache when volume serial, record size, total bytes, journal id and USN range match. Changes made while the volume was mounted elsewhere (Linux ntfs-3g/ntfs3, which to my knowledge do not write $UsnJrnl; another Windows install with the journal disabled then re-enabled with the same id is unlikely) or on a byte-identical clone sharing serial and journal id are invisible. There is no cheap content cross-check (e.g. re-reading a sample of records, or comparing the $MFT record 0 LSN / $LogFile state).
- Impact: External drive edited on Linux, then scanned on Windows with cache on -> stale sizes/mtimes/names reported; new files missing; deleted files still emitted (read errors later).
- Suggested direction: Add a cheap spot-check (re-read N random cached records and compare sequence/mtime; or store and compare the $Volume / $LogFile LSN), and document the limitation for `--mft-cache`.

### MFT-11. Whole cache file is read into memory, then decoded, then copied into a BTreeMap

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Medium
- Category: performance
- Confidence: High
- Location: crates/backend-windows/src/mft_cache.rs:243-290,451-454; crates/backend-windows/src/enumerator.rs:388
- Problem: `read` does `fs::read(path)` (whole file), `decode` builds a `Vec<RecordFacts>`, then `VolumeTree::from_facts` moves each into a BTreeMap. Peak is file bytes + decoded Vec + tree simultaneously during from_facts (the Vec is consumed incrementally, but its buffer is not freed until the end). The budget check runs only in `from_facts`, after the full decode.
- Impact: 10M records (cache file several hundred MB) -> transient peak of roughly 2-3x the budgeted size before BudgetExceeded can trigger.
- Suggested direction: Stream-decode with a `BufReader` and a hashing reader (mirror `HashingWriter`), pushing straight into the tree, and verify the trailer at the end before returning the tree.

### MFT-13. tests/mft_vs_walk.rs does not exercise the risky paths and has a stale exclusion rationale

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Medium
- Category: test-gap
- Confidence: High
- Location: crates/backend-windows/tests/mft_vs_walk.rs:89-94,122,170-173,66-87
- Problem: (a) The doc says file_id and nlink are excluded because "the walker reports None and 1", but CLAUDE.md (Gotchas) says the Windows walker now fills both via `fill_file_identity`; the test therefore skips the exact check CLAUDE.md lists as unverified (walker file index equals the $MFT file reference) and also masks out HARDLINK. (b) `AutoEnumerator::new` uses `CacheConfig::default()` (dir None, enumerator.rs:69-75,94-101), so the cache/USN delta path is never run on real hardware. (c) The tree has no junction/symlink-dir, cloud placeholder, ADS, sparse or compressed file, long path (>260), unpaired-surrogate name, case-sensitive (POSIX namespace) directory, many-hard-link file (forces $ATTRIBUTE_LIST), or deleted-then-reused record. (d) It never checks `records_skipped_errors`, so a walk that silently dropped records elsewhere on the volume (MFT-03) still passes; the scratch subtree is tiny relative to the volume. (e) Fallback detection relies on `notices.drain()` being empty rather than `notices.walker_used()`, which exists for this.
- Impact: The one on-hardware self-check can pass while MFT-02/03/04/05 are live.
- Suggested direction: Compare file_id and nlink too; add a second run with a temp cache dir (cold, then warm after mutating files); add junction, OneDrive-like reparse, ADS, compressed, sparse, long-path and 100-link cases; assert `!notices.walker_used()` and zero skipped records.

### WIO-03. Raw `$MFT` read can miss changes still in the NTFS cache, and the cache stamp makes the miss persistent

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Medium
- Category: data-integrity
- Confidence: Low
- Location: crates/backend-windows/src/volume_io.rs:175-184 (open_raw_volume, flags 0); crates/backend-windows/src/enumerator.rs:220-242 (journal stamp taken before the read, then store_cache)
- Problem: records are read through a DASD volume handle. NTFS keeps recently changed file records dirty in its own metadata cache and writes them lazily, and I am not certain a volume-handle read is coherent with those dirty pages. If it is not, the read sees the on-disk version. `cache_site` stamps the cache with the journal `next_usn` queried before the read, and later scans replay only USNs at or after that stamp. A change whose USN is below the stamp but whose record was not yet on disk is lost from the cache until the record changes again.
- Impact: files created or resized seconds before the scan are missing from this scan or carry a stale size. With the cache enabled, the stale entry survives later delta scans. To confirm: create a file, immediately run the MFT walk, and check whether it appears (the `mft_vs_walk` self-check under write load).
- Suggested direction: document the limitation, or after the full read re-read the records named by journal entries in `[stamp, next_usn_after_read)`, which is the same mechanism `apply_delta` already uses.

### CLI-04. DB self-exclusion compares a non-canonical `--db` path against canonical walk paths

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Also reported as: PIPE-14 (core pipeline, Medium)
- Severity: Medium
- Category: data-integrity
- Confidence: High (Unix symlink case); Medium (Windows verbatim and case variants)
- Location: crates/core/src/pipeline/pass_enumerate.rs:29-38 and :99; crates/core/src/pipeline/scan.rs:46; crates/cli/src/scan.rs:58-61
- Problem: roots are `std::fs::canonicalize`d, so walk paths are symlink-resolved, and on Windows they carry the `\\?\` prefix. The skip set is built with `std::path::absolute(db)`, which resolves neither symlinks nor `..` on Unix and does not add `\\?\`. The default DB (`scan_default_db`) is canonical and matches. An explicit `--db` matches only when it is already written in canonical form.
- Impact: `dedupe scan /tmp/r --db /tmp/r/idx.db` on macOS (where `/tmp` links to `/private/tmp`), or any `--db` path through a symlinked home or a `..`, makes the scan index and hash its own live database and its `-wal`/`-shm` files while the writer changes them. The result is wasted I/O, spurious `-2` errors or stale rows, and the DB showing up in `lookup` results. On Windows, `--db C:\data\x.db` against a canonical root `\\?\C:\data` probably misses the same way, and so does a case difference. Related: other `.dedupe.db` files under a root (from an earlier scan of a subfolder) are always indexed and reported as duplicates of each other.
- Suggested direction: canonicalize the DB's parent directory once the DB exists (after `Db::open`) and compare in the same raw form the walker yields. Consider also skipping any file named `.dedupe.db*`.
- PIPE-14 adds:
  - Location: crates/core/src/pipeline/pass_enumerate.rs:29-39, :99; crates/core/src/pipeline/scan.rs:46 (roots are canonicalized)
  - Problem: walker paths come from `canonicalize(root)` (symlinks resolved, `\\?\` verbatim prefix on Windows), but the skip set uses `absolute()`, which resolves neither. The match is also byte-exact, so it is case-sensitive.

### CLI-06. `println!`/`print!` panic on a closed stdout (broken pipe)

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: error-handling
- Confidence: High
- Location: crates/cli/src/scan.rs:287-288; crates/cli/src/lookup.rs:50-51; crates/cli/src/db.rs:18,26,33,42-62
- Problem: Rust ignores SIGPIPE, so `println!` on a closed pipe panics with "failed printing to stdout". `report` writes through `writeln!` and returns an error, but `scan`, `lookup` and `db` use the panicking macros.
- Impact: `dedupe scan ~/x | head -5` (JSON auto-selected) exits 101 with a panic message and backtrace hint on stderr after a long scan. Scripts see a crash rather than a clean exit.
- Suggested direction: write through a locked `stdout` with `writeln!`, and treat `ErrorKind::BrokenPipe` as success, in one shared helper.

### CLI-08. `db drop` deletes any file given as `--db`/`--dir` without checking that it is a dedupe database, and builds sibling paths from lossy `display()`

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: data-integrity
- Confidence: High
- Location: crates/cli/src/db.rs:21-34; crates/cli/src/main.rs:355-371
- Problem: `resolve_existing_db` checks only `path.exists()` for `--db`, and accepts any file for `--dir <file>` (`dir.is_file()`). `drop` then removes it. Sibling paths are built with `PathBuf::from(format!("{}{suffix}", db.display()))`, which is lossy for non-UTF-8 paths.
- Impact: (a) `dedupe db drop --dir ./thesis.docx --yes`, or `--db` pointed at the wrong file, irreversibly deletes a user file. That contradicts README lines 5 and 203 ("never deletes anything"). (b) For a non-UTF-8 DB path the lossy path does not exist, so nothing is removed, but the command still prints `dropped <path>` and exits 0.
- Suggested direction: require `discover::is_dedupe_db`-style validation before dropping, and build siblings with `OsString::push`. Also update the README wording.

### CLI-09. Read-write opens create the dedupe schema inside an unrelated SQLite file and switch it to WAL

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: data-integrity
- Confidence: High
- Location: crates/core/src/db/mod.rs:23-28; crates/core/src/db/schema.rs:27-38; reached from crates/cli/src/db.rs:16 (`vacuum`), crates/cli/src/lookup.rs:29 (`--insert`), crates/cli/src/scan.rs:170 (`--db`)
- Problem: `Db::open` applies pragmas (including `journal_mode=WAL`) and then runs `apply_migrations`. Most third-party SQLite files report `user_version` 0, which `apply_migrations` treats as a fresh dedupe DB and fills with `SCHEMA`.
- Impact: `dedupe db vacuum --db app.sqlite`, `dedupe lookup --insert --db places.sqlite`, or a `scan --db` typo writes dedupe tables into another application's database. It also converts that database to WAL, or fails partway on a table-name collision such as `files`.
- Suggested direction: for an existing non-empty file, require the `meta` table or the expected `user_version` before migrating. Create the schema only when the file is new or empty.

### CLI-10. `report --output` can truncate the database, and leaves an empty file on failure

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: data-integrity
- Confidence: High
- Location: crates/cli/src/report.rs:21-22,35-38,43-46
- Problem: the output file is created with `File::create` (truncate) after the DB is resolved, without checking that it differs from the DB path. In JSON mode it is created before the report is built.
- Impact: `dedupe report -o .dedupe.db` (a tab-completion slip) zeroes the index while it is open. A JSON report that fails or is cancelled leaves a 0-byte file that later tooling reads as an empty report.
- Suggested direction: refuse `--output` when it resolves to the DB or its siblings. Write to a temp file in the same folder and rename it on success.

### CLI-11. `scan --resume` silently ignores the roots on the command line

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: api-design
- Confidence: High
- Location: crates/core/src/pipeline/scan.rs:40-43; crates/cli/src/main.rs:84-98
- Problem: `roots` are required by clap, but in the resume branch `run_scan` uses only `plan_resume(&db.conn, ...)`. The given roots are used only to pick the default DB path (first root) and the storage probe.
- Impact: `dedupe scan ~/B --db idx.db --resume`, after an interrupted scan of `~/A`, resumes `~/A` and reports success. The user believes `~/B` was scanned. The storage profile is also probed on `~/B` while `~/A` is read.
- Suggested direction: compare the given roots with the resumed run's roots and error (or warn) on a mismatch, or make roots optional with `--resume`.

### CLI-12. Default DB inside the first root fails opaquely for file roots, read-only roots, and missing later roots

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: error-handling
- Confidence: High
- Location: crates/cli/src/main.rs:318-323; crates/core/src/pipeline/scan.rs:46
- Problem: `scan_default_db` joins `.dedupe.db` onto the canonical first root without checking that it is a writable directory. Later roots are canonicalized in core with a bare `?`, so the resulting `CoreError::Io` has no path.
- Impact: `dedupe scan photo.jpg` and `dedupe scan /mnt/readonly-backup` fail with `scan failed: unable to open database file` and no hint to pass `--db`. `dedupe scan ~/A ~/typo` fails with `scan failed: No such file or directory (os error 2)` that does not name the bad root. The storage probe also runs on the missing root first and prints the unknown-storage note before the failure.
- Suggested direction: validate every root in the CLI first (exists, is a directory, otherwise name it). When the first root is not writable, say so and suggest `--db`.

### DB-09. Stale-deletion recursive CTE has no usable index on folders.parent_id

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: core db/index
- Severity: Medium
- Category: performance
- Confidence: Medium
- Location: db/writer.rs:386-405, db/queries.rs:441-449, db/schema/v2.sql:23-30
- Problem: The recursive step joins `folders f ON f.parent_id = s.id` with no `volume_id` term. The only index containing parent_id is `UNIQUE(volume_id, parent_id, name)`, whose leading column is not constrained, and no `ANALYZE` is ever run, so a skip-scan is unlikely. The CTE is also evaluated twice (files, then folders).
- Impact: Each queued folder can trigger a full scan of `folders`, making stale deletion roughly O(F^2): with 200k folders that is tens of billions of row visits inside one writer transaction, stalling the writer and every pass waiting on `flush`. Confirm with `EXPLAIN QUERY PLAN` on a populated DB.
- Suggested direction: Add `AND f.volume_id = ?1` to the recursive step (lets it use the UNIQUE index) or add `CREATE INDEX ... ON folders(parent_id)`; materialize the subtree once into a temp table and reuse it for both deletes.

### DB-10. idx_files_size is unusable by the queries that need it

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: core db/index
- Severity: Medium
- Category: performance
- Confidence: Medium
- Location: db/schema/v2.sql:53, lookup.rs:207-209, db/queries.rs:303-315
- Problem: `idx_files_size(volume_id, size) WHERE state NOT IN (-1,-2)`. Lookup's `size_candidates` filters `size=?1 AND state NOT IN (-1,-2)` with no `volume_id`, so the leading column is unconstrained. The pass-side `size_candidates` uses `state=0`, which SQLite does not treat as implying `state NOT IN (-1,-2)`, so the partial index is not eligible there either.
- Impact: Every `lookup` is a full table scan (about 1M rows per lookup at target scale), while every insert and state change still pays to maintain the index.
- Suggested direction: Index `files(size) WHERE state NOT IN (-1,-2)` for lookup (or add the volume to the lookup query), and verify with `EXPLAIN QUERY PLAN`.

### DB-11. Hot per-file queries are re-prepared on every call

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: core db/index
- Also reported as: PIPE-13 (core pipeline, Medium)
- Severity: Medium
- Category: performance
- Confidence: High
- Location: db/queries.rs:159-172 (find_folder), 233-250 (find_by_path), 252-267 (find_move_candidate)
- Problem: These run once per enumerated directory or file on the read connection. `find_by_path` builds the SQL with `format!` and `query_row` prepares a new statement every call; `find_folder` and `find_move_candidate` also prepare fresh statements (`prepare`, not `prepare_cached`).
- Impact: At 1M files that is 1M+ SQL compiles plus string allocations during enumeration, a measurable share of a cold scan's CPU time.
- Suggested direction: Use `const` SQL strings with `conn.prepare_cached(...)`, as `PathCache` and the writer already do.
- PIPE-13 adds:
  - Location: crates/core/src/pipeline/pass_enumerate.rs:189, :217, :241; crates/core/src/db/queries.rs:159-172 (`query_row`), :233-248 (`format!` + `query_row`), :252-267 (`conn.prepare`)
  - Problem: each file does `find_by_path` and, if new, `find_move_candidate`, and each directory does `find_folder`. All of them go through `prepare` or `query_row` (which prepares internally), not `prepare_cached`, and two of them `format!` the SQL first.

### DB-12. Memory budget is only checked after the whole in-memory index is built

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: core db/index
- Also reported as: PIPE-10 (core pipeline, Medium)
- Severity: Medium
- Category: performance
- Confidence: High
- Location: index/memory.rs:41-43 and 78-80, index/memory.rs:12-14 (ENTRY_OVERHEAD), pipeline/pass_prefix.rs:47-50 and 86-106
- Problem: `over_budget()` is called once, after every row of the pass has been inserted into `MemoryIndex`, while `rows: Vec<FileEntry>` and the whole map are still in memory. The SQLite fallback therefore never limits peak memory. `ENTRY_OVERHEAD = 48` also ignores hashbrown spare capacity (up to about 2x after a resize) and the SmallVec size (24 to 32 bytes), so "deliberately conservative" is not accurate.
- Impact: A pass over a very large candidate set can exceed `mem_budget` by a wide margin before the fallback is chosen; the fallback then adds DB work on top.
- Suggested direction: Check the budget during insertion and switch to the DB path early, or estimate from `rows.len()` before building; base the estimate on `map.capacity()`.
- PIPE-10 adds:
  - Location: crates/core/src/pipeline/pass_prefix.rs:367-370, :375-378, :413-426; crates/core/src/index/memory.rs:57-64
  - Problem: (a) `MemoryIndex::insert` copies every existing partner into a new Vec on each call (`v.iter().copied().collect()`), and `pass_prefix` throws that Vec away. A group of n equal prefixes costs O(n^2) copies. (b) `over_budget()` is checked only after every large row is already in the map, so `mem_budget` does not cap memory. It only chooses the classification method afterwards. (c) `outcomes: HashMap<usize, HashOutcome>` keeps every row's outcome, cloning each `ErrRow`, although only hardlink representatives need it. `large.clone()` at :426 copies the whole vector again.

### GEN-06. Windows walker opens every file twice although walkdir already has the metadata

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: backend-generic
- Severity: Medium
- Category: performance
- Confidence: High
- Location: crates/backend-generic/src/walk.rs:44-47, crates/backend-generic/src/meta.rs:70-81, crates/backend-generic/src/lib.rs:122-143
- Problem: For each entry the walker calls `path.symlink_metadata()`. On Windows that is a `CreateFileW` plus `GetFileInformationByHandle`. Then `fill_file_identity` opens the same file again for the index and link count. walkdir 2.5 on Windows already caches `fs::Metadata` from `FindNextFileW` (`entry.metadata()` costs nothing extra), and the identity open alone returns everything else (attributes, size, times, index, nlink).
- Impact: Two opens per file on NTFS, each also passing through filter drivers such as Defender. On a 1M-file walker scan (non-elevated or `--no-mft`) that roughly doubles enumeration time.
- Suggested direction: use `entry.metadata()` (cached on Windows) for the find-data fields and a single `open_handle` plus `GetFileInformationByHandle` for identity, or build `FileMeta` entirely from that one handle.

### GEN-08. Linux storage probe returns unknown on btrfs, ZFS, bcachefs and overlay

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: backend-generic
- Severity: Medium
- Category: performance
- Confidence: High
- Location: crates/backend-generic/src/probe.rs:53-68
- Problem: The probe maps `st_dev` to `/sys/dev/block/<maj>:<min>`. btrfs (each subvolume), ZFS, bcachefs and overlayfs report an anonymous device (major 0), which has no sysfs block entry, so both lookups fail and the result is `None`.
- Impact: On Fedora and openSUSE (btrfs by default) every scan resolves to `StorageKind::Unknown`. The full-hash queue depth is capped at `UNKNOWN_QUEUE_DEPTH` (2) on NVMe, and the CLI prints the unknown-storage note.
- Suggested direction: when the major is 0, resolve the source device from `/proc/self/mountinfo` (or `/sys/fs/btrfs/<fsid>/devices/*` for btrfs) and check every backing device's `queue/rotational`.

### GEN-10. Windows extents: GetVolumePathNameW runs per file before the cluster-size cache

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: backend-generic
- Severity: Medium
- Category: performance
- Confidence: High
- Location: crates/backend-generic/src/extents.rs:120-137, 176-194
- Problem: The cache is keyed by the volume root string, but finding the key needs `volume_root(path)` (`GetVolumePathNameW`) for every file. That call walks the path components. Each lookup also does its own `CreateFileW`, separate from the hash pass's open.
- Impact: On the HDD path (where extent ordering is used), millions of files cost millions of extra path-walk calls and opens before any data is read, which partly cancels the seek savings the ordering is meant to give.
- Suggested direction: compute bytes-per-cluster once per scanned volume (the caller knows the volume) and pass it in, or key the cache by the volume serial from the already-open handle.

### PIPE-12. pass_prefix is always single-threaded, and every hash allocates a buffer of `buf_size` even for tiny files

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: core pipeline
- Severity: Medium
- Category: performance
- Confidence: High
- Location: crates/core/src/pipeline/pass_prefix.rs:380-395 (sequential loop, `queue_depth` ignored); crates/core/src/hash.rs:143 (`vec![0u8; chunk]`); pass_verify.rs:306-307
- Problem: on SSD the prefix pass over 1M small files (open + about 20 KiB read + re-stat each) is latency-bound on one thread, while `pass_full` has a pool. `hash_full_target` allocates and zeroes `chunk` bytes per call. Under the HDD profile `buf_size` is `HDD_BUF_SIZE`, about 1 MiB per the scheduler test, so each 1 KiB small file costs a 1 MiB zeroed allocation. `--buf-size` is user-set, so a large value multiplies this per file and per worker.
- Impact: the prefix pass is likely the wall-clock bottleneck on large SSD trees, with heavy allocator and page-fault churn on HDD.
- Suggested direction: reuse the `pass_full` pool for the prefix pass, and size the buffer as `min(chunk, size)` or reuse one buffer per worker.

### WIO-04. Every file open calls `GetVolumePathNameW` and takes a global mutex

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Medium
- Category: performance
- Confidence: Medium
- Location: crates/backend-windows/src/handle.rs:206-213 (hint_for), 229-231
- Problem: `hint_for` runs `volume_root(path)` before the cache lookup on every `open_target` that carries an id. `GetVolumePathNameW` walks the path's components to find mount points, which costs several kernel calls per open, and every hash worker then serializes on `self.hints.lock()`.
- Impact: noticeable per-file overhead on scans with millions of small files, where opens dominate. This is the hot path that open-by-id is meant to speed up.
- Suggested direction: cache by scan root or by the longest known root prefix, or pass the volume root in from the pipeline. Use an `RwLock`, or a `OnceLock` per volume, for the read-mostly map.

### CLI-14. cargo-deny config uses removed keys and will fail on unversioned path dependencies

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Severity: Medium
- Category: build-config
- Confidence: Medium
- Location: deny.toml:8-9,14,32; crates/cli/Cargo.toml:15-16,27; bench/Cargo.toml:14-15; crates/tui/Cargo.toml:14; no `publish = false` in any manifest
- Problem: (a) `[advisories] vulnerability`, `[licenses] unlicensed` and the `"warn"` value for `unmaintained` are cargo-deny v1 forms. They were deprecated with the config `version = 2` change and later removed. In current cargo-deny, `unmaintained` takes a scope such as `"all"` or `"workspace"`. (b) `wildcards = "deny"` treats `path` dependencies without a `version` as wildcards. `allow-wildcard-paths` (not set) applies only to crates marked `publish = false`, and none are.
- Impact: `cargo deny check`, which CLAUDE.md lists as the dependency-policy gate, either fails to parse the config on a recent cargo-deny or fails `bans` on every internal path dependency. The policy is then not enforced.
- Suggested direction: migrate to `version = 2` keys, set `publish = false` on every workspace crate, and add `allow-wildcard-paths = true`. To confirm, run `cargo deny --version` and `cargo deny check` locally.

### CLI-19. README and CLAUDE.md statements the code contradicts

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Related: CORE-18
- Severity: Medium
- Category: docs
- Confidence: High
- Location: README.md:59,90,156,170,198,207,212; CLAUDE.md:9,132 and the "Default DB location" bullet; xtask/src/main.rs:5; crates/cli/src/scan.rs:69-70
- Problem:
  - README:59 says the `--format` default is `human`. The code default is `auto`, which prints JSON whenever stdout is not a TTY (main.rs:56-66,171), so `dedupe scan ~/x > out.txt` writes JSON.
  - README:90/156/170 and CLAUDE.md say report, lookup and db default to `./.dedupe.db`. The code picks the single dedupe-schema `*.db` in the folder, or errors if there are several (discover.rs:55-73). `report [DIR]` (positional), `-o/--output`, `lookup --dir`, `db --dir`, `--exclude`, `--no-mft-cache`, `--rebuild-mft-cache` and `--one-file-system` are not documented.
  - README:207 says `--verify` is "planned but not available yet". README:61 and the code (main.rs:143-146) provide `--verify byte`.
  - README:198 says the standard walker reports no file id. CLAUDE.md says the Windows walker fills `file_id` via `fill_file_identity`.
  - README:212 and CLAUDE.md:9,132 point to `../.overall_plan.md` and say the git root is one level up. `git rev-parse --show-toplevel` is this directory, and `.overall_plan.md` is here.
  - xtask/src/main.rs:5 prints "schema is v1". CLAUDE.md says the schema version is 2.
  - scan.rs:69-70 says "core maps unknown storage to the SSD policy". Per CLAUDE.md, Unknown keeps SSD buffers but caps queue depth at 2.
- Impact: users get JSON when they expected text, follow a wrong DB default, and agents read the wrong plan path.
- Suggested direction: fix the text in one pass. The `--format` line matters most.

### CORE-08. `ExcludeSet` has no tests on Linux or macOS

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Severity: Medium
- Category: test-gap
- Confidence: High
- Location: crates/core/src/exclude.rs:1-52, crates/backend-windows/tests/mft_vs_walk.rs (the only test file that mentions exclude; Windows-only and `#[ignore]`)
- Problem: `exclude.rs` has no `#[cfg(test)]` module, and no core or CLI test passes an exclude pattern. `normalize` (backslash rewrite, `//?/` drive stripping) and `literal_separator(true)` semantics are unverified.
- Impact: A regression in pattern semantics would ship unnoticed, for example `*` crossing `/`, `**/node_modules` failing to prune, or the root being excluded. The walker prune (`backend-generic/src/walk.rs:34`) and the `$MFT` drop path (`pass_enumerate.rs:87,100`) both depend on it.
- Suggested direction: Add unit tests for `normalize` and `is_excluded`: Unix, `C:\`, `\\?\C:\`, `\\?\UNC\`, `*` vs `**`, and the trailing-slash pattern. Add one `run_scan` test with an exclude set.

### CORE-11. Missing tests for lookup and report edge cases (self-match, insert on indexed file, multi-root report, Windows paths)

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Severity: Medium
- Category: test-gap
- Confidence: High
- Location: crates/core/tests/phase1.rs:369-445, crates/core/tests/phase1.rs:767-795, crates/core/tests/phase1.rs:796-863
- Problem: No test looks up a path that is itself indexed (CORE-03), runs `--insert` on an indexed file (CORE-02), inserts a non-matching file (CORE-06), or checks the report after a partial-root rescan: `rescan_root_scoping_leaves_other_roots_untouched` asserts only that the B row survives, not that the A/B pair is still reported (CORE-01). No core test drives a Windows-style mount (`C:\`) through `reconstruct_file_path`, lookup `persist` or `relative_components`. The `cfg(windows)` proptest (props.rs:150-157) only round-trips bytes through SQLite, never through the codec.
- Impact: CORE-01, -02, -03 and -06 are all invisible to the current suite.
- Suggested direction: Add the four scenarios above to phase1.rs. Add a portable test that feeds a fake codec and volume with a `C:\` mount through scan, report and lookup.

### DB-13. Unit test still asserts schema version 1

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core db/index
- Severity: Medium
- Category: test-gap
- Confidence: High
- Location: db/mod.rs:86, db/schema.rs:7
- Problem: `file_db_enables_wal` asserts `schema::current_version(&db.conn).unwrap() == 1`, but `EXPECTED_VERSION = 2` and `apply_migrations` sets `user_version = 2`.
- Impact: `cargo test -p dedupe-core` fails on this test; this suggests the core test suite has not been run green since the v2 schema bump.
- Suggested direction: Assert against `schema::EXPECTED_VERSION`.

### PIPE-16. The naive "correctness reference" behaves differently from the persistent pipeline in ways that change results

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core pipeline
- Severity: Medium
- Category: correctness
- Confidence: High
- Location: crates/core/src/pipeline/naive.rs:395-406, :430-470, :487-523
- Problem: (a) there is no change-during-read re-stat (plan P1.4.1 rule 5), so a file modified mid-read is still grouped. (b) Roots are not deduplicated or checked for nesting, and `inode_key` falls back to the record index when `file_id` is None. Overlapping roots (`scan --no-db D:\x D:\x\sub` on a volume without file ids) therefore report a file as a duplicate of itself. (c) `meta.size == 0` is skipped while a negative size counts as an error, but persistent skips `size <= 0`, so the skip/error counts differ. (d) Small files are hashed twice (once in the prefix pass and again in the full pass), and `bytes_hashed` counts them twice.
- Impact: tests that compare naive and persistent output can hide or invent differences, and `--no-db` can print false self-duplicates.
- Suggested direction: share the eligibility, re-stat and small-file rules with the DB passes, dedupe roots by path, and reuse the prefix hash as the full hash when `size <= F+L`.

## P3 - Low

_Idiom, hygiene, docs, minor edge cases, test gaps on non-critical paths._

### DB-14. recompute_groups is not atomic

- Topic: T1 (Report correctness and duplicate-group statistics); fix in Part 2, section [T1](#t1-report-correctness-and-duplicate-group-statistics)
- Reviewed slice: core db/index
- Severity: Low
- Category: data-integrity
- Confidence: High
- Location: db/queries.rs:590-606, pipeline/scan.rs:160-169
- Problem: `DELETE FROM dup_groups` and the `INSERT ... SELECT` run as two autocommit statements on a connection without an explicit transaction.
- Impact: A concurrent `report` can see zero groups for the volume during the (multi-second at 1M rows) insert; a crash in between leaves the volume with no groups, and `report` (which reads the previous finished run) shows "0 duplicate group(s)" until a resume reruns it.
- Suggested direction: Wrap both statements (and the `set_run_volume_phase` that follows) in one transaction.

### DB-15. finished_at is recorded as the run's start time

- Topic: T1 (Report correctness and duplicate-group statistics); fix in Part 2, section [T1](#t1-report-correctness-and-duplicate-group-statistics)
- Reviewed slice: core db/index
- Also reported as: PIPE-17 (core pipeline, Low)
- Severity: Low
- Category: correctness
- Confidence: High
- Location: pipeline/scan.rs:32 and 169, db/queries.rs:730-736, db/queries.rs:840-850 (latest_finished_run)
- Problem: `run_scan` passes the `now` captured at start to `finish_run`, so `finished_at == started_at`. `latest_finished_run` orders by `finished_at DESC`.
- Impact: `db info` and any duration shown are wrong; "latest finished run" is really "latest started finished run", which picks the wrong run if two runs on one volume overlap (see DB-04).
- Suggested direction: Pass `now_unix_ns()` at finish time.
- PIPE-17 adds:
  - Location: crates/core/src/pipeline/scan.rs:119, :252, :256; crates/core/src/db/queries.rs:730-735
  - Problem: `now` is taken once at the start of `run_scan` and reused for `finished_at` and `computed_at`.

### CORE-13. `is_legal_transition` is never enforced in production, so the property tests check a dead function

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: api-design
- Confidence: High
- Location: crates/core/src/model.rs:116-131, crates/core/tests/props.rs:176-199, crates/core/src/lookup.rs:243-251
- Problem: `is_legal_transition` is referenced only by tests (grep shows no call in any `src/`). Writers set states directly, for example `UPDATE files SET ... state=5` from any state in lookup.rs:243 and 251. `terminal_states_have_no_outgoing_edges` and `random_walk_stays_valid` only restate the match table.
- Impact: The "transitions only move forward within a run" invariant from CLAUDE.md has no runtime or test enforcement. The property tests give false assurance.
- Suggested direction: Call it in the writer's batch application, at least as a `debug_assert!`, or test that the real pipeline's observed transitions are all legal.

### DB-19. Duplicate SQL paths have drifted apart

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core db/index
- Severity: Low
- Category: api-design
- Confidence: High
- Location: db/queries.rs:199-231 vs db/writer.rs:228-247 (upsert_file vs Enumerated), db/queries.rs:427-451 vs db/writer.rs:373-406 (delete_stale_under_roots vs DeleteStale), db/queries.rs:174-181 vs db/writer.rs:211-226
- Problem: `upsert_file` lacks the `ON CONFLICT(id)` clause the writer has; `delete_stale_under_roots` deletes files but not folders. `upsert_file` is used by `lookup` (lookup.rs:284), `delete_stale_under_roots` only by tests.
- Impact: Tests exercise SQL that production does not run, and the two upserts give different results for the same input.
- Suggested direction: Have the writer call the `queries` functions (taking `&Transaction` derefs to `&Connection`) so there is one statement per operation; delete the test-only copy.

### DB-20. Minor idiom and robustness items

- Topic: T2 (Enumeration, id allocation, move detection, rescan rules); fix in Part 2, section [T2](#t2-enumeration-folderfile-id-allocation-move-detection-and-rescan-rules)
- Reviewed slice: core db/index
- Severity: Low
- Category: idiom
- Confidence: High
- Location: db/writer.rs:123, db/mod.rs:47, db/mod.rs:37, index/memory.rs:50-53 (distinct_keys), db/queries.rs:498-505 and 571-581, db/ids.rs:16-21, db/schema.rs:27-42
- Problem: (a) `.expect("spawn db-writer")` panics in library code; return a `Result`. (b) `apply_pragmas` takes an unused `_cfg`. (c) `open_readonly` sets `foreign_keys=ON`, which has no effect on a read-only connection. (d) `distinct_keys` is documented as "Highest count seen" but returns the key count. (e) `PathCache` caches one mount per instance with no volume check; reusing it across volumes silently produces wrong paths. (f) `MAX(id)+1` seeding reuses ids of rows deleted by stale deletion, and `errors.file_row_id` (no FK) then points at an unrelated new file. (g) `apply_migrations` reads `user_version` outside the transaction, so two processes creating a fresh DB at once fail with "table meta already exists"; a non-dedupe SQLite file with `user_version=0` gets the same confusing error.
- Impact: Mostly misleading errors and API foot-guns; (f) mislabels the error history.
- Suggested direction: Address opportunistically; for (g) use `BEGIN IMMEDIATE` and re-read the version inside the transaction.

### CLI-07. Cancellation and every failure share exit code 1, with no distinct code for "cancelled"

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: api-design
- Confidence: High
- Location: crates/cli/src/main.rs:401-418; crates/cli/src/scan.rs:162,171; crates/cli/src/report.rs:52-57
- Problem: `main` returns `anyhow::Result`, so a Ctrl-C prints `Error: scan failed` with a `Caused by: cancelled` chain and exits 1, the same as a real failure. Clap usage errors exit 2.
- Impact: wrapper scripts cannot tell "user cancelled, safe to `--resume`" from "scan broke". The `Error:` wording is alarming for a deliberate cancel.
- Suggested direction: match on `CoreError::Cancelled` in `main`, print a short note, and exit 130. Map other errors through `std::process::ExitCode`.

### DB-16. Writer's 5-second commit is not guaranteed under backpressure

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: core db/index
- Severity: Low
- Category: correctness
- Confidence: High
- Location: db/writer.rs:137-139 and 180-184
- Problem: Time-based commits fire only on `RecvTimeoutError::Timeout`. std `recv_timeout` returns a queued message even when the deadline has already passed, so while the bounded channel stays non-empty, the timer branch never runs and only the 50k-row threshold commits.
- Impact: The documented "commits every 50k rows or 5 seconds" is really "every 50k rows" when producers outpace the writer; a crash loses up to 50k rows rather than at most 5 s of work. Bounded, but the doc overstates the guarantee.
- Suggested direction: Check `since.elapsed() >= max_ms` after every received batch, not only on Timeout.

### WIO-05. Cancellation is not observed during the journal read, the delta refresh, or the cache write; fallback is not gated on cancel

- Topic: T3 (Crash safety, resume, writer lifecycle, locking, cancellation); fix in Part 2, section [T3](#t3-crash-safety-resume-writer-lifecycle-inter-process-locking-and-cancellation)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: concurrency
- Confidence: High
- Location: crates/backend-windows/src/enumerator.rs:221-242 (load_cache_plan, refresh_cached, store_cache), 171-183 (fallback branch); crates/backend-windows/src/usn.rs:263-291 (read_journal loop)
- Problem: `cancel` is polled only inside `read_tree` and `emit_tree`. `read_journal` (up to `MAX_READ_ROUNDS`), `apply_delta`, and `store_cache` (writes a whole-volume file) run to completion after Ctrl-C. The `MftFailure::Fallback` arm starts the walker without checking `self.cancel`. The core visitor checks cancel on the first item, so the walker stops soon, but it still starts.
- Impact: Ctrl-C latency of seconds on large volumes with the cache on. A cancelled scan can also write a fresh cache file and print a "fell back" notice, which contradicts the documented "a cancel never triggers the fallback walker".
- Suggested direction: check `cancel.is_cancelled()` before `store_cache` and at the top of the `Fallback` arm (return `CoreError::Cancelled`), and pass a cancel closure into `read_journal` and `apply_delta`.

### CORE-19. `FsType::parse` mislabels common filesystem names

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: cross-platform
- Confidence: Medium
- Location: crates/core/src/model.rs:243-254, crates/backend-generic/src/volume.rs:170
- Problem: `"hfs"` maps to `Apfs`. macOS `f_fstypename` reports FAT as `"msdos"`, which falls to `Other`. Linux's `"ntfs3"` and `"fuseblk"` (ntfs-3g) also fall to `Other`. The type is only a label today; grep shows no behavioural use.
- Impact: `db info` shows wrong labels. Future logic keyed on `FsType` (reflink, timestamp granularity) would misbehave.
- Suggested direction: Add an `Hfs` variant or map hfs to `Other`. Map `msdos` to Fat32 and `ntfs3` to Ntfs.

### GEN-11. Windows ctime is creation time, so the during-read change check is weaker there

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Related: PIPE-03
- Severity: Low
- Category: cross-platform
- Confidence: High
- Location: crates/backend-generic/src/meta.rs:173-176; consumer crates/core/src/pipeline/pass_prefix.rs:251-257
- Problem: On Unix `ctime` is inode change time, which user space cannot set. On Windows the backend stores `created()`, which does not change on write. `meta_matches` compares ctime to detect a file changing during a read, and that protection is missing on Windows when a writer restores mtime and keeps the size. NTFS has a real change time (`FILE_BASIC_INFO.ChangeTime`, also in `$STANDARD_INFORMATION`).
- Impact: Rare: an in-place rewrite with preserved size and mtime during hashing goes undetected on Windows. Side note for the caller: CLAUDE.md says rescan hash reuse checks ctime, but pass_enumerate.rs:219-222 compares only size, mtime and file_id.
- Suggested direction: use `GetFileInformationByHandleEx(FileBasicInfo).ChangeTime` here and the MFT change time in backend-windows, or document that `ctime_ns` means creation time on Windows.

### GEN-12. Time conversion edge cases: overflow and inconsistent pre-epoch handling

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Also reported as: WIO-09 (backend-windows Win32 I/O, Low)
- Severity: Low
- Category: correctness
- Confidence: High
- Location: crates/backend-generic/src/meta.rs:158-171
- Problem: `md.ctime().saturating_mul(1_000_000_000) + md.ctime_nsec()` saturates and then adds, which overflows (panic in debug, wrap in release) for ctime values beyond about year 2262. `d.as_nanos() as i64` wraps to a negative value for mtimes past 2262. Pre-1970 mtimes become 0 (`duration_since` fails), while Unix pre-1970 ctimes stay negative.
- Impact: A crafted or corrupt timestamp (archive extraction, FAT image) can panic a debug build or store a negative mtime. Every pre-1970 file shares mtime 0, which weakens the size-and-mtime change checks for those files.
- Suggested direction: use `checked_mul`/`checked_add` and `i64::try_from(as_nanos())`, and represent pre-epoch times as signed nanoseconds on every platform (keep backend-windows filetime.rs in step).
- WIO-09 adds:
  - Location: crates/backend-windows/src/filetime.rs:17-21, 26-28; crates/backend-generic/src/meta.rs:104-105
  - Problem: `filetime_to_unix_ns` returns `None` (so mtime 0) when nanoseconds overflow `i64`. The generic walker does `d.as_nanos() as i64`, which truncates a `u128` and yields a wrapped, possibly negative value. The module doc claims the two match.

### GEN-13. Plain File::open follows symlinks and blocks on FIFOs after a swap since enumeration

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Low
- Category: correctness
- Confidence: High
- Location: crates/backend-generic/src/reader.rs:228, 234; crates/backend-generic/src/extents.rs:56
- Problem: Files are opened with `File::open` (O_RDONLY, follows symlinks, blocking). If a path enumerated as a regular file is replaced by a FIFO before hashing, `open` blocks until a writer appears. The post-read `meta_matches` (lstat) catches a symlink swap, but only after the target was read.
- Impact: A hash worker hangs forever and Ctrl-C cannot interrupt the blocked `open`. Rare, but a race-only hang.
- Suggested direction: on Unix open with `O_NOFOLLOW | O_NONBLOCK` (via `OpenOptionsExt::custom_flags`), then `fstat` and reject anything that is not `S_ISREG`.

### GEN-14. macOS mount_root stops at firmlinks, not at the real volume root

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Low
- Category: cross-platform
- Confidence: Low
- Location: crates/backend-generic/src/volume.rs:240-251, 373-406
- Problem: `mount_root` climbs while `st_dev` stays the same. On macOS `/Users` (a firmlink into the Data volume) has a different `st_dev` from `/`, so the "mount" for `/Users/me` is `/Users`, while the same files reached through `/System/Volumes/Data/Users/me` get mount `/System/Volumes/Data`. Volume-attribute getattrlist calls also need the true volume root (see GEN-02).
- Impact: The same volume can be stored with two mount paths depending on how the root was spelled, which breaks folder resolution in lookup.
- Suggested direction: use `statfs.f_mntonname` (already fetched) as the mount root on macOS instead of climbing by dev.

### GEN-16. EINTR from positional reads is not retried

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Low
- Category: error-handling
- Confidence: Medium
- Location: crates/backend-generic/src/reader.rs:206-223; consumer crates/core/src/hash.rs:159-169
- Problem: `read_at`/`seek_read` can return `ErrorKind::Interrupted` (NFS with `intr`, FUSE, CIFS, or a signal arriving during the read). The backend passes it through and `read_full` stops on the first error, so the file is recorded as an IO error.
- Impact: Spurious per-file errors (state -2) on network or FUSE volumes during Ctrl-C handling or other signals.
- Suggested direction: loop on `Interrupted` inside `FsHandle::read_at`, the way `std::io::Read::read_exact` does.

### GEN-17. Unsafe blocks in volume.rs lack SAFETY comments; fsid read via transmute

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Low
- Category: unsafe-soundness
- Confidence: High
- Location: crates/backend-generic/src/volume.rs:378, 379, 391, 419-427 (macOS), 482-484, 490-496, 540-551 (Windows)
- Problem: The rest of the crate documents every `unsafe`, but these blocks have no `// SAFETY:` argument. `transmute::<libc::fsid_t, [i32; 2]>` is sound (both are 8 bytes and libc's field is private), but it is the least explicit option. The workspace lints do not enable `clippy::undocumented_unsafe_blocks`, so nothing flags these.
- Impact: No UB found. The buffer lengths passed match the allocations (261 and 1024). This is a maintenance and audit risk only.
- Suggested direction: add SAFETY comments, and enable `clippy::undocumented_unsafe_blocks` in `[workspace.lints.clippy]`.

### GEN-18. Windows MAX_PATH-sized buffers for volume paths

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Low
- Category: cross-platform
- Confidence: Medium
- Location: crates/backend-generic/src/volume.rs:481, 489, 535, 539; crates/backend-generic/src/lib.rs:155-158
- Problem: `GetVolumePathNameW` output buffers are 261 or `MAX_PATH` units. A volume mounted on a folder deeper than 260 characters fails (`ERROR_FILENAME_EXCED_RANGE`). `identify` then returns `Err` and the scan aborts, while `volume_root` returns `None` (probe unknown, no extents).
- Impact: Rare, but a hard scan failure for deep folder mounts.
- Suggested direction: size the buffer from the input path length plus 1, or retry with a larger buffer.

### GEN-19. WTF-8 decode silently drops or rewrites malformed input; Windows mount converted lossily

- Topic: T4 (File and volume identity across platforms); fix in Part 2, section [T4](#t4-file-identity-and-volume-identity-across-platforms)
- Reviewed slice: backend-generic
- Severity: Low
- Category: data-integrity
- Confidence: High
- Location: crates/backend-generic/src/codec.rs:105-146; crates/backend-generic/src/volume.rs:516-522
- Problem: `decode` stops at a truncated sequence (`break`), skips invalid lead bytes, ignores continuation-byte validity, and maps overlong 4-byte forms through `saturating_sub` to arbitrary surrogates. A corrupted `RawPath` from the DB decodes to a different valid path with no error. Separately, the Windows mount is built with `String::from_utf16_lossy` and then the codec, which bypasses the lossless WTF-16 to WTF-8 path the codec exists for.
- Impact: Silent path corruption only if DB bytes are damaged. A lossy mount only matters for mount folders containing unpaired surrogates.
- Suggested direction: make decode strict (return `Result`, or at least `tracing::warn!`), and build the mount with `OsString::from_wide` plus `codec.to_raw`.

### DB-17. SqliteIndex's Index impl is unused, prefix-only, and swallows errors

- Topic: T5 (Lookup command and lookup --insert); fix in Part 2, section [T5](#t5-the-lookup-command-and-lookup---insert)
- Reviewed slice: core db/index
- Severity: Low
- Category: api-design
- Confidence: High
- Location: index/sqlite.rs:44-62, index/sqlite.rs:39-41, db/queries.rs:398-424, index/mod.rs:15-26
- Problem: Production only calls `SqliteIndex::prefix_singletons` (pass_prefix.rs:113). `Index::insert` always queries prefix partners, although the trait docs say it serves `prefix_map` and `full_map`. `Index::singletons` turns a query error into an empty iterator. `prefix_groups` is documented as a "streamed grouped cursor ... bounded by group count" but collects every row into a `Vec`.
- Impact: No production misbehavior today, but any future caller of `Index` in the full pass or of `singletons()` gets wrong partners or silently no singletons.
- Suggested direction: Remove the trait impl (or make `singletons` return `Result`), and fix the doc comments.

### MFT-10. Cache file is unauthenticated input to an elevated process

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Low
- Category: data-integrity
- Confidence: Medium
- Location: crates/backend-windows/src/mft_cache.rs:6-8,242-250,493-499
- Problem: The cache lives in `%LOCALAPPDATA%\dedupe\mft` (or `DEDUPE_CACHE_DIR`), writable by the same user's unelevated processes. BLAKE3 over the body is an integrity check, not a MAC, so anyone who can write the file can produce a valid one with arbitrary records.
- Impact: Unelevated code can make the elevated scan hide chosen files or inject fake paths into the index. v1 never deletes, so impact is limited to wrong reports.
- Suggested direction: Store the cache under an admin-only ACL (e.g. ProgramData with restricted DACL) or key the hash with a secret only readable elevated; document the trust model.

### MFT-12. Scan root inside the reserved metafile range or not a directory returns an empty success

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Low
- Category: error-handling
- Confidence: High
- Location: crates/backend-windows/src/tree.rs:137-140,204-210,273-276
- Problem: `push` drops records 0..=23 except 5, and `emit_items` returns `Ok(())` with no items when the root record is missing or not `Directory`. There is no error or notice.
- Impact: Scanning a root whose record is filtered or classified non-directory (see MFT-04/MFT-05) -> zero items, scan reports success, and the rescan stale-row logic can delete the previously indexed subtree.
- Suggested direction: Return a distinct error (mapped to `MftFailure::Fallback`) when the root record is absent or not a directory.

### MFT-14. Redundant namespace pre-read and misleading module doc

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Low
- Category: docs
- Confidence: High
- Location: crates/backend-windows/src/records.rs:12,264,275-286; ntfs-0.4.0/src/structured_values/file_name.rs:86-107,259-268
- Problem: The doc says "NtfsFileName::namespace calls unwrap on the namespace byte. The byte is checked first." But ntfs 0.4.0 already rejects unknown namespaces in `NtfsFileName::new` via `validate_namespace` (UnsupportedFileNamespace), so `structured_value` cannot produce a value whose `namespace()` panics. The extra `attribute.value(fs)` + seek + read per name is redundant work on every record.
- Impact: Extra reads per name at 10M+ records; doc misleads future maintainers.
- Suggested direction: Use `name.namespace()` after `structured_value` and map it to `NameSpace`; fix the doc.

### MFT-15. `in_use` field is always true from extract_record; base reference re-read from disk

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Low
- Category: idiom
- Confidence: High
- Location: crates/backend-windows/src/records.rs:113-118,137,180-198; crates/backend-windows/src/tree.rs:67,138
- Problem: `extract_record` returns `Ok(None)` for not-in-use records, so `RecordFacts.in_use` is always `true` and the `!facts.in_use` filter in `push` is dead outside tests; the cache stores a flag bit for it. `base_reference_of` issues a fresh seek+read for 8 bytes the crate already holds in memory (only because the field is private).
- Impact: Minor complexity and an extra seek per record (cheap only because of the chunk cache; see MFT-01).
- Suggested direction: Drop `in_use` from `RecordFacts` (or document it as test-only), and read the base reference from the same buffer if a raw-record path is introduced for MFT-01.

### MFT-16. Unused public `encode`/`write_atomic` with `expect` in non-test code

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Severity: Low
- Category: api-design
- Confidence: High
- Location: crates/backend-windows/src/mft_cache.rs:109-124,389-391
- Problem: `encode` and `write_atomic` are `pub` but called only from tests (enumerator uses `write_atomic_stream`). `encode` contains a non-test `.expect(...)`. The invariant holds today, but it is a panic site in library code.
- Impact: None at runtime today; API surface and a latent panic.
- Suggested direction: Make them `#[cfg(test)]` or `pub(crate)`, or return `Result`.

### MFT-17. lib.rs module doc lists the wrong set of Win32 modules

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows MFT
- Also reported as: WIO-13 (backend-windows Win32 I/O, Low)
- Severity: Low
- Category: docs
- Confidence: High
- Location: crates/backend-windows/src/lib.rs:5-7
- Problem: Says Win32 calls are confined to `caps`, `volume_io` and `enumerator`; CLAUDE.md and the code also put them in `handle`, `overlapped` (and `usn` issues ioctls through `RawVolume`).
- Impact: Misleads reviewers auditing `unsafe`.
- Suggested direction: List all Win32-bearing modules, matching CLAUDE.md.
- WIO-13 adds:
  - Location: crates/backend-windows/src/lib.rs:5
  - Problem: "Win32 calls are confined to `cfg(windows)` items in `caps`, `volume_io` and `enumerator`." `handle` and `overlapped` hold the `unsafe` Win32 calls, while `enumerator` has none. CLAUDE.md lists them correctly.

### WIO-06. Shared references to OVERLAPPED and boxed buffers exist while the kernel writes them

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: unsafe-soundness
- Confidence: Medium
- Location: crates/backend-windows/src/overlapped.rs:187, 218 (`wait_for(handle, &s.overlapped)`), 282 (`overlapped: &OVERLAPPED`), 45-52 (Box fields)
- Problem: `wait_for` takes `&OVERLAPPED` while a read is pending, and the kernel updates `Internal`/`InternalHigh` during the call. A `&T` to memory mutated with no `UnsafeCell` breaks Rust's aliasing rules: rustc marks `&T` parameters `noalias readonly`. `cancel_all` and `submit` also form `&mut Slot` (and so `&mut Box<..>`) over in-flight slots. No miscompile is reachable today, because nothing reads the struct after the FFI call in `wait_for`, but the SAFETY comments claim more than holds.
- Impact: latent UB under the Rust memory model. A future refactor that reads `overlapped.Internal` after the wait, or inlines differently, could observe stale values.
- Suggested direction: store `Box<UnsafeCell<OVERLAPPED>>` and the buffer as a raw `NonNull<[u8]>` (or `Box<[UnsafeCell<u8>]>`), and pass `*const OVERLAPPED` to `wait_for`.

### WIO-07. Custom `OwnedHandle` forces `unsafe impl Send/Sync` and a forget/from_raw handoff

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: idiom
- Confidence: High
- Location: crates/backend-windows/src/handle.rs:103-111 (VolumeHint Send/Sync), 118-129 (SyncFile::new mem::forget + File::from_raw_handle); crates/backend-windows/src/overlapped.rs:239; crates/backend-generic/src/lib.rs:64-88
- Problem: `win_sys::OwnedHandle` wraps a raw `HANDLE` (`*mut c_void`), so it is `!Send`/`!Sync`. Each user adds its own `unsafe impl`. std already has `std::os::windows::io::OwnedHandle`, which is `Send + Sync`, has `HandleOrInvalid`/`HandleOrNull` for the two failure conventions, and converts to `File` with `File::from(owned)`.
- Impact: more unsafe surface than needed. The `forget` handoff in `SyncFile::new` is correct but fragile: a panic between the two lines is impossible today, but it is easy to break.
- Suggested direction: switch `win_sys::OwnedHandle` to std's `OwnedHandle` (or a newtype over it). The `unsafe impl Send for VolumeHint` and the forget dance then go away.

### WIO-08. `RawVolume::read` falls back to a non-sector-multiple length for buffers over 4 GiB

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: correctness
- Confidence: High
- Location: crates/backend-windows/src/volume_io.rs:341
- Problem: `let want = u32::try_from(chunk.len()).unwrap_or(u32::MAX);` gives 4_294_967_295, which is not a multiple of any sector size. A raw volume rejects it with ERROR_INVALID_PARAMETER.
- Impact: unreachable today, because `SectorReader` requests at most `READ_CHUNK_BYTES`. A future caller with a huge buffer gets an opaque error instead of a split read.
- Suggested direction: clamp to `u32::MAX` rounded down to a sector multiple (for example `u32::MAX & !0xFFFF`), or document the limit.

### WIO-10. `UsnError::Malformed` reports "journal ended before end_usn" and "round cap reached"

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: error-handling
- Confidence: High
- Location: crates/backend-windows/src/usn.rs:280-291
- Problem: a journal that stops returning records short of `end_usn`, and hitting `MAX_READ_ROUNDS`, both return `Malformed`, whose message says "USN journal record is malformed".
- Impact: a misleading "MFT cache discarded: ... record is malformed" notice that points users at corruption when the real cause is lost records or a cap.
- Suggested direction: add distinct variants such as `Incomplete { reached, wanted }` and `TooManyRounds`.

### WIO-11. Capability probe opens the raw volume per root and gates on elevation before the real test

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: performance
- Confidence: Medium
- Location: crates/backend-windows/src/caps.rs:105-114, 63-77; crates/backend-windows/src/enumerator.rs:139, 210
- Problem: `probe_capabilities` opens `\\.\X:` and runs `IOCTL_DISK_GET_LENGTH_INFO`, closes it, and `walk_mft_volume` immediately opens it again. This repeats for each root on the same volume. `choose_mode` rejects on `!admin` even when `raw_volume_openable` is true. Whether any unelevated principal can open the volume is not verified, so this is Low confidence on the functional side.
- Impact: extra opens and probes per root (minor). A possible false "not running as administrator" for a principal that can read the volume.
- Suggested direction: return the opened `RawVolume` from the probe for reuse, and treat `raw_volume_openable` as decisive, keeping `admin` only to pick the message.

### WIO-12. Overlapped file length is fixed at open, so the verify EOF probe cannot see growth

- Topic: T6 (Windows $MFT fast path, MFT cache, Win32 I/O hygiene); fix in Part 2, section [T6](#t6-windows-mft-fast-path-mft-cache-and-win32-io-hygiene)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: correctness
- Confidence: High
- Location: crates/backend-windows/src/readahead.rs:81-83; crates/backend-windows/src/overlapped.rs:269-270; crates/core/src/pipeline/pass_verify.rs:321-329
- Problem: `ReadAhead::read_at` returns `Ok(0)` for any `offset >= file_len` without issuing I/O, and `file_len` is the size at open. `verify_pair` relies on a 1-byte read at `size` to detect a file that grew. On the overlapped path that probe never touches the disk.
- Impact: growth after open is invisible to the probe. The later `unchanged()` metadata check catches it, so there is no wrong result today, but the probe is a no-op on this backend, unlike the sync path.
- Suggested direction: on a read at or past `file_len`, issue one real read (or refresh the length with `GetFileSizeEx`) before returning 0.

### CLI-13. `--progress always` cannot draw when stderr is not a TTY, so the doc and the test are misleading

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: test-gap
- Confidence: Medium
- Location: crates/cli/src/progress.rs:17,44,178; crates/cli/tests/progress_cli.rs:29-60
- Problem: both sinks use `ProgressDrawTarget::stderr()`. In indicatif 0.17 a `Term` draw target returns no drawable when `!term.is_term()`, so nothing is drawn when stderr is redirected, whatever `should_draw` returns. The help text says "Draw when requested, even if stderr is not a terminal". `progress_cli.rs` runs with piped stderr, so it would pass even if `should_draw` ignored `json`, which makes the test vacuous.
- Impact: users who force progress into a log file get nothing. The JSON-suppression guarantee has no effective test.
- Suggested direction: for `Always`, use `ProgressDrawTarget::term(Term::stderr(), hz)` or a custom target, or drop the claim. To confirm, check `ProgressDrawTarget::drawable` in the vendored indicatif 0.17 source. Unit-test `should_draw` wiring rather than relying on the subprocess.

### CLI-20. Size parsing via f64 rounds some exact decimal inputs down by one byte

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: correctness
- Confidence: High
- Location: crates/cli/src/size.rs:24-29 (`parse_size`)
- Problem: `mantissa * multiplier as f64` then `floor()`. For example, `1.001KB` gives 1000.9999999999999, which floors to 1000. A Python check over 0.001-4.999 KB found 48 of 4999 three-decimal inputs off by one. Fractions are also accepted with `B` (`1.5B` is 1 byte).
- Impact: `--min-wasted 1.001KB` sets a 1000-byte threshold. This is minor, but the README says "rounded down to whole bytes", which implies exact decimal math.
- Suggested direction: parse the integer and fraction parts as integers (`int * mult + frac * mult / 10^digits`) with checked arithmetic, and reject fractions for `B`.

### CLI-21. `is_drive_relative` runs on every OS and only on `--dir`

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: cross-platform
- Confidence: High
- Location: crates/cli/src/main.rs:328-350
- Problem: the check is a pure string test applied on Linux and macOS, where `c:foo` is a valid relative folder name. It is applied only to `--dir`, not to scan roots, `--db`, `--output` or `lookup <FILE>`, where the same Windows drive-relative pitfall exists.
- Impact: on Unix, `dedupe report c:backup` is rejected with a Windows-specific message. On Windows, `dedupe scan d:photos` resolves against D:'s per-drive cwd with no warning.
- Suggested direction: gate the check with `cfg!(windows)` and apply it to every user path argument.

### CLI-22. `--mem-budget` is cast with `as usize`, while `--buf-size` uses `try_from`

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: idiom
- Confidence: High
- Location: crates/cli/src/scan.rs:126-129 (contrast with :76-83)
- Problem: `args.mem_budget.map(|b| b as usize)` truncates silently on 32-bit targets.
- Impact: `--mem-budget 5GiB` on a 32-bit build becomes 1 GiB with no error. This is inconsistent with the `--buf-size` handling a few lines above.
- Suggested direction: use `usize::try_from(b)` with the same context message.

### CLI-23. `BarSink` has no `Drop` cleanup, and it is built for `--no-db` too

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: correctness
- Confidence: Medium
- Location: crates/cli/src/progress.rs:36-54 (contrast with `impl Drop for ReportBar` at :198-203); crates/cli/src/scan.rs:98-115
- Problem: on error or cancel, bars still in `BarSink.bars` are dropped without `finish_and_clear`. Indicatif's default finish leaves the last frame on screen above the `Error:` line. The comment at scan.rs:98 says "Only the DB path reports progress", but `bar_sink` is created regardless of `db_path`. On Windows its `MultiProgress` is handed to `mft_observer`, so `--no-db` does draw the `$MFT` bar.
- Impact: cosmetic stale bar lines after Ctrl-C or an error. The code does not do what the comment says on Windows.
- Suggested direction: add `Drop for BarSink` that clears the remaining bars, and gate `bar_sink` on `db_path.is_some()` or fix the comment.

### CLI-24. Discovery errors lack the path and give a misleading hint

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: error-handling
- Confidence: High
- Location: crates/cli/src/discover.rs:24,40,56; crates/cli/src/main.rs:365-370
- Problem: `NotFound` says "run `dedupe scan <root>`", but `scan` puts the DB in `<root>`, not the cwd, so following the hint and rerunning `report` from the same folder fails again. `Io` prints "list database folder: <err>" without naming the folder, so `dedupe report /typo` says "No such file or directory" without saying which path.
- Impact: confusing first-run experience, which is the documented cwd versus root split, made worse by the hint.
- Suggested direction: suggest `dedupe report <root>` or `--db <root>/.dedupe.db`, and include `dir.display()` in the `Io` message.

### CORE-09. Exclude normalisation mangles legal Unix names, is lossy for non-UTF-8, and ignores `\\?\UNC\`

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: cross-platform
- Confidence: High
- Location: crates/core/src/exclude.rs:45-51
- Problem: `path.to_string_lossy().replace('\\', "/")` runs on every OS. On Unix `\` is a legal filename byte, so `/data/a\b.txt` is matched as `/data/a/b.txt`. Non-UTF-8 bytes become U+FFFD, so a pattern cannot target them, and distinct names collapse together. Only `//?/X:` is stripped, so `\\?\UNC\srv\share\x` normalises to `//?/UNC/srv/share/x` and never matches `//srv/share/**`. A relative pattern such as `node_modules` silently matches nothing, because matching is on the full path, and there is no warning.
- Impact: Excludes over-match or under-match in those edge cases. A user writing `--exclude node_modules` gets no exclusion and no diagnostic.
- Suggested direction: Rewrite `\` only for Windows-style paths (reuse `separator_for`). Map `\\?\UNC\` to `//`. Warn on patterns that cannot match an absolute path (no leading `/`, `**` or drive).

### CORE-15. `RawPath::parent` is wrong for UNC and verbatim paths

- Topic: T7 (CLI safety, path handling and output); fix in Part 2, section [T7](#t7-cli-safety-path-handling-and-output)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: cross-platform
- Confidence: High
- Location: crates/core/src/path.rs:72-83
- Problem: Only `X:\` roots are special-cased. `\\srv\share\a` walks to `\\srv\share`, then `\\srv`, then `\` (i == 1 returns `self.0[..1]`). `\\?\C:\a` walks to `\\?\C:` (drops the root separator, so it now names the device), then `\\?`, then `\`. `parent`, `component` and `is_legal_transition` have no production callers today. `join_component` is used by backend-windows/tree.rs.
- Impact: It is latent, but this is a public core API. The first caller that walks parents of a UNC or verbatim root gets nonsense ancestors.
- Suggested direction: Recognise `\\?\X:\`, `\\?\UNC\srv\share\` and `\\srv\share\` as roots (return `None` at the root and keep the trailing separator), and add tests for these shapes, or remove the unused methods.

### GEN-15. FIEMAP extent flags ignored; delalloc and inline extents report offset 0

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: backend-generic
- Severity: Low
- Category: performance
- Confidence: Medium
- Location: crates/backend-generic/src/extents.rs:76-79
- Problem: `fe_physical` is returned whatever `fe_flags` says. For `FIEMAP_EXTENT_UNKNOWN`/`DELALLOC` (recently written, not yet allocated) it is 0, for `DATA_INLINE` it is not a data location, and on btrfs it is a logical address that is not a device offset. No `FIEMAP_FLAG_SYNC` is passed.
- Impact: Only read ordering suffers: freshly written files all sort to offset 0. No correctness effect.
- Suggested direction: return `None` when `fe_flags & (UNKNOWN | DELALLOC | DATA_INLINE | NOT_ALIGNED)` is set.

### GEN-22. diskutil subprocess has no timeout

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: backend-generic
- Severity: Low
- Category: error-handling
- Confidence: Low
- Location: crates/backend-generic/src/probe.rs:110-115
- Problem: `Command::output()` waits indefinitely. `diskutil` talks to diskarbitrationd and can stall while a disk spins up or diskarbitrationd is wedged.
- Impact: `scan` hangs before enumeration with no progress output. Not observed, only plausible.
- Suggested direction: spawn, then wait with a deadline (poll `try_wait` for about 5 s) and kill on timeout, returning `None`.

### PIPE-20. Progress `Sink` bound and per-row lock

- Topic: T8 (Performance and memory at scale); fix in Part 2, section [T8](#t8-performance-and-memory-at-scale-1m-10m-files)
- Reviewed slice: core pipeline
- Severity: Low
- Category: idiom
- Confidence: High
- Location: crates/core/src/progress.rs:22-27, :51-55; pass_prefix.rs:384; pass_full.rs:129, :261
- Problem: `Sink: Send + Sync` takes `&mut self` and always sits behind a `Mutex`, so `Sync` is not needed and only narrows which types can implement it. `emit_advance` locks the mutex once per row, and a poisoned lock silently disables progress.
- Impact: minor overhead at 1M+ rows. The trait is harder to implement for `!Sync` UI types.
- Suggested direction: drop the `Sync` bound, and batch advances (as `pass_enumerate` already does with `PROGRESS_CHUNK`).

### CLI-15. deny.toml does not enforce its "no git dependencies" claim

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: build-config
- Confidence: High
- Location: deny.toml:5, no `[sources]` section
- Problem: the header says no git dependencies, but without a `[sources]` section `unknown-git` and `unknown-registry` default to `warn`. `MPL-2.0` (weak copyleft) is allowed without a note on why.
- Impact: a git or alternate-registry dependency passes the policy with only a warning.
- Suggested direction: add `[sources] unknown-registry = "deny"`, `unknown-git = "deny"`, and document the MPL exception.

### CLI-16. Workspace lints do not enforce `unsafe_code`, and `perf` is redundant

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: build-config
- Confidence: High
- Location: Cargo.toml:20-22; crates/cli/src/main.rs (no crate attribute); bench/src/main.rs; xtask/src/main.rs; crates/tui/src/lib.rs
- Problem: there is no `[workspace.lints.rust]`. Only `core` (`forbid`) and `backend-windows` (`deny`) restrict `unsafe`. CLAUDE.md says unsafe belongs only in the two backend crates, but `cli`, `bench`, `xtask` and `tui` would accept it silently. `clippy::perf` is already part of `clippy::all`.
- Impact: the layering rule is enforced by convention only.
- Suggested direction: add `[workspace.lints.rust] unsafe_code = "forbid"` (backends override it with `deny` plus local `allow`), and drop `perf`.

### CLI-17. `tracing-subscriber` `env-filter` pulls in regex for a fixed level, and `RUST_LOG` is ignored

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: performance
- Confidence: High
- Location: Cargo.toml:33; crates/cli/src/main.rs:420-434
- Problem: `with_env_filter(filter)` is given a constant level string. The `env-filter` feature brings in `regex`/`regex-automata` and `matchers`. Because the filter is built from the string, `RUST_LOG` has no effect.
- Impact: larger binary and longer compile for no gain. Per-module filtering such as `RUST_LOG=dedupe_core::pipeline=trace` is not available.
- Suggested direction: use `with_max_level(LevelFilter)` and drop the feature, or honour `RUST_LOG` when it is set.

### CLI-18. kill_fuzz cannot diagnose failures and compares unnormalized output

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: test-gap
- Confidence: High
- Location: crates/cli/tests/kill_fuzz.rs:2,4,37-40,41-49,86-94
- Problem: the doc says "normalized report", but the raw JSON strings are compared. The resume result is discarded (`let _ = scan(...)`), and its stderr goes to `Stdio::null()`. `report()` ignores the exit status. The doc says "Unix-only", but there is no `#[cfg(unix)]`. With a 100-300 ms kill window and 576 MiB of data, a fast NVMe machine with 8 hash workers can finish before the kill, which weakens the test.
- Impact: a failing iteration shows only "resume diverged" with two JSON blobs and no stderr from the resume or report. A resume error (for example `NothingToResume` when the kill lands before the run row is committed) looks like a convergence bug.
- Suggested direction: capture stderr and assert the resume status (allowing only "nothing to resume" when the first scan completed). Record whether the child exited before `kill`. Fix the doc or add the cfg.

### CLI-25. Bench: "sparse" query file is not sparse on NTFS/exFAT, and the percentile sort uses `partial_cmp().unwrap()`

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: cli/workspace
- Severity: Low
- Category: cross-platform
- Confidence: Medium
- Location: bench/src/main.rs:9,160-163,244
- Problem: `File::set_len(10 GiB)` creates a sparse file on ext4, APFS and XFS. On NTFS (without `FSCTL_SET_SPARSE`) and on exFAT/FAT it reserves real clusters. `sort_by(|a, b| a.partial_cmp(b).unwrap())` is the pre-`total_cmp` idiom.
- Impact: running the bench on Windows reserves 10 GiB of real disk in the temp folder, contrary to the "no real disk use" comment, and can fail on small system drives.
- Suggested direction: note the platform caveat or mark the file sparse on Windows. Use `f64::total_cmp`.

### CORE-10. `rescan_reuses_hashes_and_removes_deleted` relies on filesystem timestamp granularity

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: test-gap
- Confidence: Medium
- Location: crates/core/tests/phase1.rs:469-472
- Problem: The test rewrites `a.txt` with the same length and expects a re-read. Reuse is keyed on size, mtime, ctime and file id. The rewrite keeps the inode and the size, so the only change signal is mtime and ctime. On a filesystem with coarse timestamps (FAT/exFAT 2 s, HFS+ 1 s, some network or 9p mounts) all of them can be unchanged, and the stored hash is reused. `verify.rs:385` forces `set_modified(now + 3600s)` for exactly this reason, but this test does not.
- Impact: Intermittent failure ("modified file must be read again") when TMPDIR is on a coarse-timestamp filesystem. The assertion also checks only `opens() > 0`, not that the stored hash or the report changed.
- Suggested direction: Set mtime explicitly after the rewrite, as in verify.rs. Also assert that the new `full_hash` differs and that a.txt left its old group.

### CORE-12. `VerifyMismatch` is untested, and the resume test only checks convergence

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: test-gap
- Confidence: High
- Location: crates/core/tests/verify.rs:394-405, crates/core/src/pipeline/pass_verify.rs:211, crates/core/tests/phase1.rs:349-367
- Problem: No test reaches `ErrorKind::VerifyMismatch` (the only reference in tests is the comment at verify.rs:397 calling it unreachable in that scenario). That needs a same-size, same-mtime, different-content pair with an equal stored hash, which a fake reader can produce. `resume_at_every_boundary_converges` asserts only that the final report JSON equals a fresh scan's. A resume that silently restarts from scratch would pass, and the test does not use `reader.opens()` to show that completed passes were skipped.
- Impact: Byte-verify's core purpose (catching a hash collision or a stored-hash lie) has no regression guard. Resume could degrade to a full rescan unnoticed.
- Suggested direction: Add a fake-reader test where stored hashes match but the bytes differ, and assert state -2 with kind 5. In the resume test, assert that the open count after resuming at phase >= FullDone is zero.

### CORE-14. The `Hasher` trait and `Config.hash_algo` are decoupled from the actual hashing

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Also reported as: PIPE-18 (core pipeline, Low)
- Severity: Low
- Category: api-design
- Confidence: High
- Location: crates/core/src/traits.rs:144-155, crates/core/src/pipeline/mod.rs:36, crates/core/src/pipeline/naive.rs:201, crates/core/src/hash.rs:17-29, crates/core/src/config.rs:35
- Problem: `Services.hasher` is never read (naive.rs:201 has `let _ = hasher;`). `hash_prefix_target` and `hash_full_target` call `blake3::Hasher` directly. `Config.hash_algo` is a free `String` that only feeds the meta guard.
- Impact: A test double passed as `hasher` has no effect. Setting `hash_algo` to another value would label BLAKE3 output with the wrong algorithm in `meta`. The trait promises pluggability that does not exist.
- Suggested direction: Either route hashing through the trait or remove it from `Services`. Make `hash_algo` an enum or a constant derived from the hasher.
- PIPE-18 adds:
  - Location: crates/core/src/pipeline/mod.rs:36 (`Services.hasher` never read by any pass); naive.rs:426, :559; hash.rs:180-202 (`prefix_bytes_matches`, `config_fingerprint`, `_assert_error_type` have no callers)
  - Problem: every hash path calls `blake3` directly, so the `Hasher` trait described as "allows a test double" (traits.rs:143) has no effect. `cfg.hash_algo` is a free string stored in meta, while hashing is always blake3-128. `naive.rs:426` says `codec` is unused, but line 490 uses it.

### CORE-16. Error type loses context and uses `Other` for argument errors; row decoding silently substitutes values

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Related: DB-18
- Severity: Low
- Category: error-handling
- Confidence: High
- Location: crates/core/src/error.rs:11-12, crates/core/src/exclude.rs:28,33, crates/core/src/report.rs:128, crates/core/src/lookup.rs:222, crates/core/src/db/queries.rs:625,707
- Problem: `CoreError::Io(#[from] std::io::Error)` carries no path, so `lookup` on a missing file reports `lookup failed: io: No such file or directory (os error 2)` with no path. A bad `--exclude` pattern maps to `CoreError::Other` rather than the existing `InvalidArg`. Decoding maps an unknown state integer to `State::Error` and a wrong-length hash blob to `Hash([0;16])`, both silently. The zero hash can merge unrelated corrupt rows into one group in `for_each_group` pairing.
- Impact: Diagnostics are hard to act on, and corrupt rows are masked instead of reported.
- Suggested direction: Add an `Io { path, source }` variant, or use `.map_err` with the path at the call sites. Use `InvalidArg` for user input. Surface decoding failures as `InvalidState`, or skip the row with a warning.

### CORE-17. `Config` and `Profile` have no validation entry point; lookup ignores the DB's stored hash config

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Severity: Low
- Category: api-design
- Confidence: High
- Location: crates/core/src/config.rs:17-68, crates/core/src/storage_profile.rs:51-59, crates/cli/src/lookup.rs:18, crates/core/src/lookup.rs:91-92
- Problem: Every field is `pub` and nothing validates it. `prefix_total()` is a plain `u64` add (it panics in debug and wraps in release if both are huge). `prefix_first_bytes = 0` and `prefix_last_bytes = 0` turn the prefix pass into a no-op. `Profile { queue_depth: 0, buf_size: 0, .. }` bypasses `resolve`'s `non_zero` check: buf_size 0 is silently clamped to 1-byte reads in `hash_full_target`. `lookup` uses `Config::default()` prefix sizes and never compares them with `meta.prefix_first_bytes`/`prefix_last_bytes`, unlike `resume::check_config_guard`.
- Impact: Today the CLI exposes no prefix flags, so the risk is latent. Once prefix sizes become configurable, lookup would compare stored prefixes computed with other sizes and return a false `NoMatch`.
- Suggested direction: Add `Config::validate()` (with `checked_add`) and call it in `run_scan` and `lookup`. Have lookup read the prefix sizes from `meta`.

### CORE-18. Doc comments contradict behaviour (`read_range`, `StorageProbe`, CLAUDE.md on byte verify)

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core model/lookup/report
- Related: CLI-19
- Severity: Low
- Category: docs
- Confidence: Medium
- Location: crates/core/src/traits.rs:93-99, crates/core/src/traits.rs:189-193, crates/core/src/storage_profile.rs:123-126, CLAUDE.md (Gotchas: "Byte verify has no automated end-to-end test yet"), crates/core/tests/verify.rs:345-485
- Problem: The `read_range` doc says "Read exactly the requested bytes (may return fewer at EOF)", but the body makes one `read_at` call, which may legally return a short read before EOF. The method is also unused. The `StorageProbe` doc says unknown means "callers assume SSD", but `resolve` maps unknown to `StorageKind::Unknown` with depth 2. CLAUDE.md says byte verify has no automated end-to-end test, yet verify.rs drives `run_scan` with `VerifyMode::Byte` through 5 tests. Confidence is Medium because CLAUDE.md may mean CLI-level end-to-end.
- Impact: The docs mislead future callers and reviewers. A future caller of `read_range` could hash a short read as if it were the whole range.
- Suggested direction: Make `read_range` loop like `hash::read_full`, or delete it. Fix the probe doc. Narrow the CLAUDE.md statement to "no CLI-level test".

### DB-18. Corrupt or unexpected column values are silently coerced

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core db/index
- Related: CORE-16
- Severity: Low
- Category: error-handling
- Confidence: High
- Location: db/queries.rs:34 (`State::from_i64(..).unwrap_or(State::Error)`), 331-349 (rows_with_prefix drops bad hashes), 411, 625, 703 (`unwrap_or(Hash([0; 16]))`)
- Problem: An unknown state becomes `Error`; a wrong-length hash blob becomes the all-zero hash or is dropped.
- Impact: Corruption is hidden instead of reported. Two bad rows in `list_groups` or `volume_group_members` both map to the zero hash and pair with each other in the report merge.
- Suggested direction: Return a `rusqlite::Error::FromSqlConversionFailure` (or a `CoreError::Corrupt`) instead of defaulting.

### GEN-20. Minor idiom issues in non-test code

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: backend-generic
- Severity: Low
- Category: idiom
- Confidence: High
- Location: crates/backend-generic/src/volume.rs:440-441 (`char::from_digit(..).unwrap()`), 398 (`let _ = &st;` no-op), 428 (dead `buf.len() < 36`); crates/backend-generic/src/meta.rs:86 (`#[allow(unused_mut, unused_assignments)]`); crates/backend-generic/src/volume.rs:353-364 (`f_type as i64` sign-extends on 32-bit, so `BTRFS` 0x9123683E never matches there; vfat 0x4d44, exfat 0x2011BAB0 and ntfs3 map to `Other` although `FsType` has variants for them)
- Problem: The unwraps cannot fail but break the no-panic norm. The allow attribute hides the cfg-dependent assignment pattern instead of using per-OS helper functions that return the values.
- Impact: Style and maintainability; fs_type is informational only.
- Suggested direction: use `format!("{byte:02x}")`, remove the dead code, split meta per-OS into `fn os_bits(md) -> (Option<Vec<u8>>, i64, i64)`, and compare magic as `u32`.

### GEN-21. Test gaps: walker, meta conversion and Linux/macOS volume identity have no tests

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: backend-generic
- Severity: Low
- Category: test-gap
- Confidence: High
- Location: crates/backend-generic/src/walk.rs, meta.rs, volume.rs (only `fs_type_from_wide` tested)
- Problem: Nothing covers exclude pruning at depth > 0, symlink-not-followed behaviour, dir-before-children order, `file_meta_from` kind and attrs mapping, `mount_root` with a relative input, or the macOS getattrlist parse (GEN-02 would have shown up as a unit-tested parse of a known reply buffer).
- Impact: Regressions in the walker contract (`Enumerator` docs) and the bugs above go unnoticed.
- Suggested direction: tempdir-based walker tests on Unix, and pure-function extraction of the getattrlist reply parse with a fixture.

### PIPE-19. `first_extent_offset` returns `Result`, but an `Err` aborts the whole pass; the soft-fail contract lives only in the backends

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core pipeline
- Severity: Low
- Category: api-design
- Confidence: High
- Location: crates/core/src/pipeline/pass_prefix.rs:349-354; pass_full.rs:64-69; scheduler.rs:418-421; traits.rs:124-128; backend-generic/src/reader.rs:72-84
- Problem: `Scheduler::order` propagates the first key error, so on HDD one vanished or unreadable file would abort the pass. Today both backends map errors to `Ok(None)`, but the trait does not document this. The same closure also turns a `PathCache` failure into a fatal error. `PathCache::dir` (mod.rs:80) uses `expect("just inserted")` where the entry API would need none.
- Impact: a future backend that returns `Err` would make HDD scans fail on the first deleted file.
- Suggested direction: document "must not fail for per-file reasons", or treat `Err` as `None` in the key closure, and use the `entry` API in `PathCache`.

### PIPE-21. 128-bit truncation is not documented as a trust boundary when verify is off

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: core pipeline
- Severity: Low
- Category: docs
- Confidence: High
- Location: crates/core/src/hash.rs:23-29; crates/core/src/pipeline/scan.rs:223-229 (verify off promotes 3 -> 5)
- Problem: groups are keyed on BLAKE3 truncated to 128 bits plus size. Accidental collisions are negligible (birthday bound about 2^64), but a deliberately crafted colliding pair costs about 2^64 work, which is within reach of a well-funded attacker. No user-facing doc tells users to pass `--verify byte` before acting on results from untrusted content.
- Impact: none today, because v1 deletes nothing. It matters once a delete or link action exists.
- Suggested direction: document it, and make byte-verify the default for any future destructive action.

### WIO-14. Windows tests do not prove the id path or cancellation is exercised

- Topic: T9 (Build config, error types, dead code, docs, test gaps); fix in Part 2, section [T9](#t9-build-config-error-types-dead-code-docs-and-test-gaps)
- Reviewed slice: backend-windows Win32 I/O
- Severity: Low
- Category: test-gap
- Confidence: High
- Location: crates/backend-windows/tests/handle_reader.rs:160-180, 136-158
- Problem: `handle_reader_open_should_match_path_bytes_when_opened_by_file_id` passes even if `OpenFileById` fails and the reader silently falls back to the path, because the path is valid. `overlapped_file_should_not_hang_when_dropped_with_reads_in_flight` uses a 512 KiB cached temp file, so the reads have most likely already finished and `CancelIoEx` has nothing to cancel. There are no tests for a file that shrinks or grows under an overlapped handle, for synchronous `ERROR_HANDLE_EOF`, or for `RawVolume`/`read_journal` against a real volume.
- Impact: WIO-01 and WIO-02 class bugs, and cancel-path regressions, would pass CI-less manual runs.
- Suggested direction: rename or move the file after capturing its id, then open with a bogus path, so only the id can succeed. Add a drop test on a large uncached file (opened with `FILE_FLAG_NO_BUFFERING` for setup) and a truncate-during-read test.

---

# Part 2. Solutions by topic

Each topic below was written by a topic agent that first re-checked its findings against the current code, then designed the fix. The boilerplate is the shape of the change: file paths, signatures, types and key logic. It is not a ready-to-apply patch. Nothing in this part has been compiled. Several review line numbers had drifted, so the topic sections cite current lines.

## Re-verification outcome

None of the 125 findings was rejected. 10 are partially confirmed: the code path is confirmed, but the impact depends on OS behaviour or the finding is narrower than first reported.

| ID | Verdict | What remains unconfirmed or narrower |
|---|---|---|
| CORE-01 | Partial | Case (c), where rows disappear while a newer rescan is unfinished, is inherent and can only be mitigated (T1.3). |
| CORE-06 | Partial | Whether `GetVolumePathNameW` keeps the `\\?\` prefix for a canonicalized path. |
| PIPE-15 | Partial | The code behaviour is confirmed; the Windows impact needs a run with a root below the drive root. |
| GEN-09 | Partial | Same `\\?\` question as CORE-06. |
| GEN-14 | Partial | Only the getattrlist target is fixed. A full firmlink fix needs roots rewritten too (follow-up). |
| MFT-05 | Partial | Whether `GetVolumePathNameW` follows junctions. |
| WIO-01 | Confirmed mechanism | Whether `OpenFileById` with a truncated ReFS id can actually resolve to a different file. |
| WIO-03 | Partial | Whether raw volume reads can miss changes NTFS has cached but not yet flushed. |
| DB-10 | Partial | The lookup query really is a full table scan; the size-pass query already uses `idx_files_state`. |
| CLI-13 | Partial | The indicatif source was not read (it is outside the project), so the help-text claim is likely but unproven. |
| CORE-18 | Partial | Byte verify does run end to end through `run_scan` in `verify.rs`; only the CLI-level and `VerifyMismatch` tests are missing. |

The topic agents also found four new issues. Each is handled inside the topic noted:

- **T2.6:** The transition table forbids `PrefixHashed -> UniquePrefix` (2 -> -4), which `pass_prefix.rs:107-121` already does in production. Add it before the table is enforced.
- **T3.3:** On resume, a lone mismatched verify member left in state 3 gets promoted to a singleton.
- **T3.4:** The `--force` wipe also resets skipped (-1) rows to 0, and it is not atomic with the meta update.
- **T8, open point:** The in-memory prefix path counts already-committed -4 rows as partners, but the DB fallback `singleton_prefixes` does not. A resumed run can therefore classify differently depending on which path runs. T3.3 resolves this by moving singleton marking into one SQL statement.

## Cross-topic coordination

These fixes touch the same code. Implement each cluster as one unit or in the order given.

1. **Prefix pass (`pass_prefix.rs`): T3.3, T4.3, T8.4, T8.5.**
   - **Order:** T3.3 first, then T8.4/T8.5 on top, then T4.3.
   - **T3.3** streams results and moves singleton marking to SQL (`Batch::MarkPrefixSingletons`).
   - **T8.4** replaces `MemoryIndex` with a sort.
   - **T8.5** moves the pass onto the shared worker pool.
   - **T4.3** counts distinct inodes.
   - **Conflict between T3.3 and T8.4:** T3.3's SQL singleton marking makes T8.4's in-memory classification mostly redundant. Settle on one before coding:
     - SQL marking (T3.3) is crash-safe and resume-consistent.
     - The sort (T8.4) bounds memory on the read side.
     - Recommendation: T3.3 for the classification, and T8.4's sort only if a memory-bounded in-process path is still needed.
   - **T4.3's DB half:** `singleton_prefixes` counting distinct inodes must be applied to the `MarkPrefixSingletons` SQL.
2. **Stale deletion: T2.4, T3.2, T8.1.**
   - **T2.4** excludes folders that failed to read.
   - **T3.2** commits `DeleteStale` in the same transaction as `PhaseComplete`.
   - **T8.1** rewrites the recursive CTE using a TEMP table.
   - Write the single SQL path (T2.5/DB-19) once with all three properties.
3. **ctime semantics: T2.3, T4.2, T6 ($MFT path).**
   - **T2.3** adds ctime to the unchanged check.
   - **T4.2** switches Windows ctime from creation time to ChangeTime.
   - The walker, `metadata()` and the `$MFT` path (`mft_record_modification_time`) must all switch in the same commit, together with the MFT cache version bump. Otherwise every Windows file looks changed during read.
   - Expect one full rehash on Windows after this lands.
4. **File identity: T4.1, T4.2, T5.1, T8.6.**
   - **T4.1** adds `FileMeta.dev` and namespaces foreign-device ids.
   - **T4.2** makes Windows identity a single per-handle call.
   - **T8.6** has the walker open each file once.
   - **T5.1** excludes hardlinks in lookup.
   - T4.2 and T8.6 change the same Windows walker function. Do them together.
5. **Inter-process lock: T3.6, T5.3, T7.3.**
   - **T3.6** defines `db::lock::acquire(path) -> Result<DbLock, CoreError>`: a sidecar `<db>.lock`, using std `File::try_lock`.
   - **T5.3** (`lookup --insert`) and T7.3 (`db drop`/`vacuum`) use it.
   - `File::try_lock` was stabilized in Rust 1.89 (unconfirmed, from memory; check the std docs). The toolchain is pinned at 1.92, so this works, but `rust-version` in the workspace must go from 1.83 to 1.89.
6. **DB identity and pragmas: T7.3, T2.8(g), T9.2.**
   - T7.3 adds `PRAGMA application_id` and `schema::classify` (Empty / Dedupe / Foreign).
   - T2.8(g) adds an IMMEDIATE transaction for creating a new DB.
   - Both edit `db/mod.rs` and `schema.rs`.
7. **Walker fallback: T2.4, T6.1.**
   - T6.1 makes any `$MFT` gap fall back to the walker.
   - T2.4 makes the walker report its own errors (`WalkItem::Error`).
   - Land T2.4 first so the fallback does not reintroduce silent drops.
8. **Version bumps.**
   - **DB schema:** stays at v2 for every fix except the optional covering index in T8.3. Hold that index for whichever change next forces a v3.
   - **MFT cache format:** goes to v2 (T6.6/T6.9). Bump it once for T6.6, T6.9 and the T4.2 ctime switch together.
   - **Delete-and-rescan policy:** the bumps add no migrations; an older DB or cache is still refused with a "delete and rescan" error.

## Recommended global order of work

1. **T1.1:** report merge (P0 DB-01). Small and self-contained.
2. **T2.1, T2.2:** folder overlay and file claims (P0 DB-02 and P1 DB-03).
3. **T3.1, T3.2, T3.4:** writer lifecycle, atomic phase marker, `--force` wipe. These make every later fix crash-safe.
4. **T3.6:** lock, then T5.1 to T5.3 (lookup).
5. **T1.2, T1.3:** run-scoped groups.
6. **T7.1, T7.2, T7.6, T7.7:** CLI output safety (P1 CLI-01 to CLI-03).
7. **T4.1, T4.4, T4.5, T4.6:** volume identity, including the macOS, Linux FUSE and Windows UNC fixes.
8. **The prefix-pass cluster:** T3.3, T8.4, T8.5, T4.3.
9. **The ctime and Windows identity cluster:** T2.3, T4.2, T8.6, then the T6 cache v2 bump.
10. **T6.1 to T6.4:** MFT fail-closed, root resolution, reparse classification, attribute-list refusal.
11. **The rest, by priority.** T9 docs and config can go in any time. T9.3 changes five test files, which under the project rules needs your approval first.

## Open decisions for the user

- **Report scope (T1):**
  - **Recommended:** keep "latest completed run of each volume, only the roots that run covered". This matches plan P1.3/P1.4 and README line 86.
  - **Alternative:** an opt-in whole-volume mode that groups across runs. This would hash files outside the roots asked for, which makes it a product change.
- **Prefix classification (cluster 1 above):** SQL marking alone, or SQL marking plus the memory-bounded sort.
- **`lookup --insert` semantics (T5.3):**
  - Rows in states -3/-4 get hashes outside a run, and are promoted to 5 only when the file is unchanged.
  - Record this rule in plan P1.4/P1.8.
  - It adds new JSON fields: `unconfirmed`, `insert` status, and the `unreadable` freshness value.
- **`--resume` with roots (T7.8):** roots become optional with `--resume`. If given, they must match the resumed run's roots as a set unless `--force` is passed.

## Behaviour to verify on real hardware before trusting the fixes

- **Windows:**
  - Whether `GetVolumePathNameW` follows junctions and keeps `\\?\`.
  - Whether `FILE_ID_INFO` on NTFS is the zero-extended file reference.
  - Whether `dwVolumeSerialNumber` is the low 32 bits of the NTFS serial.
  - `ReOpenFile` with the overlapped flag.
  - Whether raw `$MFT` reads see cached changes NTFS has not flushed.
  - What `FSCTL_IS_VOLUME_DIRTY` reports after a foreign mount.
  - What `GetVolumePathNameW` returns for UNC and subst roots.
- **macOS:** the getattrlist volume UUID layout. It was checked against the man page, not run.
- **Linux:**
  - That FUSE leaves fsid at zero.
  - The output format of `zpool list -vHPL` across OpenZFS versions.
  - The bcachefs source format in mountinfo.
  - The ntfs/ntfs3 statfs magic values.
- **Tooling and crates:**
  - That cargo-deny treats `publish = false` crates as private for `allow-wildcard-paths`.
  - That rusqlite 0.32 has `FromSqlError::InvalidBlobSize`.
  - The tracing-subscriber feature names.

## Side effects of landing the fixes

- **macOS:** existing DBs re-index each volume once after GEN-02, because the volume uuid changes.
- **Windows:** expect one full rehash after the ctime switch (cluster 3).
- **Pre-1970 files:** these rehash once after T4.8.
- **Windows open cost:** `metadata()` adds one attribute-only open per hashed file (T4.2).
- **Rescan cost:** on Unix, chmod, chown and new hardlinks now trigger a rehash (T2.3, because of the ctime check).
- **Code churn:** about 19 `FileMeta` struct literals need `dev: None` (T4.1). `lookup` changes its signature to `&mut Connection -> LookupReport` (T5).

---

## T1. Report correctness and duplicate-group statistics

**Findings covered:** DB-01, DB-05, PIPE-07, CORE-01, DB-14, DB-15, PIPE-17

**Root causes:**
- Report scope is undefined in code: `recompute_groups` aggregates every live row on the volume (no run filter), while the passes, `group_totals`, `volume_group_members` and `run_file_counts` are scoped to one run id. Stored `member_count` / `distinct_inodes` / `wasted_logical` therefore describe a different row set than the members listed -> DB-05, PIPE-07, CORE-01 (a), (b).
- The report pairs two independently ordered, independently snapshotted queries (`list_groups` by wasted bytes, `volume_group_members` by hash) with a forward merge -> DB-01.
- The end-of-scan bookkeeping (`DELETE` + `INSERT` of `dup_groups`, phase update, `finish_run`) runs as separate autocommit statements -> DB-14, and it reuses the `now` captured at scan start -> DB-15, PIPE-17 (duplicates of each other).
- A rescan resets rows to state 0 and stamps them with the unfinished run id before that run finishes -> CORE-01 (c). Inherent to in-place state reset; mitigated, not eliminated.

**Semantic decision: "the report" = the latest completed run of each volume (the roots that run covered).**

Justification:
- Plan P1.4 (`.overall_plan.md:1212`): "Report considers `state IN (3, 4, 5) AND last_seen_run_id = <latest completed run for volume>`". Plan P1.3 (`:1183`): rows not seen by the latest completed enumeration are excluded from the report. README `:86`: "Prints the duplicate groups from the last completed scan." CLAUDE.md "Reading results" says the same.
- The pipeline can only make this scope correct. Every pass selects `last_seen_run_id = run` (`pass_size.rs:18` via `rows_in_state`, `queries.rs:285-405`). A file under an unscanned root B keeps the state it reached in its own run: a B file that was `-3` (unique by size then) has no hash, so it can never join an A group, while two B files that reached state 5 in an older run would. "Whole index" groups built from those rows are a partial, order-dependent subset of the real duplicates, which is exactly the inconsistency PIPE-07 describes.
- Making a whole-index report correct means running size/prefix/full over every live row of the volume, i.e. reading files outside the roots the user asked to rescan. That contradicts the partial-root rescan contract (P1.7.1 step 2) and is a product change, not a bug fix. Listed below as a follow-up decision for the user.

So the fix is: compute `dup_groups` with the same run filter the report uses, swap it atomically with `finish_run`, and derive every number the report prints (header, per-group inode count, wasted bytes) from the one row set it lists.

**Recommended order of work:**
1. T1.2 (run-scoped, atomic recompute with a real finish timestamp). Small, unblocks consistent data.
2. T1.1 (single-query report build; removes the merge and the header/body split).
3. T1.3 (stderr note for an unfinished newer run) and README wording.

### T1.1 Build each volume's groups from one member query, compute stats from listed rows (closes DB-01, DB-05, CORE-01 (b), report half of PIPE-07)

- **Verification:**
  - DB-01 Confirmed. `list_groups` orders `wasted_logical DESC, full_hash` (`crates/core/src/db/queries.rs:618`); `volume_group_members` orders `g.full_hash, f.id` (`:697`); `for_each_group` (`crates/core/src/report.rs:215-232`) skips rows with `r.0 < g.full_hash`, so for G1 (hash 0x90, 10 MB) then G2 (hash 0x10, 1 MB), G2's rows are discarded while processing G1 and G2 fails the `>= 2` guard. The comment at `:215` is false. `stream_human`'s header (`group_totals`, `:164`) still counts G2, so header and body disagree. The two statements are also separate autocommit reads, so they can see different WAL snapshots.
  - DB-05 Confirmed. `recompute_groups` (`queries.rs:590-605`) has no `last_seen_run_id` filter; `group_totals` (`:640-655`) and `volume_group_members` (`:686-697`) filter on `run_id`; `for_each_group` prints the stored `distinct_inodes` / `wasted_logical` (`report.rs:237-238`).
  - CORE-01 Confirmed for (a) and (b) as stated. (a) is resolved by the scope decision (rows outside the latest run's roots are by definition not in the report; README wording updated in T1.3). (b) is fixed here.
  - PIPE-07 Confirmed (`pipeline/scan.rs:165`, `queries.rs:590-612` vs `:640-712`); `list_groups` indeed applies no member filter.
- **Fix:** Replace `list_groups` + `group_totals` + the merge with one query per volume that returns member rows already keyed by `(full_hash, size)`, joined to `dup_groups` only as a prefilter (`g.wasted_logical >= min_wasted` is a safe superset filter, since stats recomputed from a subset of rows can only be lower). A pure function folds consecutive rows into groups, recomputes `distinct_inodes` and `wasted_logical` from the rows themselves, applies the `> 1 inode` and `min_wasted` filters, and sorts by `(wasted desc, hash, size)`. Header totals are summed from those same folded groups before any path resolution, so header, per-group numbers and listed members cannot disagree, even during the degraded window of CORE-01 (c). Path resolution (the expensive part, `PathCache`) still streams group by group.
  - `files.file_id` is `BLOB` (`schema/v2.sql:37`) and `id` is `INTEGER`, so the existing SQL `COALESCE(file_id, id)` never collides across types. In Rust use an enum key to keep that property.
  - Join on `f.size = g.size` too: groups are keyed by `(full_hash, size)` in `recompute_groups`.
- **Rejected alternatives:**
  - Only change `volume_group_members` to `ORDER BY g.wasted_logical DESC, g.full_hash, f.id`: fixes DB-01 but keeps two snapshots and stored stats that can still disagree with the listed rows.
  - `HashMap<Hash, Vec<_>>` of members then iterate `list_groups`: fixes the pairing, same two-snapshot and stale-stat problems.
  - Drop `dup_groups` and `GROUP BY` over `files` at report time: correct but throws away the precompute that B13a relied on for large DBs.
  - Keep a separate SQL `group_totals` rewritten with the same semantics: two copies of the same rule that can drift; the Rust fold is the single source of truth.
- **Boilerplate:**

```rust
// crates/core/src/db/queries.rs  (replaces list_groups, group_totals, GroupMemberRow,
// volume_group_members; group_members has no caller outside this file and can go too)

/// Identity used to collapse hard links. `file_id` is a BLOB and `id` an INTEGER,
/// so the two variants never compare equal (same rule as `COALESCE(file_id, id)`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum InodeKey {
    FileId(Vec<u8>),
    Row(i64),
}

/// One listed member of a duplicate group, before path resolution.
pub struct GroupMemberRow {
    pub full_hash: Hash,
    pub size: i64,
    pub inode: InodeKey,
    pub folder_id: i64,
    pub name: RawPath,
}

/// Member rows of every `dup_groups` group on a volume that are live in `run_id`,
/// ordered by `(full_hash, size, file row id)` so equal keys are adjacent.
/// `min_wasted` prefilters on the stored total; callers re-check it on the listed rows.
pub fn volume_group_members(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
    min_wasted: i64,
) -> Result<Vec<GroupMemberRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT f.full_hash, f.size, f.file_id, f.id, f.folder_id, f.name \
         FROM dup_groups g \
         JOIN files f ON f.volume_id=g.volume_id AND f.full_hash=g.full_hash AND f.size=g.size \
         WHERE g.volume_id=?1 AND g.wasted_logical>=?2 \
           AND f.state IN (3,4,5) AND f.last_seen_run_id=?3 \
         ORDER BY f.full_hash, f.size, f.id",
    )?;
    let rows = stmt
        .query_map(params![volume_id, min_wasted, run_id], |r| {
            let h: Vec<u8> = r.get(0)?;
            let file_id: Option<Vec<u8>> = r.get(2)?;
            let id: i64 = r.get(3)?;
            Ok(GroupMemberRow {
                full_hash: Hash::from_slice(&h).unwrap_or(Hash([0; 16])), // existing pattern
                size: r.get(1)?,
                inode: file_id.map_or(InodeKey::Row(id), InodeKey::FileId),
                folder_id: r.get(4)?,
                name: RawPath::new(r.get::<_, Vec<u8>>(5)?),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}
```

```rust
// crates/core/src/report.rs

/// One group's rows before path resolution. Stats come from `members`, never from
/// the stored `dup_groups` columns, so header, group line and member list agree.
struct PendingGroup {
    full_hash: Hash,
    size: i64,
    distinct_inodes: i64,
    wasted_logical: i64,
    members: Vec<(i64, RawPath)>, // (folder_id, name)
}

/// Folds adjacent rows with the same `(full_hash, size)` into groups, keeps groups
/// with more than one distinct inode and at least `min_wasted` wasted bytes, and
/// orders them most-wasted first, then by hash and size.
fn fold_groups(rows: Vec<queries::GroupMemberRow>, min_wasted: i64) -> Vec<PendingGroup> {
    let mut out = Vec::new();
    let mut it = rows.into_iter().peekable();
    while let Some(first) = it.next() {
        let (full_hash, size) = (first.full_hash, first.size);
        let mut inodes = vec![first.inode];
        let mut members = vec![(first.folder_id, first.name)];
        while let Some(r) = it.next_if(|r| r.full_hash == full_hash && r.size == size) {
            inodes.push(r.inode);
            members.push((r.folder_id, r.name));
        }
        inodes.sort_unstable();
        inodes.dedup();
        let distinct_inodes = inodes.len() as i64;
        let wasted_logical = size.saturating_mul(distinct_inodes - 1);
        if distinct_inodes > 1 && wasted_logical >= min_wasted {
            out.push(PendingGroup { full_hash, size, distinct_inodes, wasted_logical, members });
        }
    }
    out.sort_by(|a, b| {
        b.wasted_logical
            .cmp(&a.wasted_logical)
            .then_with(|| a.full_hash.cmp(&b.full_hash))
            .then_with(|| a.size.cmp(&b.size))
    });
    out
}

/// One volume's groups, read in a single statement (one snapshot).
fn load_groups(conn: &Connection, volume_id: i64, run_id: i64, min_wasted: i64)
    -> Result<Vec<PendingGroup>>
{
    let rows = queries::volume_group_members(conn, volume_id, run_id, min_wasted)?;
    Ok(fold_groups(rows, min_wasted))
}

/// Resolves paths and hands each group to `sink`. Emits `Volume` with the exact
/// group count, then one `Group` per group; checks cancellation before each group.
fn emit_groups(
    conn: &Connection,
    volume_id: i64,
    groups: Vec<PendingGroup>,
    cancel: &Token,
    on: &mut dyn FnMut(ReportEvent),
    mut sink: impl FnMut(Group) -> Result<()>,
) -> Result<()> {
    on(ReportEvent::Volume { volume_id, groups: groups.len() as u64 });
    let mut paths = queries::PathCache::new();
    for (index, g) in groups.into_iter().enumerate() {
        cancel.check()?;
        let mut members = g
            .members
            .into_iter()
            .map(|(folder_id, name)| {
                Ok(Member { volume_id, path: paths.file_path(conn, folder_id, &name)?, size: g.size })
            })
            .collect::<Result<Vec<_>>>()?;
        members.sort_by(|a, b| a.path.cmp(&b.path));
        sink(Group {
            full_hash: g.full_hash,
            size: g.size,
            distinct_inodes: g.distinct_inodes,
            wasted_logical: g.wasted_logical,
            members,
        })?;
        on(ReportEvent::Group { done: index as u64 + 1 });
    }
    Ok(())
}

// Report::from_db_with: per volume
//     let groups = load_groups(conn, volume_id, run_id, min_wasted)?;
//     emit_groups(conn, volume_id, groups, cancel, on, |g| { out.push(g); Ok(()) })?;
//     // run_file_counts unchanged (already run-scoped)

// Report::stream_human: load first, then header, then stream
pub fn stream_human<W: std::io::Write>(
    conn: &Connection, min_wasted: i64, cancel: &Token, on: ReportProgress<'_>, out: &mut W,
) -> Result<()> {
    let mut loaded = Vec::new();
    for (volume_id, run_id) in completed_volumes(conn)? {
        cancel.check()?;
        loaded.push((volume_id, load_groups(conn, volume_id, run_id, min_wasted)?));
    }
    let count: usize = loaded.iter().map(|(_, g)| g.len()).sum();
    let wasted: i64 = loaded.iter().flat_map(|(_, g)| g).map(|g| g.wasted_logical).sum();
    let mut line = String::new();
    write_header(&mut line, count, wasted).expect("writing to a String cannot fail");
    out.write_all(line.as_bytes())?;
    for (volume_id, groups) in loaded {
        emit_groups(conn, volume_id, groups, cancel, on, |g| {
            line.clear();
            write_group(&mut line, &g).expect("writing to a String cannot fail");
            out.write_all(line.as_bytes())?;
            Ok(())
        })?;
    }
    on(ReportEvent::Done);
    out.flush()?;
    Ok(())
}
```

  Also update the `ReportEvent::Volume` doc: `groups` is now the exact count of groups that will be emitted (no "before the two-member guard").
- **Tests to add:**
  - `report.rs` unit tests for `fold_groups`: (1) two hard links of one inode plus one other file -> `distinct_inodes=2`, `wasted=size`, three members listed; (2) two hard links only -> dropped; (3) same hash, different size -> two groups; (4) `min_wasted` applied to recomputed wasted; (5) output order is wasted desc then hash.
  - DB-01 regression (core integration, inserting rows directly or with crafted contents): two groups whose wasted order is the reverse of hash order; `from_db` and `stream_human` both list 2 groups and the header says 2 and the summed bytes.
  - Parity: `stream_human` output equals `to_human()` of `from_db` for a single-volume DB (the pending B13a parity test), plus header numbers equal `group_count` / `wasted_logical_total` of `to_json`.
- **Risk / follow-ups:**
  - `stream_human` now holds all volumes' member rows (no paths) before writing the header; previously one volume at a time. Same order of magnitude as today's per-volume `Vec`; only multi-volume DBs grow. Acceptable, note for AUX-1 A5 timing.
  - `Hash::from_slice(..).unwrap_or(Hash([0;16]))` silently maps a corrupt BLOB to zero; out of scope, but a `FromSqlConversionFailure` would be better.
  - `bench/src/main.rs:119` keeps compiling after T1.2 only with the added `run_id` argument (pass `1`, the run id its synthetic rows use).

### T1.2 Run-scoped `dup_groups`, swapped atomically with `finish_run`, stamped at finish (closes PIPE-07, DB-14, DB-15, PIPE-17; data half of DB-05 / CORE-01)

- **Verification:**
  - PIPE-07 Confirmed: `recompute_groups` (`queries.rs:590-605`) has no run filter.
  - DB-14 Confirmed: `DELETE` and `INSERT ... SELECT` are two autocommit statements; `scan.rs:160-169` adds two more (`set_run_volume_phase`, `finish_run`) outside any transaction. A crash between recompute and `finish_run` also leaves `dup_groups` belonging to the new run while `latest_finished_run` still points to the old one.
  - DB-15 and PIPE-17 Confirmed (same bug): `now` is taken at `scan.rs:32` and reused at `:165` (`computed_at`) and `:169` (`finished_at`), so `finished_at == started_at`. `latest_finished_run` orders by `finished_at DESC` (`queries.rs:845`), so it really orders by start time.
- **Fix:** Filter the recompute by `last_seen_run_id = run_id`, so `dup_groups` holds exactly the groups the report lists (the passes already use this scope, so no partner is missing). Run all volumes' recompute, the phase updates and `finish_run` in one `IMMEDIATE` transaction on `db2`, so a concurrent `report` (WAL snapshot) sees either the old run's groups with the old latest run, or the new ones with the new run, never a mix or an empty table. Take a fresh `now_unix_ns()` for `computed_at` and `finished_at`. No schema change, stays v2.
  - `IMMEDIATE` rather than the default deferred transaction: the block starts with a read (`get_phase`), and upgrading a deferred WAL read transaction to a writer returns `SQLITE_BUSY_SNAPSHOT` without retrying through the busy handler if another connection committed meanwhile.
- **Rejected alternatives:**
  - Add `run_id` to `dup_groups` and keep one group set per run (schema v3): lets the report read the exact set of any run, but the atomic swap already guarantees `dup_groups` matches the latest finished run; not worth a schema bump.
  - Wrap only `recompute_groups` in its own transaction: fixes the empty-table window but not the mismatch between `dup_groups` and `latest_finished_run`.
- **Boilerplate:**

```rust
// crates/core/src/db/queries.rs
/// Idempotent recompute of one volume's groups from the rows live in `run_id`
/// (the report's scope, plan P1.4). Callers run it inside the transaction that
/// also marks the run finished, so readers never see a half-swapped table.
pub fn recompute_groups(conn: &Connection, volume_id: i64, run_id: i64, now: i64) -> Result<u64> {
    conn.execute("DELETE FROM dup_groups WHERE volume_id=?1", [volume_id])?;
    let n = conn.execute(
        "INSERT INTO dup_groups(volume_id,full_hash,size,member_count,distinct_inodes,\
         wasted_logical,wasted_physical,sharing_kind,computed_at) \
         SELECT volume_id, full_hash, size, COUNT(*), \
                COUNT(DISTINCT COALESCE(file_id, id)), \
                size*(COUNT(DISTINCT COALESCE(file_id, id))-1), NULL, 0, ?2 \
         FROM files \
         WHERE volume_id=?1 AND last_seen_run_id=?3 \
           AND state IN (3,4,5) AND full_hash IS NOT NULL \
         GROUP BY full_hash, size \
         HAVING COUNT(DISTINCT COALESCE(file_id, id)) > 1",
        params![volume_id, now, run_id],
    )?;
    Ok(n as u64)
}
```

```rust
// crates/core/src/pipeline/scan.rs  (before: lines 160-170)
if !stopped {
    let mut db2 = db::Db::open(db_path, cfg)?;
    let finished = now_unix_ns();
    let tx = db2
        .conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for v in &volumes {
        let phase = queries::get_phase(&tx, run_id, v.volume_id)?;
        if phase < Phase::GroupsDone {
            queries::recompute_groups(&tx, v.volume_id, run_id, finished)?;
            queries::set_run_volume_phase(&tx, run_id, v.volume_id, Phase::GroupsDone)?;
        }
    }
    queries::finish_run(&tx, run_id, finished)?;
    tx.commit()?;
}
```

  (`Transaction` derefs to `Connection`, so the existing `&Connection` query functions take `&tx` unchanged. `rusqlite::Error` already converts into `CoreError` via `?` elsewhere in `db/`.)
- **Tests to add:**
  - After a finished scan: `scan_runs.finished_at > started_at` and every `dup_groups.computed_at == finished_at` for the volume.
  - Partial-root rescan (extend `rescan_root_scoping_leaves_other_roots_untouched`, `crates/core/tests/phase1.rs:797`): scan A and B with a pair inside A, a pair inside B, and an A/B pair; rescan only A. Expect `dup_groups` and the report to contain only the A pair, with header totals equal to the listed groups. A group with 2 copies in A and 1 in B reports `x2`, `2 inode(s)`, `wasted size`.
  - Atomicity: call the block with a transaction that is dropped without `commit` and assert `dup_groups` and `scan_runs.status` are unchanged.
  - `--resume` on a completed DB still a no-op apart from the recompute (plan P1.7 invariant).
- **Risk / follow-ups:** Existing DBs keep `finished_at == started_at` for old runs; harmless, `run_id DESC` breaks ties. Bench call site needs the new argument.

### T1.3 Unfinished newer run note and README wording (CORE-01 (c), CORE-01 (a) documentation)

- **Verification:** CORE-01 (c) Partially confirmed. Enumeration upserts set `last_seen_run_id = run` and reset `state = 0` (`db/writer.rs:228-247`, plan P1.7.1), so rows already re-enumerated by a killed or still-running rescan drop out of the latest finished run's report. This is not specific to the run filter: those rows are back in state 0, so no scope (whole index included) can list them until the run finishes. It cannot be fixed without keeping a second copy of the previous state; mitigate by telling the user.
- **Fix:** In the CLI `report` command, before printing, warn on stderr when a run newer than the volume's latest finished run is still `running`. Core never prints, so core exposes a query and the CLI prints. Update README `report` text.
- **Boilerplate:**

```rust
// crates/core/src/report.rs
/// Id of a `running` scan newer than every finished run, if any. While one exists,
/// files it already re-enumerated are missing from the report until it completes.
pub fn unfinished_newer_run(conn: &Connection) -> Result<Option<i64>> {
    let running = queries::latest_running_run(conn)?;
    let newest_finished = completed_volumes(conn)?.into_iter().map(|(_, r)| r).max();
    Ok(running.filter(|r| newest_finished.is_none_or(|f| *r > f)))
}

// crates/cli/src/report.rs, before building the report
if let Some(run) = dedupe_core::report::unfinished_newer_run(&conn)? {
    eprintln!("note: scan run {run} did not finish; files it touched are missing from this report until `dedupe scan --resume` completes it");
}
```

  README (`README.md:86`), suggested text: "Prints the duplicate groups from the last completed scan of each volume. If that scan covered only some folders, duplicates under the folders it did not cover are not shown; scan all folders together to see them."
- **Tests to add:** CLI test: DB with a finished run plus a newer `running` row -> stderr contains the note; with no running run -> no note.

### Open product decision for the user (not a bug fix)

Whether `report` should ever show duplicates across roots scanned in different runs. Options to put to the user:
1. (Recommended) Keep latest-run scope as implemented above: matches plan and README, correct by construction, partial rescans show only the rescanned roots.
2. Add an opt-in whole-volume mode (for example `scan --whole-volume`) that runs size/prefix/full over every live row on the volume, reusing stored hashes and hashing old-run rows that never got one; costs I/O outside the requested roots and needs stale-file handling for unscanned roots.

---

## T2. Enumeration, folder/file id allocation, move detection and rescan rules

**Findings covered:** DB-02, PIPE-01, DB-03, PIPE-02, PIPE-03, PIPE-15, GEN-05, DB-19, DB-20, CORE-13

**Root causes:**
- `pass_enumerate` treats the read-only snapshot (`ctx.read`) as the only source of truth, but its own output sits uncommitted in `folders_out`/`files_out` and in the writer (5k-row local flush, 50k-row / 5 s writer commit). Nothing in-run is consulted before the DB, so the same `(parent, name)` gets a second folder id, and the same file row id is handed to two paths -> DB-02 = PIPE-01, DB-03 = PIPE-02.
- The writer's `ON CONFLICT(id) DO UPDATE` silently rewrites whatever row owns that id, so an id handed out twice becomes data loss rather than an error. The same SQL exists twice and has drifted -> DB-03, DB-19.
- The rescan "unchanged" predicate was coded without ctime -> PIPE-03.
- The `Enumerator` contract has no error channel, and stale deletion assumes "not seen" means "deleted" -> GEN-05. Path-to-mount mapping and missing-parent drops are lenient and silent for the same reason (no diagnostics path) -> PIPE-15.
- `is_legal_transition` is a table that production never consults. It is also out of date (see T2.6) -> CORE-13.

**Recommended order of work:**
1. T2.1 in-run folder overlay (Critical, small, self-contained).
2. T2.2 file claim overlay + guarded id-conflict upsert, which also lands the shared SQL from T2.5 (DB-19).
3. T2.3 ctime in the unchanged check.
4. T2.4 walk errors and protected subtrees for stale deletion (trait change, touches backends).
5. T2.7 PIPE-15 root/mount mapping and drop counters.
6. T2.6 CORE-13 SQL transition guards.
7. T2.8 DB-20 leftovers, opportunistically.

---

### T2.1 In-run folder overlay keyed by (parent_id, name) (closes DB-02, PIPE-01)

- **Verification:** DB-02 Confirmed, PIPE-01 Confirmed (same bug). `pass_enumerate.rs:72-74`: `let id = ensure_folder(...); dir_ids.entry(acc.clone()).or_insert(id); cur = id;` so `cur` takes the freshly allocated id even when `dir_ids` already holds the canonical one. `ensure_folder` (`pass_enumerate.rs:182-201`) only queries `queries::find_folder(ctx.read, ...)`, which cannot see `folders_out` (flushed at `BATCH`, line 123) or uncommitted writer batches (`writer.rs:171-178`). The writer's `ON CONFLICT(volume_id,parent_id,name) DO UPDATE SET last_seen_run_id` (`writer.rs:212-217`) drops the duplicate id, while children keep it as `parent_id`. The `WalkItem::Dir` branch (`:91-94`) has the same lookup, so nested roots (`/a`, `/a/b`) or a `$MFT`-to-walker fallback that re-yields entries allocate throwaway duplicate ids too (harmless there only because `or_insert` keeps the first, but still wasted ids and redundant upserts).
- **Fix:** keep a per-volume `HashMap<(i64, RawPath), i64>` of folders resolved this run, owned by `pass_enumerate::run` and passed to `ensure_folder`. Lookup order: overlay, then RO snapshot, then allocate. The overlay is authoritative for everything this run emitted, the snapshot covers rows from earlier runs, and the allocator only fires for a genuinely new folder. Emit the `FolderRow` upsert only on the first resolution per run (the overlay hit means the `last_seen_run_id` bump is already queued), which also removes the repeated mount-root and chain upserts per root. Keying by `(parent_id, name)` instead of by full path makes it independent of path-form normalisation (PIPE-15). `cur` always takes the id returned by `ensure_folder`, which is now canonical.
- **Rejected alternatives:**
  - `cur = *dir_ids.entry(acc).or_insert(id)` only: fixes the root chain but still allocates and emits a duplicate folder id; does not cover the `Dir` branch.
  - `ctx.writer.flush()` before each root: makes the snapshot current for chains, but adds a commit per root, couples correctness to commit timing, and still misses re-yields within one root.
  - Map inside `IdAllocator`: it is shared and atomic; pass_enumerate is single-threaded, so a `Mutex<HashMap>` there is needless.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/pass_enumerate.rs

/// Folders resolved this run on this volume: `(parent_id, name) -> id`.
/// Authoritative over `ctx.read`, which cannot see uncommitted rows.
type FolderOverlay = HashMap<(i64, RawPath), i64>;

pub fn run(ctx: &Ctx, v: &VolumeScan) -> Result<Vec<i64>> {
    // ...
    let mut folders: FolderOverlay = HashMap::new();
    let mount_root = ensure_folder(ctx, v.volume_id, 0, &RawPath::empty(), &mut folders, &mut folders_out)?;
    dir_ids.insert(v.mount.clone(), mount_root);

    for root in &v.roots {
        ctx.svc.cancel.check()?;
        let mut cur = mount_root;
        let mut acc = v.mount.clone();
        for comp in relative_components(root, &v.mount)? {          // T2.7 makes this fallible
            acc = acc.join_component(&comp);
            cur = ensure_folder(ctx, v.volume_id, cur, &RawPath::new(comp), &mut folders, &mut folders_out)?;
            dir_ids.insert(acc.clone(), cur);                          // always the canonical id
        }
        root_folder_ids.push(cur);
        // ... WalkItem::Dir branch:
        //     if let Some(&pid) = dir_ids.get(&parent_raw) {
        //         let id = ensure_folder(ctx, v.volume_id, pid, &name, &mut folders, &mut folders_out)?;
        //         dir_ids.insert(path, id);
        //     }
    }
    // ...
}

/// Resolve `(volume, parent, name)` to a folder id. Overlay first, then the
/// committed snapshot, then a new id. Emits the upsert once per run.
fn ensure_folder(
    ctx: &Ctx,
    volume_id: i64,
    parent_id: i64,
    name: &RawPath,
    overlay: &mut FolderOverlay,
    out: &mut Vec<FolderRow>,
) -> Result<i64> {
    let key = (parent_id, name.clone());
    if let Some(&id) = overlay.get(&key) {
        return Ok(id);
    }
    let id = match queries::find_folder(ctx.read, volume_id, parent_id, name)? {
        Some(id) => id,
        None => ctx.ids.folder(),
    };
    overlay.insert(key, id);
    out.push(FolderRow {
        id,
        volume_id,
        parent_id,
        name: name.clone(),
        last_seen_run_id: Some(ctx.run_id),
    });
    Ok(id)
}
```
  Memory: one entry per directory on the volume (dirs are typically 5-10% of files). `dir_ids` already holds a full path per directory, so this is the same order of cost. If that matters later, `dir_ids` can be dropped in favour of the overlay plus a path-to-id map for the walker's parent lookup only, but that is not needed now.
- **Tests to add:**
  - `phase1`: two roots under a shared, not-yet-indexed intermediate dir (`<tmp>/shared/x`, `<tmp>/shared/y`, mount = `<tmp>`), first scan with default writer thresholds. Assert every file's `reconstruct_file_path` equals its real path, `SELECT COUNT(*) FROM folders f WHERE parent_id<>0 AND NOT EXISTS (SELECT 1 FROM folders p WHERE p.id=f.parent_id)` is 0, and cross-root duplicates are reported.
  - Nested roots (`/a`, `/a/b`): same orphan-folder query is 0, and folder id count equals the number of distinct directories.
  - Unit: a fake enumerator that yields the same `Dir` twice; assert one `FolderRow` per directory reaches the writer.
- **Risk / follow-ups:** None structural. Root dedupe / nesting checks belong to the scan setup (scan.rs) and are tracked elsewhere (PIPE-16); the overlay makes nesting safe regardless.

---

### T2.2 Per-run file claims + guarded id-conflict upsert (closes DB-03, PIPE-02; lands DB-19 for the file upsert)

- **Verification:** DB-03 Confirmed, PIPE-02 Confirmed (same bug). Step 1 (`pass_enumerate.rs:217-218`) inserts `existing.id` into `seen_ids` without first checking whether step 2 already claimed that id for another path in this run (`:242-247`). `find_by_path` reads `ctx.read`, so it returns the pre-move location of a row whose move is still uncommitted. The writer (`writer.rs:228-247`) then applies two entries with the same `id`: the second hits `ON CONFLICT(id) DO UPDATE SET folder_id, name, ...` and moves the row back, losing the other path. If the target path is occupied, the id-conflict update raises `UNIQUE(volume_id,folder_id,name)`, which kills the writer.
- **Fix (two layers):**
  1. **Source of truth in pass_enumerate.** Replace `seen_ids: HashSet<i64>` with `claims: HashMap<i64, Claim>` where `Claim` records how the row was claimed this run. In step 1:
     - `Some(Claim::Path)`: this same path was already enumerated this run (nested roots, `$MFT`-to-walker fallback re-yield). Skip the item, return `None`.
     - `Some(Claim::Move)`: the row at this path in the snapshot has already been moved to another path this run. This path holds a different file now: fall through to step 2/3 (new row or another move).
     - `None`: claim as `Path`, current logic.
     Step 2 inserts `Claim::Move`. Ordering invariant: a claim that frees a path is always emitted before any entry that reuses it, because `files_out` and batches preserve order. That is what makes the writer's path-conflict branch safe for the freed path.
  2. **Fail loudly in the writer.** Keep `ON CONFLICT(id)` (moves need it: a moved row keeps its id and changes path), but guard it with `WHERE files.last_seen_run_id <> excluded.last_seen_run_id`. A legitimate move always targets a row from an earlier run (`find_move_candidate` filters `last_seen_run_id<>run_id`), so a zero-change execute means an id was handed out twice in one run (a bug, or DB-04 concurrency). Return an error instead of silently rewriting. Move the statement into `queries` so `lookup` (`queries::upsert_file`, `lookup.rs:284`) and the writer share one SQL string (DB-19).
- **Rejected alternatives:**
  - Drop `ON CONFLICT(id)` and add a separate `Batch::Moved` with `UPDATE ... WHERE id=?`: cleaner semantics, but adds a batch type and a second code path for the same row shape; the guarded clause gives the same safety with one statement.
  - Track every enumerated path in a `HashSet<(folder_id, name)>`: ~100 MB at 1M files; the id-keyed claim map costs the same as today's `seen_ids` plus one byte.
  - Skip move detection when `nlink > 1` (suggested in PIPE-02): not needed once claims are checked; it would also disable move reuse for every hardlinked file.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/pass_enumerate.rs

/// How a pre-existing row was claimed this run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Claim {
    /// Found at the same path (step 1).
    Path,
    /// Taken by move detection for a different path (step 2).
    Move,
}

// in run(): let mut claims: HashMap<i64, Claim> = HashMap::new();
// File branch:
//     if let Some(entry) = build_entry(ctx, v.volume_id, folder_id, &name, &meta, now, &mut claims, &mut claimed_inodes)? {
//         files_out.push(entry);
//         accepted += 1; ...
//     }

/// `None` when this exact path was already enumerated this run.
#[allow(clippy::too_many_arguments)]
fn build_entry(
    ctx: &Ctx,
    volume_id: i64,
    folder_id: i64,
    name: &RawPath,
    meta: &FileMeta,
    now: i64,
    claims: &mut HashMap<i64, Claim>,
    claimed_inodes: &mut HashSet<Vec<u8>>,
) -> Result<Option<FileEntry>> {
    let skip = skip_reason(meta, ctx.cfg.include_placeholders);

    // 1. Existing row at this path in the committed snapshot.
    if let Some(existing) = queries::find_by_path(ctx.read, volume_id, folder_id, name)? {
        match claims.get(&existing.id) {
            Some(Claim::Path) => return Ok(None), // re-yield of the same path
            Some(Claim::Move) => {}               // row moved away; this path is a new file
            None => {
                claims.insert(existing.id, Claim::Path);
                let unchanged = skip.is_none() && unchanged_since(&existing, meta); // T2.3
                let mut e = existing;
                apply_meta(&mut e, meta, now, ctx.run_id);
                e.state = if skip.is_some() { State::Skipped } else { State::Enumerated };
                if !unchanged {
                    e.prefix_hash = None;
                    e.full_hash = None;
                    e.prefix_bytes = None;
                }
                return Ok(Some(e));
            }
        }
    }

    // 2. Move detection (unchanged except the claim map).
    //     if claims.contains_key(&cand.id) { continue; }
    //     ... claims.insert(cand.id, Claim::Move); return Ok(Some(e));

    // 3. New row (unchanged), wrapped in Some.
    todo!()
}
```
```rust
// crates/core/src/db/queries.rs  (single statement, used by writer and lookup)

const UPSERT_FILE_SQL: &str = "INSERT INTO files(id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,\
     ctime_ns,attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,last_seen_run_id) \
     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17) \
     ON CONFLICT(volume_id,folder_id,name) DO UPDATE SET \
       size=excluded.size, mtime_ns=excluded.mtime_ns, ctime_ns=excluded.ctime_ns, \
       file_id=excluded.file_id, nlink=excluded.nlink, attrs=excluded.attrs, \
       prefix_hash=excluded.prefix_hash, full_hash=excluded.full_hash, \
       prefix_bytes=excluded.prefix_bytes, state=excluded.state, err=excluded.err, \
       scanned_at=excluded.scanned_at, last_seen_run_id=excluded.last_seen_run_id \
     ON CONFLICT(id) DO UPDATE SET \
       folder_id=excluded.folder_id, name=excluded.name, size=excluded.size, \
       mtime_ns=excluded.mtime_ns, ctime_ns=excluded.ctime_ns, \
       file_id=excluded.file_id, nlink=excluded.nlink, attrs=excluded.attrs, \
       prefix_hash=excluded.prefix_hash, full_hash=excluded.full_hash, \
       prefix_bytes=excluded.prefix_bytes, state=excluded.state, err=excluded.err, \
       scanned_at=excluded.scanned_at, last_seen_run_id=excluded.last_seen_run_id \
     WHERE files.last_seen_run_id <> excluded.last_seen_run_id";

/// Upsert one enumerated row. A move (id conflict) may only take a row last
/// seen in an earlier run; anything else is an id handed out twice and fails.
pub fn upsert_file(conn: &Connection, f: &FileEntry) -> Result<()> {
    let n = conn.prepare_cached(UPSERT_FILE_SQL)?.execute(params![
        f.id, f.volume_id, f.folder_id, f.name.as_bytes(), f.file_id, f.nlink, f.size,
        f.mtime_ns, f.ctime_ns, f.attrs,
        f.prefix_hash.as_ref().map(|h| h.as_bytes().as_slice()),
        f.full_hash.as_ref().map(|h| h.as_bytes().as_slice()),
        f.prefix_bytes, f.state.as_i64(), f.err, f.scanned_at, f.last_seen_run_id,
    ])?;
    if n == 0 {
        return Err(CoreError::Other(format!(
            "file row id {} already claimed by another path in run {}",
            f.id, f.last_seen_run_id
        )));
    }
    Ok(())
}

// crates/core/src/db/writer.rs
//     Batch::Enumerated(rows) => for f in &rows { queries::upsert_file(tx, f)?; }
// (`&Transaction` derefs to `&Connection`; prepare_cached keeps the statement cache.)
```
  Consider a dedicated `CoreError::IdConflict { id: i64, run_id: i64 }` instead of `Other`, so the CLI can say "another scan may be writing this database" (ties into DB-04).
- **Tests to add:**
  - Writer unit test: two `Enumerated` entries with the same id and different paths, same `last_seen_run_id`, in one commit -> commit returns the id-conflict error and the first row is intact. Also confirm the zero-change count (the `n == 0` behaviour of a `DO UPDATE ... WHERE false` upsert is per SQLite docs, but verify it in this test; I did not run it).
  - `phase1` rescan, rename-and-recreate with forced order: fake enumerator yields `app.log.1` (old inode) before a new `app.log`; one commit window. Assert both rows exist, `app.log.1` kept its hashes (moved), `app.log` has null hashes then gets hashed.
  - Hardlink case: new link `c` to existing inode of `a`, yielded before `a`. Assert both paths have rows.
  - Re-yield: fake enumerator yields the same file twice in one run -> one row, hashes kept when unchanged.
- **Risk / follow-ups:** Re-yield of a brand-new file inside one commit window still allocates a second id that the path-conflict branch absorbs (wasted id, no data effect). An erroring writer now stops the scan where it used to corrupt silently; that is the intended trade.

---

### T2.3 Add ctime to the rescan unchanged check (closes PIPE-03)

- **Verification:** Confirmed. `pass_enumerate.rs:219-222` compares `size`, `mtime_ns`, `file_id` only; plan P1.7.1 (`.overall_plan.md:1336-1337`) and CLAUDE.md specify `(size, mtime_ns, ctime_ns, file_id)`. Hash reuse downstream trusts this decision.
- **Decision: add ctime, with the same "both Some" rule as `meta_matches` (`pass_prefix.rs:251-257`).**
  - Unix: ctime is inode change time and cannot be set from user space, so it catches `touch -r`, `cp -p` over a file, and archive extraction with preserved mtime. Cost: chmod/chown/new hardlink also bump ctime and cause a rehash of that file. Rename updates ctime on Linux too, but renames go through move detection (step 2, size+mtime only, which the plan allows), not step 1, so moves still keep hashes.
  - Windows (GEN-11): both the walker (`backend-generic/src/meta.rs:119-121`, `created()`) and the `$MFT` path (`backend-windows/src/records.rs:142`, `creation_time()`) store creation time, so the two enumerators agree and the check is consistent, if weak (creation time does not change on write). The check is still correct: it never causes false "unchanged".
  - Coupling with GEN-11: if GEN-11 switches Windows `ctime_ns` to `ChangeTime`, it must change both the walker and the `$MFT` path in the same commit. Otherwise every walker/MFT mode switch would mismatch ctime and rehash everything (same class of problem as the unverified file_id equality in CLAUDE.md Gotchas). Existing rows will hold creation times, so the first rescan after that change rehashes once; acceptable under the no-migration policy, or bump schema to v3 if a clean break is preferred. No schema change is needed for PIPE-03 itself.
- **Rejected alternatives:** Leave ctime out and update the plan: loses the only Unix-side guard against mtime-preserving rewrites, which is exactly the false-duplicate case.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/pass_enumerate.rs

/// P1.7.1: hashes survive a rescan only if size, mtime, ctime and file id match.
/// ctime is compared only when both sides have one (FAT, pre-1970 on Windows).
fn unchanged_since(existing: &FileEntry, meta: &FileMeta) -> bool {
    let ctime_ok = match (existing.ctime_ns, meta.ctime_ns) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    existing.size == meta.size
        && existing.mtime_ns == meta.mtime_ns
        && ctime_ok
        && existing.file_id == meta.file_id
}
```
  Optionally have `meta_matches` call a shared `ctime_matches(a, b)` helper so the rule lives in one place.
- **Tests to add:** Unix-only integration test in the CLI crate or a core test with a fake enumerator: scan, rewrite the file in place with same size, restore mtime with `filetime`/`touch -r` semantics (fake: same size/mtime, different ctime), rescan; assert `prefix_hash`/`full_hash` were nulled and recomputed, and the false duplicate is gone.
- **Risk / follow-ups:** Slightly more rehashing after metadata-only changes on Unix (chmod -R on a tree rehashes it). Document in README.

---

### T2.4 Walk errors are reported and their subtrees are protected from stale deletion (closes GEN-05)

- **Verification:** Confirmed. `backend-generic/src/walk.rs:35` `walker.filter_map(|e| e.ok())` and `:50` `Err(_) => continue` drop errors; `WalkItem` (`core/src/traits.rs:14-17`) has no error variant; `rescan::delete_stale` (`pipeline/rescan.rs:11-21`, SQL `writer.rs:373-406`) deletes every row under the roots with `last_seen_run_id < run`, so an unreadable directory's old rows (and their hashes) are deleted.
- **Fix:**
  1. Add `WalkItem::Error { path, is_dir, kind, message }`. The generic walker emits it for walkdir errors and lstat failures. `is_dir` is the backend's best knowledge; when unknown it is `true`, which is the conservative choice (protect more, delete less).
  2. `pass_enumerate` handles it:
     - always queue an `ErrRow` (kind Permission or Io, path set);
     - directory error: resolve its folder id (`dir_ids.get(path)`; walkdir yields the dir entry before reading it, so it is normally registered; otherwise use the parent's id) and push it into `protected: Vec<i64>`;
     - file error: if a row exists at that path (overlay-aware via `claims`), re-emit it with `state = Error`, `err = kind`, hashes kept, `last_seen_run_id = run`, and claim it `Path`. It survives stale deletion, is not reported this run (state -2 is terminal), and the next run re-evaluates it through the normal step 1 check.
  3. `Batch::DeleteStale` gains `protected_folder_ids`. The recursive CTE does not descend into protected folders, so their files and subfolders keep their rows until a run that can read them. The protected folder row itself was already bumped when its `Dir` item was seen.
  The `$MFT` path reads the raw table and has no per-directory access errors; it only needs the variant for exhaustive matches. Its walker fallback is the generic walker, so it gets the behaviour for free.
- **Rejected alternatives:**
  - Separate error callback on `Enumerator::walk`: a second `&mut dyn FnMut` in the trait for one rare event; the enum variant keeps one visitor.
  - Abort the whole scan on any walk error: one unreadable `lost+found` would make scans of `/` impossible.
  - Skip stale deletion entirely when any error occurred: correct but coarse; one EACCES would stop all stale cleanup forever on that root.
- **Boilerplate:**
```rust
// crates/core/src/traits.rs
#[derive(Clone, Debug)]
pub enum WalkItem {
    Dir { path: RawPath },
    File { path: RawPath, meta: FileMeta },
    /// An entry the walker could not read. `is_dir` is true when the failed path
    /// is (or may be) a directory: its subtree was not enumerated.
    Error {
        path: RawPath,
        is_dir: bool,
        kind: crate::model::ErrorKind,
        message: String,
    },
}
```
```rust
// crates/backend-generic/src/walk.rs
use dedupe_core::model::ErrorKind;

fn walk_error(codec: &GenericPathCodec, path: &Path, err: &std::io::Error, is_dir: bool) -> WalkItem {
    let kind = match err.kind() {
        std::io::ErrorKind::PermissionDenied => ErrorKind::Permission,
        _ => ErrorKind::Io,
    };
    WalkItem::Error { path: codec.path_to_raw(path), is_dir, kind, message: err.to_string() }
}

// in walk():
for entry in walker {
    let entry = match entry {
        Ok(e) => e,
        Err(err) => {
            // walkdir errors carry the path being read (a directory for read_dir
            // failures). No path (rare) means nothing to attribute: skip.
            let Some(path) = err.path() else { continue };
            // Unknown type => treat as a directory so its old rows are kept.
            let is_dir = path.symlink_metadata().map(|m| m.is_dir()).unwrap_or(true);
            let io = err.io_error().map(|e| std::io::Error::new(e.kind(), e.to_string()))
                .unwrap_or_else(|| std::io::Error::other(err.to_string()));
            visit(walk_error(&codec, path, &io, is_dir))?;
            continue;
        }
    };
    // ... Dir branch unchanged ...
    match path.symlink_metadata() {
        Ok(md) => { /* unchanged */ }
        Err(e) => visit(walk_error(&codec, path, &e, false))?,
    }
}
```
```rust
// crates/core/src/pipeline/pass_enumerate.rs  (inside the visitor)
WalkItem::Error { path, is_dir, kind, message } => {
    let native = ctx.svc.codec.raw_to_path(&path);
    let (parent_raw, name) = ctx.svc.codec.split_component(&native);
    let mut row_id = None;
    if is_dir {
        if let Some(&id) = dir_ids.get(&path).or_else(|| dir_ids.get(&parent_raw)) {
            protected.push(id);
        }
    } else if let Some(&fid) = dir_ids.get(&parent_raw) {
        if let Some(mut e) = queries::find_by_path(ctx.read, v.volume_id, fid, &name)? {
            if !claims.contains_key(&e.id) {
                claims.insert(e.id, Claim::Path);
                e.state = State::Error;
                e.err = Some(kind.as_i64());
                e.last_seen_run_id = ctx.run_id;
                row_id = Some(e.id);
                files_out.push(e); // hashes kept; next run re-checks via step 1
            }
        }
    }
    errors_out.push(ErrRow { run_id: ctx.run_id, file_row_id: row_id, path: Some(path), kind, message: Some(message) });
    Ok(())
}
// after the walk: flush errors_out as Batch::Error(...) after flush_files,
// then rescan::delete_stale(ctx, v.volume_id, &root_folder_ids, &protected)?;
```
  Note `Batch::Error` sets `state=-2, err` again for `file_row_id`; harmless. Alternatively send the `ErrRow` with `file_row_id: None` and rely on the re-emitted entry; either works, pick one.
```rust
// crates/core/src/db/writer.rs  (or queries::delete_stale, see T2.5)
Batch::DeleteStale { run_id, volume_id, root_folder_ids, protected_folder_ids }

// SQL (both statements), with {roots} and {prot} as comma-joined i64 lists
// ({prot} = "NULL" when empty so `NOT IN (NULL)` is never used: build the
// `AND f.id NOT IN (...)` clause only when the list is non-empty):
WITH RECURSIVE sub(id) AS (
  SELECT id FROM folders WHERE id IN ({roots}) AND id NOT IN ({prot})
  UNION ALL
  SELECT f.id FROM folders f JOIN sub s ON f.parent_id = s.id
  WHERE f.id NOT IN ({prot})
)
DELETE FROM files WHERE volume_id=?1 AND last_seen_run_id < ?2
  AND folder_id IN (SELECT id FROM sub);
```
  Match sites that need an `Error` arm (exhaustive matches): `backend-windows/src/mft_cache.rs:1124,1133`, `mft_walk.rs:431,713`, `tree.rs:364,528,1038`, `backend-windows/tests/mft_vs_walk.rs:108`. `core/src/pipeline/naive.rs:75` uses `let ... else` and keeps working, but should count the error into its summary. Run the Windows cross-check after this change.
- **Tests to add:**
  - Unix integration (skip when running as root, since root ignores mode bits): scan a tree, `chmod 000` a subdirectory, rescan; assert its file rows still exist with old hashes, an `errors` row with kind Permission exists for the dir, and `report` still runs. Restore mode and rescan: rows refreshed, nothing deleted.
  - Fake enumerator emitting `WalkItem::Error { is_dir: false }` for a previously indexed file: row survives with state -2; next clean run returns it to normal states with hashes kept.
  - Writer unit: `DeleteStale` with a protected folder leaves its subtree intact and deletes stale rows elsewhere.
- **Risk / follow-ups:** A directory that is permanently unreadable keeps stale rows forever; `db info` could show a "protected stale" count. `report` does not show errors today; consider a one-line "N paths unreadable" note in the CLI (CLI topic).

---

### T2.5 One SQL statement per operation (closes DB-19)

- **Verification:** Confirmed. `queries::upsert_file` (`queries.rs:199-231`) lacks the `ON CONFLICT(id)` clause the writer has (`writer.rs:228-247`); `queries::delete_stale_under_roots` (`queries.rs:427-451`) deletes files only and is used only by tests (`queries.rs:1023`), while production runs the writer's two-statement version (`writer.rs:373-406`); `insert_folder` (`queries.rs:174-181`) duplicates `writer.rs:211-226`.
- **Fix:** move each statement into `queries` and have the writer call them with `tx` (`&Transaction` derefs to `&Connection`; `prepare_cached` keeps statement caching): `upsert_folder(conn, &FolderRow)` (rename of `insert_folder`), `upsert_file` (T2.2), `delete_stale(conn, run_id, volume_id, roots, protected) -> Result<(u64, u64)>` (files, folders; T2.4 SQL). Delete `delete_stale_under_roots` and point its test at `delete_stale`.
- **Rejected alternatives:** Keep two copies and add a test asserting equal behaviour: duplicates the drift risk instead of removing it.
- **Boilerplate:**
```rust
// crates/core/src/db/writer.rs
Batch::Folders(rows) => for f in &rows { queries::upsert_folder(tx, f)?; },
Batch::Enumerated(rows) => for f in &rows { queries::upsert_file(tx, f)?; },
Batch::DeleteStale { run_id, volume_id, root_folder_ids, protected_folder_ids } => {
    queries::delete_stale(tx, run_id, volume_id, &root_folder_ids, &protected_folder_ids)?;
}
```
- **Tests to add:** Existing `queries` tests now exercise production SQL; add the folder-deletion assertion to the renamed stale test.
- **Risk / follow-ups:** Per-row `prepare_cached` lookups are a hash lookup each; negligible against SQLite execute cost. Keep a per-batch statement if profiling disagrees.

---

### T2.6 Enforce transitions as SQL guards derived from the table (closes CORE-13)

- **Verification:** Confirmed, plus a new detail: the table is already wrong for production. `is_legal_transition` (`model.rs:116-131`) has no `src/` callers. `pass_prefix.rs:107-121` (DB-index path) sends `PrefixHashed` (state 2), commits, then sends `UniquePrefix` for the singletons, i.e. 2 -> -4, which the table forbids (`PrefixHashed => FullHashed | Done | Skipped | Error`). `lookup.rs:243,251` sets `state=5` from any state, outside a run.
- **Decision: enforce, do not delete.** The writer already guards one transition (`Verified ... WHERE id=?1 AND state=3`, `writer.rs:333`). Generalise that: every `UPDATE files SET state=X WHERE id=?` gets `AND state IN (<legal sources of X>)`, with the source list computed from `is_legal_transition`, so the table becomes load-bearing and the property tests test real behaviour. Cost: a filter on a PK lookup, effectively free. A zero-change update is counted and logged (`tracing::warn!` once per commit with the count); it is not an error, because a duplicate or late batch for a row reset by a rescan should be dropped, not abort the scan.
  - First add `PrefixHashed -> UniquePrefix` to the table (the DB-index classification path), and decide whether `SizeCandidate -> FullHashed` for `HashedPending` is covered (it is).
  - `Batch::Error`: legal sources are all non-terminal states (0..=4), per the table.
  - `PromotePending` (3 -> 5) and `Verified` already have explicit guards; derive them too for consistency.
  - Lookup (`lookup.rs:243,251`) is outside a run; restrict it to `AND state NOT IN (-1,-2)` so it never resurrects a skipped or errored row. Leave the rest as is.
- **Rejected alternatives:**
  - Delete `is_legal_transition` and the prop tests: removes the only written statement of the state machine.
  - `debug_assert!` with a per-row `SELECT state` in the writer: an extra query per row in debug builds, and still no protection in release.
- **Boilerplate:**
```rust
// crates/core/src/model.rs
impl State {
    pub const ALL: [State; 10] = [ /* every variant */ ];
}

/// `state IN (...)` list of every state that may legally move to `to`.
pub fn legal_sources_sql(to: State) -> String {
    State::ALL
        .iter()
        .filter(|&&from| is_legal_transition(from, to))
        .map(|s| s.as_i64().to_string())
        .collect::<Vec<_>>()
        .join(",")
}
// is_legal_transition: PrefixHashed => matches!(to, UniquePrefix | FullHashed | Done | Skipped | Error)
```
```rust
// crates/core/src/db/writer.rs
Batch::SizeCandidate(ids) => {
    let sql = format!(
        "UPDATE files SET state=1 WHERE id=?1 AND state IN ({})",
        legal_sources_sql(State::SizeCandidate)
    );
    let mut stmt = tx.prepare_cached(&sql)?; // same text every time, so cached
    for id in ids {
        rejected += usize::from(stmt.execute([id])? == 0);
    }
}
// ... same pattern for every state-setting batch ...
// end of commit(): if rejected > 0 { tracing::warn!(rejected, "illegal state transitions skipped"); }
```
  Build the guarded SQL strings once (e.g. a small `fn guarded(sql_head: &str, to: State) -> String`) rather than formatting per batch if preferred; with `prepare_cached` either is fine.
- **Tests to add:** Replace the restating prop tests with one that runs the real pipeline (`phase1` fixtures, both the in-memory and DB-index prefix paths, verify off and byte) and asserts the warn counter is zero, exposed via a `pub(crate)` counter or a writer test hook. Writer unit: `UniqueSize` for a row in state 5 changes nothing.
- **Risk / follow-ups:** If any other undocumented transition exists in production it now silently stops applying; the zero-counter test over both prefix paths is what catches that. Run it before merging.

---

### T2.7 Strict root/mount mapping and counted drops (closes PIPE-15)

- **Verification:** Partially confirmed. Code behaviour Confirmed: `relative_components` (`pass_enumerate.rs:302-311`) falls back to the whole root when it does not start with the mount, matches without a component boundary, and `split_components` (`:313-319`) splits on `\` on Unix. Missing-parent drops are silent (`:91-94`, `:104-107`). `scan.rs:46` calls `std::fs::canonicalize` in core. Impact on Windows (whether `GetVolumePathNameW` and `canonicalize` produce matching forms after codec normalisation) is not reproduced here and stays unverified.
- **Fix:**
  - `relative_components` returns `Result`, splits only on the mount's separator (`path::separator_for`), requires a component boundary, compares the prefix ASCII-case-insensitively when the separator is `\` (Windows drive letters; no `cfg` needed), and errors with `InvalidArg` when the root is not under the mount instead of producing a fake chain.
  - Track pruned directories (excluded, or themselves dropped) in a `HashSet<RawPath>`; a Dir/File whose parent is pruned is dropped silently as today. A missing parent that is not pruned increments `orphans` and logs the first few at `warn`; the count is returned in the pass result so the CLI can print a `note:` (CLI topic).
  - `canonicalize` in `scan.rs`: std is portable and does not break the no-`cfg` rule, so leave it, but note it in CLAUDE.md. Moving it behind `PathCodec` is optional and not worth the churn now.
- **Rejected alternatives:** Normalise both through the codec and keep the fallback: still silently wrong when they disagree; an error is better than a scan that reports 0 files.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/pass_enumerate.rs
use crate::error::CoreError;
use crate::path::separator_for;

/// Components of `root` below `mount`. Empty when root == mount.
pub fn relative_components(root: &RawPath, mount: &RawPath) -> Result<Vec<Vec<u8>>> {
    let (m, r) = (mount.as_bytes(), root.as_bytes());
    let sep = separator_for(m);
    let prefix_ok = r.len() >= m.len()
        && if sep == b'\\' { r[..m.len()].eq_ignore_ascii_case(m) } else { &r[..m.len()] == m };
    let rest = &r[m.len().min(r.len())..];
    let boundary = rest.is_empty() || rest[0] == sep || m.last() == Some(&sep);
    if !prefix_ok || !boundary {
        return Err(CoreError::InvalidArg(format!(
            "scan root {} is not under its volume mount {}",
            root.to_display(),
            mount.to_display()
        )));
    }
    Ok(rest
        .split(|&b| b == sep)
        .filter(|c| !c.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}
```
- **Tests to add:** Unit tests: `/mnt/data` vs `/mnt/database/x` errors; `C:\` vs `c:\Users\a` succeeds; Unix name containing `\` stays one component; root == mount returns empty. Fake enumerator yielding a file whose parent was never yielded: `orphans == 1` and a warn.
- **Risk / follow-ups:** A root that used to "work" through the fallback now errors; that root was producing wrong paths anyway.

---

### T2.8 DB-20 minor items (closes DB-20)

- **Verification:** Confirmed for all sub-items: (a) `writer.rs:123` `.expect("spawn db-writer")`; (b) `db/mod.rs:47` unused `_cfg`; (c) `db/mod.rs:36` `foreign_keys=ON` on the RO connection; (d) `index/memory.rs:50-53` doc says "Highest count" but returns key count; (e) `PathCache` (`queries.rs:498-583`) caches one mount with no volume check; (f) `ids.rs:16-29` seeds from `MAX(id)+1`; (g) `schema.rs:27-42` reads `user_version` outside the transaction.
- **Fixes:**
  - (a) `spawn_writer*` return `Result<WriterHandle, CoreError>`; map the spawn `io::Error` via `CoreError::Io`. Update callers (scan.rs, tests).
  - (b) Drop the `_cfg` parameter. (c) Remove the pragma line from `open_readonly`. (d) Fix the doc: "Number of distinct keys recorded."
  - (e) Store `mount: Option<(i64, RawPath)>` and reload when `volume_id` differs.
  - (f) T2-relevant: seed files from `MAX(COALESCE(MAX(files.id),0), COALESCE(MAX(errors.file_row_id),0)) + 1` so a stale-deleted id referenced by error history is never reused. No schema change. (Rejected: `AUTOINCREMENT`/high-water mark in `meta`: needs a schema change or extra writes for the same effect.)
  - (g) Begin with `TransactionBehavior::Immediate`, re-read `user_version` inside, and when it is 0 refuse unless `sqlite_master` is empty ("not a dedupe database").
- **Boilerplate (f, g):**
```rust
// crates/core/src/db/ids.rs
let max_file = conn.query_row(
    "SELECT MAX(COALESCE((SELECT MAX(id) FROM files),0), \
                COALESCE((SELECT MAX(file_row_id) FROM errors),0))",
    [],
    |r| r.get::<_, i64>(0),
)?;

// crates/core/src/db/schema.rs
let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
let v = current_version(&tx)?;
if v == 0 {
    let objects: i64 = tx.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
    if objects > 0 {
        return Err(CoreError::InvalidArg("not a dedupe database".into()));
    }
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "user_version", EXPECTED_VERSION)?;
} else if v != EXPECTED_VERSION {
    return Err(CoreError::SchemaVersion { found: v, expected: EXPECTED_VERSION });
}
tx.commit()?;
```
- **Tests to add:** (f) insert an error row with `file_row_id = 99` and no file row, seed, assert next file id is 100. (g) open a SQLite file with an unrelated table and `user_version=0`, expect "not a dedupe database". (e) one `PathCache` across two volumes returns correct mounts.
- **Risk / follow-ups:** (g) `Immediate` waits on `busy_timeout` if another writer holds the DB; fine. Inter-process locking itself is DB-04, outside this topic.

---

## T3. Crash safety, resume, writer lifecycle, inter-process locking and cancellation

**Findings covered:** PIPE-04, PIPE-05, DB-06, PIPE-06, DB-07, DB-04, DB-16, PIPE-08, PIPE-09, WIO-05, CLI-05, CLI-07

**Root causes:**
- Passes treat "send to writer" as an end-of-pass step instead of a streaming step, and put the phase marker before trailing work. Result: whole-pass loss on crash (PIPE-04) and a phase marker that commits ahead of `DeleteStale` / `UniquePrefix` (DB-06 = PIPE-06, plus the prefix over-budget branch). One fix: stream bounded result chunks, and make every trailing write ride in the same writer transaction as `PhaseComplete`.
- `WriterHandle` has no owned lifecycle: errors are flattened to `WriterDied`, `run_scan` returns via `?` before `shutdown()`, a panic drops the handle without a join, and the time-based commit only fires on idle (DB-07, DB-16, part of PIPE-09). One fix: always join the writer (explicit merge in `run_scan` plus a `Drop` safety net) and check the deadline after every message.
- No exclusion between processes sharing a DB (DB-04). Fix: an OS advisory lock on a sidecar `<db>.lock` file, held for every read-write command.
- `--force` wipe resets file states but not the run's phase bookkeeping (PIPE-05). Fix: reset `phase_reached` and clear `dup_groups` in the same transaction.
- Cancellation is checked only between files, a cancelled read is indistinguishable from an I/O error, worker panics are not caught, and the CLI handler never escalates or distinguishes "cancelled" (PIPE-08, PIPE-09, WIO-05, CLI-05, CLI-07). Fix: a `&Token` threaded into the chunk loops, `Cancelled` propagated (never recorded as a `-2` row), `catch_unwind` per job, a two-stage Ctrl-C handler and exit code 130.

**Recommended order of work:**
1. T3.1 Writer lifecycle (DB-07, DB-16, writer half of PIPE-09). Everything else relies on "a returned error still commits what was sent".
2. T3.2 Phase marker ordering (DB-06, PIPE-06).
3. T3.3 Streamed pass results (PIPE-04), including the prefix singleton rewrite.
4. T3.4 `--force` wipe resets phases (PIPE-05).
5. T3.5 Cancellation inside reads and panic isolation (PIPE-08, PIPE-09).
6. T3.6 Inter-process lock (DB-04). Needs the MSRV bump decision.
7. T3.7 CLI Ctrl-C handler and exit code (CLI-05, CLI-07).
8. T3.8 Windows cancel gaps (WIO-05).

---

### T3.1 Writer lifecycle: always join, surface the real error, honour the 5 s deadline (closes DB-07, DB-16, part of PIPE-09)

- **Verification:**
  - DB-07 Confirmed. `crates/core/src/db/writer.rs:85-87` maps every send failure to `WriterDied`; `:159-161` acks a failed `Flush` with `WriterDied` and returns the real error only through the thread result; `:96-98` maps a join panic to `WriterDied`. `crates/core/src/pipeline/scan.rs:95-156` uses `?` on every pass (`:103, :111, :119, :127, :137, :141, :144`), so `writer.shutdown()` at `:158` is skipped on any error including `Cancelled`; the `JoinHandle` is dropped and the thread detaches. The real `CoreError` only reaches `tracing::error!` at `writer.rs:121`.
  - DB-16 Confirmed. `writer.rs:138-139` computes a timeout, but the commit only happens in `Err(RecvTimeoutError::Timeout)` (`:180-184`). std `recv_timeout` returns `Ok(msg)` whenever a message is queued, even if the deadline has passed, so with a non-empty channel only the 50k threshold (`:174`) commits.
  - Side finding: `writer.rs:123` uses `.expect("spawn db-writer")` in non-test code.
- **Fix:**
  1. `run_scan` runs the volume loop in a helper that returns `Result<bool>` (stopped), then always calls `writer.shutdown()`, then merges the two results, preferring the writer's real error over `WriterDied`. A `Cancelled` scan therefore commits every batch already sent (the `Shutdown` arm commits `pending`, then checkpoints), which makes Ctrl-C lose strictly less than `kill -9`.
  2. `shutdown` turns a panic payload into a readable `CoreError::Other`.
  3. `WriterHandle` gets a `Drop` that sends `Shutdown` and joins if `shutdown` was not called. This covers unwinding (a worker panic re-raised by `thread::scope`, PIPE-09) and any future early return.
  4. The `Flush` ack can stay `WriterDied`: the caller always joins now, and the merge picks the real error. No need to make `CoreError` `Clone`.
  5. After every received batch, commit if `since.elapsed() >= max` (DB-16).
  6. `spawn_writer` returns `Result<WriterHandle, CoreError>` instead of `expect`.
- **Rejected alternatives:**
  - Make `CoreError: Clone` (or wrap in `Arc`) so the `Flush` ack carries the real error: `rusqlite::Error` and `io::Error` are not `Clone`, and joining already yields the original.
  - Stringify the error into the ack (`CoreError::Other(e.to_string())`): loses the variant (`Sqlite(SQLITE_FULL)`), which the CLI may want to match on.
- **Boilerplate:**
```rust
// crates/core/src/db/writer.rs
pub struct WriterHandle {
    tx: SyncSender<Batch>,
    join: Option<JoinHandle<Result<(), CoreError>>>,
}

impl WriterHandle {
    pub fn shutdown(mut self) -> Result<(), CoreError> {
        self.join_inner()
    }

    fn join_inner(&mut self) -> Result<(), CoreError> {
        let Some(join) = self.join.take() else {
            return Ok(());
        };
        // Ignored: a send error means the writer already exited; join reports why.
        let _ = self.tx.send(Batch::Shutdown);
        match join.join() {
            Ok(res) => res,
            Err(payload) => Err(CoreError::Other(format!(
                "db writer panicked: {}",
                panic_message(payload.as_ref())
            ))),
        }
    }
}

impl Drop for WriterHandle {
    fn drop(&mut self) {
        // Safety net for unwinding and early returns: commit what was sent and join.
        if let Err(e) = self.join_inner() {
            tracing::error!("db writer failed during drop: {e}");
        }
    }
}

/// Text of a panic payload (`&str` or `String`), for error messages.
pub(crate) fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_owned())
}

pub fn spawn_writer_with(
    conn: Connection,
    prefix_total: i64,
    max_rows: usize,
    max_ms: u64,
) -> Result<WriterHandle, CoreError> {
    let (tx, rx) = sync_channel::<Batch>(4);
    let join = std::thread::Builder::new()
        .name("db-writer".into())
        .spawn(move || {
            run(conn, rx, prefix_total, max_rows, max_ms)
                .inspect_err(|e| tracing::error!("db-writer failed: {e}"))
        })?; // io::Error -> CoreError::Io
    Ok(WriterHandle { tx, join: Some(join) })
}

// In `run`, the generic arm (DB-16):
            Ok(b) => {
                rows += b.row_count();
                pending.push(b);
                // recv_timeout returns queued messages even past the deadline, so the
                // time-based commit must also be checked here, not only on Timeout.
                if rows >= max_rows || since.elapsed() >= max_wait {
                    commit(&mut conn, &mut pending, prefix_total)?;
                    rows = 0;
                    since = Instant::now();
                }
            }
// with `let max_wait = Duration::from_millis(max_ms);` hoisted above the loop.
```
```rust
// crates/core/src/pipeline/scan.rs
    let read = db::Db::open_readonly(db_path)?.conn;
    let ids = IdAllocator::seed(&read)?;
    let writer = spawn_writer(db.conn, cfg.prefix_total() as i64)?;
    let ctx = Ctx { cfg, run_id, writer: &writer, read: &read, ids: &ids, svc };
    let passes = run_passes(&ctx, &volumes, &phases, stop_after(cfg, opts));
    // `ctx` is not used after this point, so `writer` can be moved (no Drop on Ctx).
    let stopped = merge_writer_result(passes, writer.shutdown())?;
    // ... unchanged post-shutdown recompute and finish_run ...

/// The old `for v in &volumes { ... }` body, unchanged except that it returns `stopped`.
fn run_passes(
    ctx: &Ctx,
    volumes: &[VolumeScan],
    phases: &HashMap<i64, Phase>,
    stop_after: Option<i64>,
) -> Result<bool> {
    // ...
}

/// Prefer the writer's real error over the `WriterDied` a pass saw when sending.
fn merge_writer_result(passes: Result<bool>, writer: Result<()>) -> Result<bool> {
    match (passes, writer) {
        (Ok(stopped), Ok(())) => Ok(stopped),
        (Ok(_), Err(w)) | (Err(CoreError::WriterDied), Err(w)) => Err(w),
        (Err(e), Err(w)) => {
            tracing::error!("db writer also failed: {w}");
            Err(e)
        }
        (Err(e), Ok(())) => Err(e),
    }
}
```
- **Tests to add:**
  - Writer unit test: set `max_ms = 50`, keep the channel non-empty with a producer that sends 1-row batches in a loop for 300 ms, and assert from a reader connection that rows were committed before the producer stops (DB-16).
  - Writer unit test: force a constraint failure (a `Folders` batch with a duplicate `id` and different `(parent, name)`), then `flush()` returns `WriterDied` while `shutdown()` returns `CoreError::Sqlite(..)`.
  - `merge_writer_result` table test over the 5 arms.
  - Pipeline test: a `Services` with a token that cancels after N files in `pass_full`; assert `run_scan` returns `Cancelled` and the rows hashed before the cancel are committed (needs T3.3).
  - Drop test: drop a `WriterHandle` with a pending batch and no `shutdown`; assert the row is committed.
- **Risk / follow-ups:** `Drop` joins, so a writer stuck in a long statement delays unwinding. That is the intended trade (a commit beats a detached thread killed at exit). The CLI's second Ctrl-C (T3.7) is the escape hatch.

---

### T3.2 Phase marker shares the transaction with stale deletion (closes DB-06, PIPE-06)

- **Verification:** DB-06 and PIPE-06 Confirmed, same bug. `pass_enumerate.rs:143-148` sends `PhaseComplete{Enumerated}` and flushes; only then does `:161` call `rescan::delete_stale`, which sends `DeleteStale` and flushes (`rescan.rs:15-20`). `scan.rs:102` skips `pass_enumerate` when `phase >= Enumerated`, so a crash between the two flushes skips stale deletion for good. The prefix over-budget case (`pass_prefix.rs:105-121`: `PhaseComplete{PrefixDone}` flushed at `:114`, `UniquePrefix` sent at `:121`) is the same pattern; it is closed by T3.3.
- **Fix:** send `DeleteStale` before `PhaseComplete`, with no flush in between. The writer applies batches in order, and the `PhaseComplete` arm (`writer.rs:165-170`) pushes the marker and commits all of `pending` in one transaction, so `DeleteStale` and the marker are atomic. The recursive CTE runs inside the RW transaction and sees the `Folders`/`Enumerated` upserts that precede it, whether already committed or still pending. A crash before that commit leaves the phase at `None`, so resume re-runs enumeration, which is an idempotent upsert, then deletion. This also matches plan P1.4.2 ("Enumerated = pass_enumerate done (incl. rescan merge + stale deletion)").
- **Rejected alternatives:** a separate `StaleDeleted` phase adds a `Phase` value and resume branch for no gain over one transaction. Re-running `delete_stale` on resume when `phase == Enumerated` needs `root_folder_ids` to be recomputed on resume and still leaves a window.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/rescan.rs
/// Queue deletion of files/folders not seen this run, under the given root folder ids.
/// No flush: the caller sends `PhaseComplete` next, so both commit in one transaction.
pub fn delete_stale(ctx: &Ctx, volume_id: i64, root_folder_ids: &[i64]) -> Result<()> {
    if root_folder_ids.is_empty() {
        return Ok(());
    }
    ctx.writer.send(Batch::DeleteStale {
        run_id: ctx.run_id,
        volume_id,
        root_folder_ids: root_folder_ids.to_vec(),
    })
}

// crates/core/src/pipeline/pass_enumerate.rs, end of `run` (before)
//     ctx.writer.send(Batch::PhaseComplete { .. Enumerated })?;
//     ctx.writer.flush()?;
//     emit_finished(..);
//     rescan::delete_stale(ctx, v.volume_id, &root_folder_ids)?;
// (after)
    // Stale deletion rides in the same transaction as the phase marker (4.7a step 2).
    rescan::delete_stale(ctx, v.volume_id, &root_folder_ids)?;
    ctx.writer.send(Batch::PhaseComplete {
        run_id: ctx.run_id,
        volume_id: v.volume_id,
        phase: crate::model::Phase::Enumerated,
    })?;
    ctx.writer.flush()?;
    emit_finished(/* unchanged */);
```
- **Tests to add:** writer-level test: commit `[Enumerated(new rows), DeleteStale, PhaseComplete]` with a stale row present and assert the stale row is gone and the phase is set in one transaction. Fault-injection test with `spawn_writer_with` where `DeleteStale` fails (for example, a folder id list that triggers an error): assert `phase_reached` stays below `Enumerated`.
- **Risk / follow-ups:** the commit carrying the marker can take longer on large trees (the CTE). That only delays the commit; it does not widen any loss window.

---

### T3.3 Stream pass results in bounded chunks; classify prefix singletons in SQL with the phase marker (closes PIPE-04; also the prefix half of DB-06)

- **Verification:** PIPE-04 Confirmed. `pass_prefix.rs:56-58` (`small`, `large`, `errors`) and `outcomes` at `:59` are filled for the whole pass and sent only at `:105-141`. `pass_full.rs:83-103` sends one `FullHashed` after `hash_sequential` / `hash_pooled` return; the pooled path turns a cancel into `Err(Cancelled)` at `:174` and drops every finished result. `pass_verify.rs:61-62, :99-104` buffer `verified` and `errors` for the whole pass. `pass_size.rs` also buffers, but it does no I/O (pure SQL plus a set lookup), so redoing it after a crash is cheap: out of scope. Plan line 518: "Kill -9 and restart must lose at most one batch window."
- **Fix:**
  - **Shared helper.** `pipeline/mod.rs` gets `RESULT_CHUNK = 5_000` and a `send_chunk` function that sends a `Vec` and leaves it empty once it reaches the chunk size. Each pass pushes results as they are produced and calls it. The writer still decides when to commit (50k rows or 5 s after T3.1), so the loss bound becomes "one writer window plus at most 5k unsent rows". Per-id transitions are idempotent and resume selects by state, so partial commits are safe.
  - **pass_full.** The sequential path pushes into `hashed`/`errors` and calls `send_chunk` per row. In the pooled path, `plan_rows` pushes `Reused`/`Failed` results directly (no `Slot` vector). `hash_parallel` takes an `on_result` callback that runs on the calling thread inside the `rx` loop. That thread may borrow `PathCache`/`ctx.read`, because only spawned closures need `Send`. The callback judges the result and pushes it. `Job` carries the `RawPath` it was planned with, so judging needs no cache lookup. On cancel or a callback error, set `abort`, leave the loop, send the partial chunks, and return the error.
  - **Ordering note.** Results now reach the writer in completion order, not row order. The final DB state does not depend on depth (each `UPDATE` is by id), but the module doc at `pass_full.rs:5-7` must be reworded. Any test that compares batch order rather than DB state would need adjusting.
  - **pass_prefix.** `small` and `large` (as `PrefixHashed`, state 2) are streamed. Hardlink siblings are handled inside the main loop: the representative always has a lower row index (`hardlink_reps` takes the first occurrence after `Scheduler::order`), so its outcome is already known. Keep `outcomes` only for indices that are representatives (`rep_of.values()`), which cuts memory. Unique-prefix classification moves to one writer-side statement sent right before `PhaseComplete{PrefixDone}`, so the downgrade 2 -> -4 and the marker commit atomically. That removes the in-memory `MemoryIndex` and the over-budget branch from this pass, and with them the "marker before `UniquePrefix`" ordering bug. After a crash mid-pass, the rows already committed in state 2 still take part in the final classification, because it is computed over every state-2 row of this run. The current in-memory classification would leave them unclassified.
  - **pass_verify.** Per group, push error rows before verified ids and flush both buffers at group boundaries when either buffer is full. The writer commits only between messages, so if a commit lands between `Error(chunk)` and `Verified(chunk)`, the committed part is the errors (terminal -2). On resume, the group's verified members are re-selected on their own, and they are byte-equal to each other, so they verify again. The opposite order is unsafe: a committed `Verified` with lost errors leaves a lone mismatched member in state 3, and `verify_group` (`pass_verify.rs:133-135`) promotes a singleton to `Verified` without comparing. This hazard exists today whenever `Verified` alone reaches 50k rows (`writer.rs:174`), so the ordering rule fixes a latent bug too.
- **Rejected alternatives:**
  - Keep `MemoryIndex` and downgrade the uniques at the end from memory: after a resume, the memory index does not hold the hashes of rows committed before the crash unless it is re-seeded. `rows_with_prefix` does seed it, but the SQL statement is simpler, is already the over-budget path (`queries::singleton_prefixes`), and is atomic with the marker.
  - A combined `Batch::VerifyChunk { verified, errors }` variant: explicit, but it adds a variant to fix ordering that a one-line rule plus a comment already guarantees. Acceptable if reviewers prefer explicitness.
  - Commit per file: too many transactions. The writer already batches.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/mod.rs
/// Rows per streamed result chunk. A crash loses at most one writer window plus
/// this many unsent rows (plan line 518).
pub(crate) const RESULT_CHUNK: usize = 5_000;

/// Send `buf` as one batch once it holds `RESULT_CHUNK` rows (or always, with `force`).
pub(crate) fn send_chunk<T>(
    ctx: &Ctx,
    buf: &mut Vec<T>,
    force: bool,
    wrap: fn(Vec<T>) -> Batch,
) -> Result<()> {
    if !buf.is_empty() && (force || buf.len() >= RESULT_CHUNK) {
        ctx.writer.send(wrap(std::mem::take(buf)))?;
    }
    Ok(())
}
```
```rust
// crates/core/src/db/writer.rs: new variant replacing UniquePrefix in pass_prefix
    /// Move this run's state-2 rows whose prefix hash is unique to -4. Sent right
    /// before PhaseComplete{PrefixDone} so both commit in one transaction.
    MarkPrefixSingletons { volume_id: i64, run_id: i64 },

// in `apply`
        Batch::MarkPrefixSingletons { volume_id, run_id } => {
            tx.prepare_cached(
                "UPDATE files SET state=-4 \
                 WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2 \
                 AND prefix_hash IN ( \
                   SELECT prefix_hash FROM files \
                   WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2 \
                     AND prefix_hash IS NOT NULL \
                   GROUP BY prefix_hash HAVING COUNT(*)=1)",
            )?
            .execute(params![volume_id, run_id])?;
        }
// row_count(): 0 for MarkPrefixSingletons.
```
```rust
// crates/core/src/pipeline/pass_prefix.rs (core of the new `run`)
    let rep_of = hardlink_reps(&rows);
    let reps: HashSet<usize> = rep_of.values().copied().collect();
    let mut outcomes: HashMap<usize, HashOutcome> = HashMap::new(); // reps with siblings only
    let mut small: Vec<(i64, Hash, Hash)> = Vec::new();
    let mut large: Vec<(i64, Hash)> = Vec::new();
    let mut errors: Vec<ErrRow> = Vec::new();
    let small_wrap: fn(Vec<(i64, Hash, Hash)>) -> Batch = if cfg.verify == VerifyMode::Byte {
        Batch::HashedPending
    } else {
        Batch::Hashed
    };

    let res = (|| -> Result<()> {
        for (i, r) in rows.iter().enumerate() {
            ctx.svc.cancel.check()?;
            emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
            let outcome = match rep_of.get(&i) {
                // The representative has a lower index, so its outcome is known.
                Some(rep) => match outcomes.get(rep) {
                    Some(o) => sibling_outcome(ctx, &mut cache, r, o)?,
                    None => continue, // rep vanished (cannot happen); sibling stays in state 1
                },
                None => hash_row(ctx, &mut cache, r)?, // propagates Cancelled (T3.5)
            };
            match &outcome {
                HashOutcome::Small(p, f) => small.push((r.id, *p, *f)),
                HashOutcome::Large(p) => large.push((r.id, *p)),
                HashOutcome::Err(e) => errors.push(e.clone()),
            }
            if reps.contains(&i) {
                outcomes.insert(i, outcome);
            }
            send_chunk(ctx, &mut small, false, small_wrap)?;
            send_chunk(ctx, &mut large, false, Batch::PrefixHashed)?;
            send_chunk(ctx, &mut errors, false, Batch::Error)?;
        }
        Ok(())
    })();
    // Hand over finished work even when the loop stopped (cancel, I/O error).
    send_chunk(ctx, &mut small, true, small_wrap)?;
    send_chunk(ctx, &mut large, true, Batch::PrefixHashed)?;
    send_chunk(ctx, &mut errors, true, Batch::Error)?;
    res?;

    ctx.writer.send(Batch::MarkPrefixSingletons { volume_id, run_id: ctx.run_id })?;
    ctx.writer.send(Batch::PhaseComplete { run_id: ctx.run_id, volume_id, phase: Phase::PrefixDone })?;
    ctx.writer.flush()?;

/// A hardlink sibling copies its representative's outcome (4.4a rule 3).
fn sibling_outcome(
    ctx: &Ctx,
    cache: &mut PathCache<'_>,
    r: &FileEntry,
    rep: &HashOutcome,
) -> Result<HashOutcome> {
    Ok(match rep {
        HashOutcome::Err(e) => {
            let mut e2 = e.clone();
            e2.file_row_id = Some(r.id);
            e2.path = Some(cache.file_path(r.folder_id, &r.name)?);
            HashOutcome::Err(e2)
        }
        other => other.clone(),
    })
}
```
```rust
// crates/core/src/pipeline/pass_full.rs (pooled path; the sequential path is the same
// buffers + send_chunk, with `hash_full_target(.., ctx.svc.cancel)`).
struct Job {
    idx: usize,
    raw: RawPath,     // for the error row; no cache lookup when judging
    path: PathBuf,
    size: u64,
    file_id: Option<Vec<u8>>,
}

struct Out {
    hashed: Hashed,
    errors: Vec<ErrRow>,
    pending_wrap: fn(Hashed) -> Batch, // FullHashed or FullHashedPending
}

fn flush_out(ctx: &Ctx, out: &mut Out, force: bool) -> Result<()> {
    send_chunk(ctx, &mut out.hashed, force, out.pending_wrap)?;
    send_chunk(ctx, &mut out.errors, force, Batch::Error)
}

fn hash_pooled(ctx: &Ctx, volume_id: i64, rows: &[FileEntry], cache: &mut PathCache<'_>,
               depth: usize, out: &mut Out) -> Result<()> {
    let jobs = plan_rows(ctx, volume_id, rows, cache, out)?; // pushes Reused/Failed into `out`
    let res = hash_parallel(ctx, &jobs, depth, &mut |job, wr| {
        emit_advance(ctx.svc.sink, ctx.run_id, volume_id, 1);
        let r = &rows[job.idx];
        match wr.hash {
            // A read stopped by Ctrl-C is not a file error: leave the row in state 2.
            Err(CoreError::Cancelled) => return Ok(()),
            _ => {}
        }
        match judge(ctx, r, &job.raw, wr) {
            Ok(h) => out.hashed.push((r.id, h)),
            Err(row) => out.errors.push(row),
        }
        flush_out(ctx, out, false)
    });
    flush_out(ctx, out, true)?;
    res?;
    ctx.svc.cancel.check() // a cancel that skipped jobs still ends the pass
}

/// Workers hash; `on_result` runs on this thread in completion order.
fn hash_parallel(
    ctx: &Ctx,
    jobs: &[Job],
    depth: usize,
    on_result: &mut dyn FnMut(&Job, WorkerResult) -> Result<()>,
) -> Result<()> {
    let reader = ctx.svc.reader;
    let cancel = ctx.svc.cancel;
    let chunk = ctx.cfg.profile.buf_size;
    let next = AtomicUsize::new(0);
    let abort = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<(usize, WorkerResult)>();
    thread::scope(|s| -> Result<()> {
        for _ in 0..depth.min(jobs.len()) {
            // ... spawn_scoped(run_worker(reader, cancel, jobs, &next, &abort, chunk, &tx)) as today
        }
        drop(tx);
        for (k, wr) in rx {
            if let Err(e) = on_result(&jobs[k], wr) {
                abort.store(true, Ordering::Relaxed);
                return Err(e); // dropping rx makes the workers' next send fail; they exit
            }
        }
        Ok(())
    })
}
// run_worker sends `k` (the job index) instead of `job.idx`.
```
```rust
// crates/core/src/pipeline/pass_verify.rs (inside the group loop)
        for (r, verdict) in group.iter().zip(verdicts) {
            match verdict {
                None => verified.push(r.id),
                Some(e) => errors.push(e),
            }
        }
        // Errors first: a commit that splits the two batches must never leave a
        // lone mismatched member in state 3, which a resume would promote as a singleton.
        if verified.len() + errors.len() >= RESULT_CHUNK {
            send_chunk(ctx, &mut errors, true, Batch::Error)?;
            send_chunk(ctx, &mut verified, true, Batch::Verified)?;
        }
    // after the loop: same two sends with force, then PhaseComplete + flush.
    // On error/cancel inside the loop: send both buffers (errors first), then return.
```
- **Tests to add:**
  - `kill_fuzz`-style test (existing `crates/cli/tests/kill_fuzz.rs` harness): kill during `pass_full` after N files, then assert that at least `N - (writer window + RESULT_CHUNK)` rows are in state 5 before resume, and that the resumed report matches `--no-db`.
  - Cancel test via the token: cancel inside `pass_full` (pooled and sequential), assert committed state-5 rows > 0 and no `-2` rows with a "cancelled" message.
  - `MarkPrefixSingletons` SQL test: rows in state 2 with hashes {A, A, B}, plus one row of another run with hash B; assert only the B row of this run moves to -4.
  - Prefix resume test: stop after half the rows were committed in state 2 (simulate with a fixture), run the pass again, assert that singletons among the pre-committed rows end up in -4.
  - Verify split test: commit `Error(g)` only, then re-run `pass_verify`; assert the remaining members end in 4 and no mismatched member is promoted.
- **Risk / follow-ups:** the `MarkPrefixSingletons` subquery scans this run's state-2 rows via `idx_files_state`. That is O(n log n) in SQLite and well within the cost of the hashing pass, but measure it in the 1M-row scale test (`DEDUPE_SCALE_N`). Removing `MemoryIndex` from `pass_prefix` overlaps with PIPE-10 (prefix memory): coordinate with whichever topic owns PIPE-10 so the two changes do not conflict. Prefix grouping by hash only, not `(size, hash)`, is unchanged here.

---

### T3.4 `--force` wipe resets the run phases and groups, in one transaction (closes PIPE-05)

- **Verification:** Confirmed. `resume.rs:38-41` runs `UPDATE files SET ... state=0` on every row but leaves `run_volumes.phase_reached` alone. `plan_resume` (`resume.rs:89`) then reads the old phases, and `scan.rs:110-133` skips every pass whose phase is already reached. With phase >= `SizeDone`, nothing consumes state 0, `recompute_groups` finds no 3/4/5 rows, and the run finishes empty. Two related problems: (a) the wipe also resets `-1` (skipped: symlinks, placeholders) to 0, so `pass_size` would classify skipped rows as candidates; (b) the wipe and `init_meta` are separate autocommit statements, so a crash between them leaves a wiped DB that still has the old meta.
- **Fix:** in one `IMMEDIATE` transaction, wipe the hash columns for every row except skipped ones, lower `phase_reached` to at most `Enumerated` for running runs (state 0 is exactly what `Enumerated` produced, and stale deletion already happened if the phase was at least `Enumerated`), delete `dup_groups`, then rewrite meta. Use `MIN(phase_reached, 1)` rather than 0, so a run that never finished enumeration keeps its lower phase.
- **Rejected alternatives:** refusing `--resume --force` together: safe, but it makes the documented flag combination useless. Resetting to `Phase::None`: forces a re-walk with no benefit.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/resume.rs, inside `if mismatch { if cfg.force { ... } }`
            tracing::warn!("hash config changed; wiping hashes because --force was given");
            let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE files SET prefix_hash=NULL, full_hash=NULL, prefix_bytes=NULL, \
                 state=0, err=NULL WHERE state <> -1",
                [],
            )?;
            // State 0 is what pass_enumerate produced, so no running run may claim more.
            tx.execute(
                "UPDATE run_volumes SET phase_reached=MIN(phase_reached, ?1) \
                 WHERE run_id IN (SELECT id FROM scan_runs WHERE status='running')",
                [Phase::Enumerated.as_i64()],
            )?;
            tx.execute("DELETE FROM dup_groups", [])?;
            init_meta(&tx, cfg, now_unix_ns())?; // &Transaction derefs to &Connection
            tx.commit()?;
```
`Transaction::new_unchecked` is needed because `check_config_guard` takes `&Connection`. Alternatively change the signature to `&mut Connection` and use `conn.transaction_with_behavior(TransactionBehavior::Immediate)`; both callers own the connection mutably. The latter is cleaner.
- **Tests to add:** integration test: scan to `--stop-after-pass` FullDone (test-hooks feature), change `prefix_first_bytes`, run `scan --resume --force`, and assert that the report equals the `--no-db` result and `phase_reached` went through every phase. Unit test: a skipped row (-1) stays -1 after a forced wipe.
- **Risk / follow-ups:** finished runs keep their old `phase_reached = GroupsDone`. That is harmless, because only running runs are resumed, and `report` reads `dup_groups`, which is now empty until the next scan. Say so in the `--force` help text.

---

### T3.5 Cancellation inside read loops, Cancelled never becomes a file error, worker panics isolated (closes PIPE-08, PIPE-09)

- **Verification:**
  - PIPE-08 Confirmed. `hash.rs:144-153` loops over chunks with no token. `pass_verify.rs:309-320` does the same. `pass_full.rs:279` checks only between jobs. `naive.rs:73-74` checks once per root, and the walk closure (`naive.rs:74-110`) has no check, unlike `pass_enumerate.rs:80`.
  - PIPE-09 Confirmed. Workers run `hash_job` (`pass_full.rs:293-303`) with no `catch_unwind`. The other workers keep claiming jobs (`:279`), `abort` is set only on spawn failure (`:251`), the `rx` loop drains everything, and `thread::scope` re-raises the panic at `:244`. The sequential path (`:118-158`) and `pass_prefix::hash_row` unwind straight out of the pass. `[profile.release]` in `Cargo.toml:50-52` does not set `panic = "abort"`, so `catch_unwind` is effective.
- **Fix:**
  1. `hash_full_target(reader, target, size, chunk, cancel: &Token)` checks `cancel.check()?` once per chunk. An atomic load per 1 MiB-class read is negligible. `hash_full` gains the same parameter. `bench` passes `&Token::new()`.
  2. `verify_pair` gets `cancel: &Token` and checks it per chunk, returning `Err((Side::Reference, CoreError::Cancelled))`.
  3. **Critical:** every place that turns a read error into an `ErrRow` must propagate `Cancelled` instead of recording a `-2` row. Otherwise a Ctrl-C marks the file as errored and resume never retries it. The sites are `pass_prefix::hash_row` (`:196-216`), `pass_full::hash_sequential` (`:148-155`), `judge` (`:346-350`, via the pooled callback in T3.3), and `pass_verify::compare_pair` (`:247-250`). Add a small guard helper.
  4. `scan_naive`: `cancel.check()?` at the top of the walk closure and in the hash loop. The existing checks at `:130/:155` stay.
  5. Panics: `hash_job` runs under `catch_unwind(AssertUnwindSafe(..))`. A panic becomes `WorkerResult { hash: Err(CoreError::Other("reader panicked: ..")), .. }`, so that file gets a `-2` row with the panic text (`ErrorKind::Other`) and the scan continues. The sequential paths use the same wrapper. A panic is per file and deterministic, so a fatal abort would also make every `--resume` panic on the same file. Recording it as a per-file error is what keeps the scan resumable.
- **Rejected alternatives:**
  - Abort the pass on the first panic (set `abort`, return `CoreError`): fixes "hours of wasted hashing" but makes the run un-resumable when the panic is deterministic for one file.
  - Joining the handles explicitly instead of `scope`: still needs `catch_unwind` to avoid wasting the other workers' results. `scope` is fine.
  - Checking the token every N chunks: no measurable saving over a relaxed or SeqCst load per chunk.
- **Boilerplate:**
```rust
// crates/core/src/hash.rs
pub fn hash_full_target(
    reader: &dyn ContentReader,
    target: ReadTarget<'_>,
    size: u64,
    chunk: usize,
    cancel: &Token,
) -> Result<Hash> {
    let chunk = chunk.max(1);
    let mut handle = reader.open_target(target)?;
    let mut h = blake3::Hasher::new();
    let mut offset = 0u64;
    let mut buf = vec![0u8; chunk];
    while offset < size {
        cancel.check()?; // bounded Ctrl-C latency: one chunk, not one file
        let want = usize::try_from(size - offset).map_or(chunk, |w| w.min(chunk));
        let n = handle.read_at(offset, &mut buf[..want])?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        offset += n as u64;
    }
    Ok(finish(h))
}
```
```rust
// crates/core/src/pipeline/pass_prefix.rs (shared by full/verify)
/// A read stopped by the cancel token is not a file error: propagate it so the
/// row keeps its state and resume retries it.
pub(crate) fn file_err_or_cancel(
    ctx: &Ctx,
    r: &FileEntry,
    path: &RawPath,
    e: CoreError,
) -> Result<ErrRow> {
    match e {
        CoreError::Cancelled => Err(CoreError::Cancelled),
        e => Ok(io_err(ctx, r, path, e)),
    }
}
// usage in hash_row:  Err(e) => Ok(HashOutcome::Err(file_err_or_cancel(ctx, r, &path, e)?)),
// usage in compare_pair:
//     Err((_, CoreError::Cancelled)) => return Err(CoreError::Cancelled),
```
```rust
// crates/core/src/pipeline/pass_full.rs
fn hash_job(reader: &dyn ContentReader, job: &Job, chunk: usize, cancel: &Token) -> WorkerResult {
    let run = || {
        let target = ReadTarget {
            path: &job.path,
            file_id: job.file_id.as_deref(),
            access: AccessPattern::Sequential,
        };
        WorkerResult {
            hash: hash_full_target(reader, target, job.size, chunk, cancel),
            meta: reader.metadata(&job.path),
        }
    };
    // A backend panic is one file's error, not the scan's. AssertUnwindSafe: the
    // only shared state is the reader; a poisoned lock in it surfaces as later errors.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).unwrap_or_else(|p| {
        let msg = crate::db::writer::panic_message(p.as_ref());
        tracing::error!(path = %job.path.display(), "hash worker panicked: {msg}");
        WorkerResult {
            hash: Err(CoreError::Other(format!("reader panicked: {msg}"))),
            meta: Err(CoreError::Other("not read".into())),
        }
    })
}
```
```rust
// crates/core/src/pipeline/naive.rs:74
        enumr.walk(root, cfg, &mut |item| {
            cancel.check()?;
            // ...
```
- **Tests to add:**
  - `hash_full_target` with a pre-cancelled token returns `Cancelled` before reading; with a fake reader that cancels the token from inside `read_at`, it returns `Cancelled` after one chunk.
  - Pass test: cancel mid-file in `pass_full` and assert no `errors` row with "cancelled" and the row is still in state 2.
  - Panic test: a fake `ContentReader` that panics on one path. Assert the pass completes, that file is `-2` with "reader panicked", and the others are 5 (pooled and sequential paths).
  - `scan_naive` with a token cancelled by the visitor after 10 items returns `Cancelled` before the walk ends.
- **Risk / follow-ups:** `catch_unwind` does not make an unwind safe through Windows overlapped I/O with in-flight kernel writes into a buffer being dropped. That soundness belongs with WIO-06 (backend `Drop` must `CancelIoEx` and wait). The default panic hook still prints to stderr over the progress bar, which is acceptable. `core::lookup::lookup` needs `cancel: &Token` for its `hash_full` calls (`lookup.rs:131, :143`); see T3.7.

---

### T3.6 Inter-process lock on a sidecar lock file (closes DB-04)

- **Verification:** Confirmed. `IdAllocator::seed` reads `MAX(id)+1` once (`db/ids.rs`). `run_scan` abandons other running runs (`scan.rs:61-64`) but cannot stop them. `finish_run` (`queries.rs:730-736`) has no `status='running'` guard, so an abandoned run flips itself back to `finished`. Nothing in `Db::open` (`db/mod.rs:23-28`) takes an exclusive lock. `lookup --insert` seeds a private allocator (`lookup.rs:269`).
- **Toolchain check:** `std::fs::File::{lock, lock_shared, try_lock, try_lock_shared, unlock}` and `std::fs::TryLockError { Error(io::Error), WouldBlock }` were stabilized in Rust 1.89.0, to my knowledge (from the 1.89 release notes as I recall them; not re-checked online in this session, so confirm against the docs). The toolchain is pinned to 1.92.0 (`rust-toolchain.toml`), so it compiles. The workspace declares `rust-version = "1.83"` (`Cargo.toml:16`), which **must be bumped to "1.89"**, or the declared MSRV is false. std implements this with `flock(LOCK_EX|LOCK_NB)` on Unix and `LockFileEx(LOCKFILE_EXCLUSIVE_LOCK|LOCKFILE_FAIL_IMMEDIATELY)` on Windows. It is safe code, with no `cfg` and no `unsafe`, so it fits `crates/core`. Platforms without support return an `io::Error` of kind `Unsupported`.
- **Fix:**
  - Add `db/lock.rs` with an RAII `DbLock` holding an exclusively locked `<db>.lock` file. The OS drops the lock when the process dies, including `kill -9`, so a crashed scan never leaves a stale lock.
  - Acquire with `try_lock` and fail fast with a new `CoreError::DbBusy(path)`: "another dedupe process is using <db>; wait for it or stop it".
  - Take the lock in `run_scan` before `Db::open`, and hold it through the post-shutdown recompute and `finish_run`. Also take it in `lookup --insert`, `db vacuum` and `db drop` (CLI). `report`, read-only `lookup` and `db info` stay lock-free: WAL readers are safe.
  - Never delete the lock file while running. Deleting it would let a third process lock a new inode while the first still holds the old one. `db drop` removes it after acquiring it.
  - Belt and braces: `finish_run` only flips `status='running'` rows and warns if 0 rows changed.
- **Why a sidecar file and not the DB file:** on Windows, `LockFileEx` locks are mandatory byte-range locks, so locking the `.db` itself would make SQLite's own reads and writes on other handles fail with `ERROR_LOCK_VIOLATION`. On Unix, `flock` and SQLite's `fcntl` locks would coexist, but one uniform scheme is simpler.
- **Rejected alternatives:**
  - `PRAGMA locking_mode=EXCLUSIVE` on the writer: in WAL mode it blocks every other connection, including the pipeline's own read-only connection (`scan.rs:75`), so the design breaks.
  - A lock row in `meta` claimed in `BEGIN IMMEDIATE` with pid plus heartbeat: after `kill -9` the row is stale, and checking whether the pid is alive needs platform code (`cfg` or `unsafe`) and is racy with pid reuse.
  - The `fs4` crate: equivalent, but it adds a dependency for what std 1.89+ already provides.
- **Boilerplate:**
```rust
// crates/core/src/db/lock.rs
//! Inter-process exclusion for read-write commands: an OS advisory lock on
//! `<db>.lock`, released automatically when the process exits or is killed.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{CoreError, Result};

/// Held for the lifetime of a read-write command. Dropping it releases the lock.
#[must_use = "the lock is released when this guard is dropped"]
pub struct DbLock {
    _file: File,
}

pub fn lock_path(db: &Path) -> PathBuf {
    let mut os = db.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// Take the exclusive lock or fail fast with `DbBusy`.
pub fn acquire(db: &Path) -> Result<DbLock> {
    let path = lock_path(db);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    match file.try_lock() {
        Ok(()) => Ok(DbLock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(CoreError::DbBusy(path.display().to_string())),
        Err(TryLockError::Error(e)) if e.kind() == io::ErrorKind::Unsupported => {
            tracing::warn!(path = %path.display(), "file locking unsupported; continuing unlocked");
            Ok(DbLock { _file: file })
        }
        Err(TryLockError::Error(e)) => Err(e.into()),
    }
}
```
```rust
// crates/core/src/error.rs
    #[error("database is in use by another dedupe process (lock: {0})")]
    DbBusy(String),

// crates/core/src/pipeline/scan.rs, first line of run_scan
    let _lock = db::lock::acquire(db_path)?; // held until run_scan returns

// crates/core/src/db/queries.rs
pub fn finish_run(conn: &Connection, run_id: i64, now: i64) -> Result<()> {
    let n = conn.execute(
        "UPDATE scan_runs SET status='finished', finished_at=?1 WHERE id=?2 AND status='running'",
        params![now, run_id],
    )?;
    if n == 0 {
        tracing::warn!(run_id, "run was no longer running; not marked finished");
    }
    Ok(())
}

// crates/core/src/pipeline/pass_enumerate.rs:32 skip set
        for suf in ["-wal", "-shm", ".lock"] {
```
```rust
// crates/cli/src/db.rs (vacuum / drop) and crates/cli/src/lookup.rs (--insert)
    let _lock = dedupe_core::db::lock::acquire(&db).context("lock database")?;
// drop: remove the db, -wal, -shm, then `.lock` while still holding the guard.
```
- **Tests to add:** two `acquire` calls on the same path from one process. Note that `flock` locks are per open file description, so a second `File` in the same process conflicts and returns `DbBusy` on Linux and macOS. Windows `LockFileEx` behaves the same per handle. Then drop the first and assert the second succeeds. Process-level test: start `scan` on a slow root in a child (or hold the lock in the test), run a second `scan` on the same DB, and assert a non-zero exit with "in use". `finish_run` on an `abandoned` run leaves it `abandoned`.
- **Risk / follow-ups:**
  - The CLI's post-scan `set_run_enumerator` write (`cli/src/scan.rs:176-178`) runs after `run_scan` dropped the lock. It is a single `UPDATE ... WHERE id=run_id`, which another process cannot corrupt, but moving it into `ScanOptions`/`run_scan` would close the gap fully.
  - `flock` over NFS may be local-only on older kernels. WAL on network filesystems is already unsupported (`WalUnavailable`).
  - `.dedupe.db.lock` files under other scanned roots will be indexed like any file: same open issue as `.dedupe.db*` (separate finding).

---

### T3.7 Two-stage Ctrl-C and exit code 130 (closes CLI-05, CLI-07)

- **Verification:**
  - CLI-05 Confirmed. `cli/src/main.rs:406-410` installs one global handler that only sets the token. `lookup::run` (`cli/src/lookup.rs:14`) and `db::run` (`cli/src/db.rs:11`) never receive it, and `core::lookup::lookup` (`core/src/lookup.rs:59-67`) has no cancel parameter. There is no escalation on a second press.
  - CLI-07 Confirmed. `main` returns `anyhow::Result<()>` (`main.rs:401`), so `Cancelled` prints `Error: scan failed / Caused by: cancelled` and exits 1. `report.rs:52-57` prints "output incomplete" and then still returns the error, also with exit 1.
- **Fix:**
  - One handler for the whole process, as `ctrlc` allows only one. The first press sets the token and prints a one-line notice. If the running command does not poll the token (`db vacuum`/`drop`/`info`), it exits 130 at once. That is safe because SQLite is crash-safe, and `VACUUM` is atomic. The second press always calls `std::process::exit(130)`.
  - `lookup` gets the token and passes it into `core::lookup::lookup` (and on to `hash_full`, per T3.5).
  - `main` returns `ExitCode`. It walks the error chain for `CoreError::Cancelled` and prints "cancelled" (for scan: "run `dedupe scan --resume` to continue"), exiting 130. Other errors print as `anyhow` does today (`Error: {e:?}`) and exit 1.
- **Rejected alternatives:**
  - Install the handler only for `scan`/`report`: `lookup` would then die mid-hash on the first Ctrl-C (fine), but scan-time stretches that do not poll would still have no escalation. The two-stage handler covers both.
  - Map exit codes inside each subcommand: duplicated logic.
- **Boilerplate:**
```rust
// crates/cli/src/main.rs
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

/// Exit code for a user cancel, matching the shell convention 128 + SIGINT.
const EXIT_CANCELLED: u8 = 130;

fn install_ctrlc(token: &Token, cooperative: bool) {
    let token = token.clone();
    let presses = Arc::new(AtomicUsize::new(0));
    let cooperative = Arc::new(AtomicBool::new(cooperative));
    let res = ctrlc::set_handler(move || {
        let n = presses.fetch_add(1, Ordering::SeqCst);
        if n == 0 && cooperative.load(Ordering::SeqCst) {
            token.cancel();
            eprintln!("\ncancelling, finishing the current batch (Ctrl-C again to quit now)");
        } else {
            // Committed work is safe: SQLite (WAL) recovers like after kill -9.
            std::process::exit(i32::from(EXIT_CANCELLED));
        }
    });
    if let Err(e) = res {
        tracing::warn!("Ctrl-C handler not installed: {e}");
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_level);
    let token = Token::new();
    // `db` subcommands never poll the token, so their first Ctrl-C exits at once.
    install_ctrlc(&token, !matches!(cli.cmd, Cmd::Db(_)));

    let res = match cli.cmd {
        Cmd::Scan(args) => scan::run(args, &token),
        Cmd::Report(args) => report::run(args, &token),
        Cmd::Lookup(args) => lookup::run(args, &token),
        Cmd::Db(args) => db::run(args),
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if is_cancelled(&e) => {
            eprintln!("cancelled");
            ExitCode::from(EXIT_CANCELLED)
        }
        Err(e) => {
            eprintln!("Error: {e:?}");
            ExitCode::FAILURE
        }
    }
}

fn is_cancelled(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|c| matches!(c.downcast_ref::<CoreError>(), Some(CoreError::Cancelled)))
}
```
```rust
// crates/core/src/lookup.rs
pub fn lookup(
    conn: &Connection,
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
    cfg: &Config,
    insert: bool,
    cancel: &Token,
) -> Result<LookupOutcome> { /* pass `cancel` to hash_full at :131 and :143 */ }
```
In `scan::run`, the `"scan failed"` context wraps the error, and `is_cancelled` walks the chain, so no change is needed there. A resume hint can be printed in `scan::run` when `is_cancelled` and the DB path is used.
- **Tests to add:** `crates/cli/tests`: run `scan` with the `test-hooks` feature and a hook that cancels the token (or send SIGINT to the child on Unix), then assert exit status 130, stderr contains "cancelled", and no "Error:". Unit test for `is_cancelled` through two layers of `.context()`. `lookup` with a pre-cancelled token returns `Cancelled`.
- **Risk / follow-ups:** `std::process::exit` skips destructors, so the writer does not checkpoint. That is acceptable, because WAL is recovered on the next open. Exit code 130 also lands on Windows, where it carries no SIGINT meaning but is still distinct. Document it in the README exit-code table. This depends on T3.1: without the writer join, a cooperative cancel still loses the queued batches.

---

### T3.8 Windows: observe cancel in the journal read, delta refresh, cache write and fallback (closes WIO-05, Low)

- **Verification:** Confirmed. `backend-windows/src/usn.rs:263-291` loops up to `MAX_READ_ROUNDS` with no cancel check. `enumerator.rs` `obtain_tree` calls `refresh_cached` / `apply_delta` with no token. `store_cache(site, &tree, notices)` runs before `emit_tree` with no cancel gate. The `MftFailure::Fallback` arm (`enumerator.rs` around lines 171-183 of the current file) starts `self.fallback.walk` without checking `self.cancel`.
- **Fix (short):**
  - At the top of the `Fallback` arm, add `if self.cancel.is_cancelled() { return Err(CoreError::Cancelled); }` before pushing the notice. This keeps the documented "a cancel never triggers the fallback walker".
  - In `walk_mft_volume`, skip `store_cache` when `cancel.is_cancelled()`. The tree may be complete, but a cancelled scan should not spend seconds writing a whole-volume file.
  - `read_journal(.., cancelled: &dyn Fn() -> bool)`: check per round and return a new `UsnError::Cancelled`. `load_cache_plan` maps it to `Plan::Full`, whose `read_tree` returns `Stop(Cancelled)` on its first poll.
  - In `obtain_tree`, check `cancel` before and after `refresh_cached`, returning `Err(MftFailure::Stop(CoreError::Cancelled))`. Optionally pass the same closure into `apply_delta` for per-entry checks.
```rust
// crates/backend-windows/src/enumerator.rs, Fallback arm
                    Err(MftFailure::Fallback(reason)) => {
                        if self.cancel.is_cancelled() {
                            return Err(CoreError::Cancelled);
                        }
                        // ... existing warn, notice, fallback walk
                    }
// walk_mft_volume
    if let Some(site) = site.as_ref().filter(|_| !unchanged && !cancel.is_cancelled()) {
        store_cache(site, &tree, notices);
    }
```
- **Tests to add:** portable unit test of `obtain_tree` with a fake `RecordSource` and a pre-cancelled token: it returns `Stop(Cancelled)` and does not call `refresh_cached`. The `read_journal` change is `cfg(windows)`: compile-check only via the cross-check commands, and run it in the `mft_vs_walk` session on Windows.
- **Risk / follow-ups:** Win32 paths are compile-checked only, as are all `cfg(windows)` items in this crate.

---

## T4. File identity and volume identity across platforms

**Findings covered:** GEN-01, DB-08, GEN-02, GEN-03, GEN-04, GEN-07, GEN-09, GEN-11, GEN-12, WIO-09, GEN-13, GEN-14, GEN-16, GEN-17, GEN-18, GEN-19, WIO-01, WIO-02, PIPE-11, CORE-19

**Root causes:**
- `FileMeta` has no device component and core uses bare `file_id` as the inode identity, so identity is only unique within one device, while both the naive pipeline (all roots under `volume_id 0`) and `--cross-mounts` (foreign devices under the root's `volume_id`) mix devices -> GEN-01, DB-08.
- Hardlink collapse is applied in some places (`pass_prefix` hashing, `recompute_groups`, `pass_verify`) but not in the prefix-partner count or `pass_full` -> PIPE-11.
- Windows identity is built in two different places with different fidelity (walker `fill_file_identity` vs `ContentReader::metadata`, 64-bit index only, no ReFS 128-bit id, creation time as ctime) -> GEN-07, GEN-11, and feeds WIO-01 (unchecked ids reach `OpenFileById`) and WIO-02 (second id resolution).
- Per-OS `identify` only handles the happy path: wrong getattrlist layout and wrong target path on macOS, no network/mapped/subst path on Windows, no zero-fsid guard on Linux, no prefix normalisation between `canonicalize` output and the mount -> GEN-02, GEN-03, GEN-04, GEN-09, GEN-14, GEN-18, GEN-19, CORE-19.
- No single time-conversion convention shared by the walker and the `$MFT` path -> GEN-12, WIO-09. Plus small read-path robustness gaps -> GEN-13, GEN-16, GEN-17.

**Recommended order of work:**
1. T4.1 identity key (`FileMeta.dev`, naive key, foreign-device namespacing in `pass_enumerate`) - closes the two High correctness bugs.
2. T4.2 one Windows identity function (FileIdInfo, volume serial, ChangeTime) used by walker and `metadata()` - GEN-07, GEN-11, and part of GEN-01 on Windows. Must land together with the `$MFT` ctime change in the same commit.
3. T4.3 hardlink-aware prefix classification and `pass_full` collapse (PIPE-11), which builds on the identity key from 1.
4. T4.4 macOS getattrlist (GEN-02, GEN-14 part), T4.5 Linux zero fsid (GEN-04), T4.6 Windows identify rewrite (GEN-03, GEN-09, GEN-18, GEN-19 mount part).
5. T4.7 open-by-id hardening (WIO-01, WIO-02).
6. T4.8 shared time convention (GEN-12, WIO-09).
7. Low items: T4.9 (GEN-13, GEN-16), T4.10 (GEN-17), T4.11 (GEN-19 codec), T4.12 (CORE-19).

---

### T4.1 Device-aware identity key: `FileMeta.dev`, naive key, foreign-device namespacing (closes GEN-01, DB-08)

- **Verification:**
  - GEN-01 Confirmed. `crates/backend-generic/src/meta.rs:50` stores `md.ino()` only. Windows `crates/backend-generic/src/lib.rs:146-151` reads `BY_HANDLE_FILE_INFORMATION` but keeps only `nFileIndexHigh/Low`, `dwVolumeSerialNumber` is dropped. `crates/core/src/traits.rs:20-30` `FileMeta` has no device field. Naive: `crates/core/src/pipeline/naive.rs:38-56` keys `distinct_count` on `file_id` alone, and `naive.rs:106` sets `volume_id: 0` for every root, so `scan --no-db /mnt/a /mnt/b` collapses `(dev a, ino 12)` and `(dev b, ino 12)` into one inode and drops the pair at the size pass (`naive.rs:121-125`).
  - DB-08 Confirmed (same root cause). `crates/core/src/db/queries.rs:303-314` (`size_candidates`) and `queries.rs:590-605` (`recompute_groups`) use `COUNT(DISTINCT COALESCE(file_id, id))` per `volume_id`. With `--cross-mounts` (`one_file_system=false`, `crates/backend-generic/src/walk.rs:25-27`) every file below a nested mount is written under the root's `volume_id` (`crates/core/src/pipeline/pass_enumerate.rs:108-117`), so btrfs subvolumes (inodes 256/257 repeat) collide. Move detection (`pass_enumerate.rs:237-258`, `find_move_candidate` by `(volume_id, file_id)` + size + mtime) and `hardlink_reps` (`pass_prefix.rs:222-235`, hashes one rep and copies the hash to "siblings") are also fooled, and the latter copies a hash to a different file: this is a wrong-result bug, not just a missed group.
  - Windows walker cannot cross volumes: `std` reports name-surrogate reparse points (junctions, volume mount points) as `is_symlink()`, and the walker uses `follow_links(false)`; the `$MFT` path is single-volume. So the DB-side collision is Unix-only; the naive multi-root collision affects every OS. (std behaviour recalled from `library/std/src/sys/pal/windows/fs.rs`, not re-checked here.)
- **Fix:** separate two notions that today share one field.
  1. `file_id` stays the backend's *per-volume, stable* native id (inode / NTFS FRN / ReFS 128-bit id). It is persisted, used for rescan reuse, move detection and open-by-id. Unchanged for every file that lives on the root's own device, so existing DBs keep working and nothing is re-hashed.
  2. New runtime-only `FileMeta.dev: Option<u64>`: the device the entry was observed on (`st_dev` on Unix, 32-bit volume serial on Windows). Never persisted as identity, because `st_dev` is not stable across reboot, USB re-plug or btrfs (anonymous dev numbers).
  3. Naive pipeline keys hardlink identity on `(dev, file_id)`.
  4. `pass_enumerate`: when a file's `dev` differs from its scan root's `dev` (only possible with `--cross-mounts` on Unix), the stored `file_id` is namespaced: `tag(8) || native_id`, where `tag` = first 8 bytes of `blake3(foreign VolumeInfo.uuid)`, cached per foreign `dev`. The tag is stable across reboots (uuid, not dev), so rescan reuse and move detection still work for cross-mount files, and the 16-byte length means `file_id_to_frn` (8 bytes only, `crates/backend-windows/src/handle.rs:42`) never misreads it. No SQL change is needed: every `COALESCE(file_id, id)` query becomes correct because ids are now unique per `volume_id`. No schema bump.
- **Rejected alternatives:**
  - Fold `st_dev` into the persisted `file_id`: dev numbers change across reboot and re-plug, so every rescan of an external disk would re-hash everything and lose move detection.
  - New `dev` column (schema v3) and a two-column DISTINCT: same dev instability, needs a schema bump, and SQLite `COUNT(DISTINCT ...)` takes one expression anyway.
  - Give each nested mount its own `volume_id`: the correct long-term model, but touches folder chains, `run_volumes`, stale-deletion scoping and resume; out of scope for a bug fix (listed as follow-up).
  - Store `NULL` file_id for foreign rows: loses hardlink collapse on those mounts and over-reports wasted space.
- **Boilerplate:**
```rust
// crates/core/src/traits.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    /// Stable per-volume identity: inode / NTFS FRN / ReFS 128-bit id. `None` where absent.
    /// Unique only together with the volume; see `dev`.
    pub file_id: Option<Vec<u8>>,
    /// Device the entry was observed on this run: `st_dev` on Unix, the 32-bit volume
    /// serial on Windows. Runtime only, never persisted as identity (not stable across
    /// reboots). `None` when unknown.
    pub dev: Option<u64>,
    pub nlink: i64,
    // ... unchanged
}
```
```rust
// crates/backend-generic/src/meta.rs  (inside file_meta_from)
    let mut dev: Option<u64> = None;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        file_id = Some(md.ino().to_le_bytes().to_vec());
        dev = Some(md.dev());
        // ...
    }
    // Windows: `dev` is filled by `fill_file_identity` (T4.2).
    FileMeta { file_id, dev, nlink, /* ... */ }
```
```rust
// crates/core/src/identity.rs  (new, pure, no cfg; `pub mod identity;` in lib.rs)
//! Identity keys for hardlink collapse. `file_id` is unique per volume; these helpers
//! make it unique per run when entries from several devices are mixed.

use crate::traits::FileMeta;

/// Run-local hardlink key for the naive pipeline: `(dev, file_id)`. `None` when the
/// file has no id (callers fall back to a per-row key).
pub fn run_key(meta: &FileMeta) -> Option<Vec<u8>> {
    let fid = meta.file_id.as_ref()?;
    let mut k = Vec::with_capacity(9 + fid.len());
    match meta.dev {
        Some(d) => { k.push(1); k.extend_from_slice(&d.to_le_bytes()); }
        None => k.push(0),
    }
    k.extend_from_slice(fid);
    Some(k)
}

/// Stable 8-byte tag for a foreign volume, from its identity string.
pub fn volume_tag(uuid: &str) -> [u8; 8] {
    let h = blake3::hash(uuid.as_bytes());
    let mut t = [0u8; 8];
    t.copy_from_slice(&h.as_bytes()[..8]);
    t
}

/// `tag || native id`: a file_id that cannot collide with ids native to the root volume
/// (those are never 8 + n bytes with this tag) and that open-by-id never accepts.
pub fn namespaced_file_id(tag: [u8; 8], native: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + native.len());
    v.extend_from_slice(&tag);
    v.extend_from_slice(native);
    v
}
```
Note: an 8-byte native id can never equal a 16-byte namespaced id, but a 16-byte ReFS id (T4.2) could in theory equal `tag || 8-byte inode` only if a Unix foreign mount and a ReFS root mix, which cannot happen in one run (Windows walker never crosses volumes). Document that invariant next to `namespaced_file_id`.
```rust
// crates/core/src/pipeline/naive.rs
struct Rec {
    // ...
    /// `(dev, file_id)` from `identity::run_key`; `None` -> per-row key.
    ident: Option<Vec<u8>>,
}
impl Rec {
    fn inode_key(&self, fallback_idx: usize) -> Vec<u8> {
        match &self.ident {
            Some(b) => b.clone(),
            None => { let mut k = b"idx:".to_vec(); k.extend_from_slice(&fallback_idx.to_be_bytes()); k }
        }
    }
}
// in the walk closure:
            recs.push(Rec { path, volume_id: 0, size: meta.size, bytes,
                            ident: crate::identity::run_key(&meta) });
```
```rust
// crates/core/src/pipeline/pass_enumerate.rs
/// Per-root device and a cache of foreign-device tags (only populated with --cross-mounts).
struct DevScope {
    root_dev: Option<u64>,
    tags: HashMap<u64, [u8; 8]>,
}

impl DevScope {
    fn for_root(ctx: &Ctx, native_root: &Path) -> Result<Self> {
        // Directories carry `dev` on Unix; on Windows it is `None`, which disables
        // namespacing there (the walker cannot leave the volume).
        let root_dev = ctx.svc.reader.metadata(native_root)?.dev;
        Ok(Self { root_dev, tags: HashMap::new() })
    }

    /// Rewrites `meta.file_id` for an entry on a different device than its root.
    fn apply(&mut self, ctx: &Ctx, native: &Path, meta: &mut FileMeta) {
        let (Some(root), Some(dev)) = (self.root_dev, meta.dev) else { return };
        if dev == root { return; }
        let Some(fid) = meta.file_id.as_deref() else { return };
        let tag = *self.tags.entry(dev).or_insert_with(|| {
            match ctx.svc.volume.identify(native) {
                Ok(info) => crate::identity::volume_tag(&info.uuid),
                Err(e) => {
                    // Unstable but collision-free within this run: rescans of this mount re-hash.
                    tracing::warn!(dev, error = %e, "cannot identify nested mount; dev-based tag");
                    crate::identity::volume_tag(&format!("dev:{dev:016x}"))
                }
            }
        });
        meta.file_id = Some(crate::identity::namespaced_file_id(tag, fid));
    }
}

// in run(), per root, before walk:
        let native_root = ctx.svc.codec.raw_to_path(root);
        let mut scope = DevScope::for_root(ctx, &native_root)?;
// in WalkItem::File { path, mut meta }:
                    scope.apply(ctx, &native, &mut meta);
                    let entry = build_entry(ctx, v.volume_id, folder_id, &name, &meta, now, ...)?;
```
  Every other `FileMeta { .. }` literal (about 19 sites: `backend-windows/src/tree.rs`, `core/src/hash.rs`, `core/src/traits.rs` tests, `core/tests/{phase1,pool,progress,scale,verify}.rs`) gains `dev: None` (or the volume serial in `tree.rs`, see T4.2).
- **Tests to add:**
  - Naive unit test with a fake `Enumerator` that yields two roots whose files share `file_id` and size but differ in `dev` and content-equal: expect one group with `distinct_inodes == 2`. Same `dev` + same `file_id`: expect no group.
  - `pass_enumerate` test with a fake enumerator yielding a file whose `dev` differs from the root's: stored `file_id` is 16 bytes, starts with `volume_tag(uuid)`, and `size_candidates` returns its size when a same-size, same-inode file on the root device exists.
  - Rescan of that fixture: foreign row is reused (hash kept), proving the tag is stable.
  - Pure tests for `run_key` and `namespaced_file_id`.
- **Risk / follow-ups:** `identify` per foreign dev costs one `/dev/disk/by-uuid` scan per nested mount (cached). Long-term: give nested mounts their own `volume_id`. `lookup` needs no change: it identifies the queried file's own volume, whose ids are native.

---

### T4.2 One Windows identity function for walker and `metadata()` (closes GEN-07, GEN-11; Windows half of GEN-01)

- **Verification:**
  - GEN-07 Confirmed. `crates/backend-generic/src/reader.rs:63-66` `metadata()` calls `file_meta_from` only; `walk.rs:46-47` additionally calls `fill_file_identity`. On Windows `metadata()` returns `file_id None, nlink 1`. `lookup --insert` (`crates/core/src/lookup.rs:277`) writes `file_id NULL`, so the next rescan sees `None != Some` at `pass_enumerate.rs:222` and re-hashes. `HandleReader::metadata` (`backend-windows/src/handle.rs:259-261`) forwards to the same function.
  - GEN-11 Confirmed. `meta.rs:118-121` stores `created()` as ctime on Windows; `backend-windows/src/records.rs:142` stores `$SI` creation time. `meta_matches` (`pass_prefix.rs:251-257`) therefore cannot see an in-place rewrite with restored mtime. Side note confirmed: rescan reuse at `pass_enumerate.rs:219-222` compares size, mtime and file_id only, not ctime (CLAUDE.md overstates it).
  - CLAUDE.md gotcha (ReFS ids may collide) is real: `lib.rs:150` truncates to the 64-bit index.
- **Fix:** replace `win_sys::file_identity` with one function that, on the handle it already opens, reads `GetFileInformationByHandle` (nlink, 32-bit serial, 64-bit index), `GetFileInformationByHandleEx(FileIdInfo)` (128-bit id) and `GetFileInformationByHandleEx(FileBasicInfo)` (`ChangeTime`). Store 8 bytes when the 128-bit id is the zero-extended 64-bit index (NTFS, so walker and `$MFT` ids still match), else all 16 bytes (ReFS: no truncation, no collisions, and `file_id_to_frn` rejects 16 bytes so it is never opened by id). Call it from both the walker and `FsContentReader::metadata()` for regular files. Switch ctime to `ChangeTime` on Windows, and in the same change switch `records.rs:142` to `info.mft_record_modification_time()` (the `$STANDARD_INFORMATION` change time in the `ntfs` crate) and bump the `$MFT` cache format version (`mft_cache.rs` stores `ctime_ft`). Both sides must change together: `meta_matches` compares the enumerated ctime with a fresh `metadata()` ctime, so a mismatch in sources would mark every file "changed during read".
- **Rejected alternatives:** only document that ctime means creation time on Windows (keeps a real gap; acceptable fallback if the `$MFT` change cannot land in the same commit). Move identity into `file_meta_from` (it has no path or handle; the open has to happen somewhere).
- **Boilerplate:**
```rust
// crates/backend-generic/src/lib.rs  (win_sys)
use windows_sys::Win32::Storage::FileSystem::{
    GetFileInformationByHandleEx, FileBasicInfo, FileIdInfo, FILE_BASIC_INFO, FILE_ID_INFO,
};

/// What one attribute-only open reveals about a file.
pub struct FileIdentity {
    pub file_id: Vec<u8>,
    pub nlink: u32,
    /// `dwVolumeSerialNumber`, the same 32-bit serial the `$MFT` path must report as `dev`.
    pub volume_serial: u32,
    /// `FILE_BASIC_INFO.ChangeTime` (FILETIME ticks), `None` if the query failed.
    pub change_time: Option<i64>,
}

pub fn file_identity(path: &Path) -> Option<FileIdentity> {
    let wide = to_wide_nul(path.as_os_str());
    let handle = open_handle(&wide, 0, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                             FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).ok()?;
    // SAFETY: integers and FILETIMEs only; all-zero is valid.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: live handle; `info` is a correctly sized writable out-parameter.
    if unsafe { GetFileInformationByHandle(handle.raw(), &mut info) } == 0 {
        return None;
    }
    let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
    let id128 = query_by_handle::<FILE_ID_INFO>(&handle, FileIdInfo).map(|i| i.FileId.Identifier);
    let change_time = query_by_handle::<FILE_BASIC_INFO>(&handle, FileBasicInfo).map(|b| b.ChangeTime);
    Some(FileIdentity {
        file_id: file_id_bytes(id128.as_ref(), index),
        nlink: info.nNumberOfLinks,
        volume_serial: info.dwVolumeSerialNumber,
        change_time,
    })
}

/// `GetFileInformationByHandleEx` for a plain-old-data class. `None` on failure.
fn query_by_handle<T: Copy>(handle: &OwnedHandle, class: i32) -> Option<T> {
    // SAFETY: `T` is one of the FILE_*_INFO structs, integers only, all-zero valid.
    let mut out: T = unsafe { std::mem::zeroed() };
    let len = u32::try_from(std::mem::size_of::<T>()).ok()?;
    // SAFETY: live handle; `out` is writable for exactly `len` bytes.
    let ok = unsafe {
        GetFileInformationByHandleEx(handle.raw(), class, (&mut out as *mut T).cast(), len)
    };
    (ok != 0).then_some(out)
}

/// 8 bytes when the 128-bit id is the zero-extended 64-bit index (NTFS), so walker and
/// `$MFT` ids stay equal; otherwise all 16 bytes (ReFS), never truncated.
/// Pure: unit-tested on every OS (move it out of `win_sys` behind `cfg(any(windows, test))`).
pub fn file_id_bytes(id128: Option<&[u8; 16]>, index: u64) -> Vec<u8> {
    let low = index.to_le_bytes();
    match id128 {
        Some(id) if id[8..] == [0u8; 8] && id[..8] == low => low.to_vec(),
        Some(id) => id.to_vec(),
        None => low.to_vec(),
    }
}
```
```rust
// crates/backend-generic/src/meta.rs
#[cfg(windows)]
pub fn fill_file_identity(meta: &mut FileMeta, path: &std::path::Path) {
    if !matches!(meta.kind, EntryKind::RegularFile) { return; }
    if let Some(id) = crate::win_sys::file_identity(path) {
        meta.file_id = Some(id.file_id);
        meta.dev = Some(u64::from(id.volume_serial));
        meta.nlink = i64::from(id.nlink);
        if id.nlink > 1 { meta.attrs |= attrs::HARDLINK; }
        // T4.8 convention; `None` keeps whatever file_meta_from set.
        if let Some(ct) = id.change_time.and_then(|t| u64::try_from(t).ok()) {
            meta.ctime_ns = dedupe_core::time::filetime_to_unix_ns(ct);
        }
    }
}

// crates/backend-generic/src/reader.rs
    fn metadata(&self, path: &Path) -> Result<FileMeta> {
        let md = path.symlink_metadata()?;
        let mut meta = crate::meta::file_meta_from(&md, md.file_type());
        crate::meta::fill_file_identity(&mut meta, path); // no-op on Unix
        Ok(meta)
    }
```
```rust
// crates/backend-windows/src/records.rs:142
        ctime_ft: info.mft_record_modification_time().nt_timestamp(), // $SI ChangeTime
// crates/backend-windows/src/tree.rs FileMeta literal: dev: Some(u64::from(volume_serial32))
```
  For `dev` on the `$MFT` path, read the 32-bit serial with `GetVolumeInformationByHandleW` on the volume handle rather than truncating the 64-bit `FSCTL_GET_NTFS_VOLUME_DATA` serial: it is the exact source the walker uses. (That the low 32 bits of the NTFS 64-bit serial equal `dwVolumeSerialNumber` is commonly stated but not confirmed here.)
- **Tests to add:** pure `file_id_bytes` cases (NTFS-shaped, ReFS-shaped, missing). Extend `mft_vs_walk` (Windows, ignored) to assert equal `file_id`, `dev` and `ctime_ns` between walker and `$MFT`. Windows test: `metadata()` on a hardlinked file returns `Some(file_id)` equal to the walker's and `nlink == 2`.
- **Risk / follow-ups:** `metadata()` on Windows now costs one extra attribute-only open per call (once per hashed file in `unchanged`). Not counted in `opens()`. Stored ctime values change meaning on Windows: harmless for reuse (ctime is not compared at `pass_enumerate.rs:219-222`) but resumes across the upgrade could see one "changed during read" per in-flight file; recommend noting "rescan after upgrade". Verify on hardware that `FILE_ID_INFO` on NTFS is the zero-extended FRN; if not, the code still works (16 bytes, path open) but walker and `$MFT` ids differ, which `mft_vs_walk` will show.

---

### T4.3 Count distinct inodes in prefix classification and collapse hardlinks in `pass_full` (closes PIPE-11)

- **Verification:** Confirmed. `crates/core/src/pipeline/pass_prefix.rs:77-92` copies the representative's outcome to each sibling and pushes the sibling into `large`; `pass_prefix.rs:94-96` inserts every `large` row into `index`; `pass_prefix.rs:125-131` uses `index.count(h) > 1` (row count, `index/memory.rs:46-48`). Two links of one inode are therefore "partnered" with each other. DB fallback `queries.rs:368-379` uses `COUNT(*)=1`, same flaw. `pass_full.rs:185-217` (`plan_rows`) and `pass_full.rs:118-157` (sequential) hash every row; no grouping by `file_id`. `recompute_groups` later drops the group (distinct = 1), so the result is correct but the bytes were read N times.
- **Fix:** identity key per row = `COALESCE(file_id, id)` (unique per volume after T4.1). (a) `pass_prefix`: insert into `index` only once per `(hash, identity)` so `count` counts inodes; a sibling inherits its representative's partnered/unique classification. Preloaded rows (`rows_with_prefix`) also carry `file_id`. (b) DB fallback: singleton by distinct identity, returning all rows of such groups. (c) `pass_full`: move `hardlink_reps` to `pipeline/mod.rs` and reuse it; siblings get a `Slot::Sibling(rep_idx)` and copy the rep's judged result.
- **Rejected alternatives:** count distinct identities on the fly per hash with a side `HashMap<Hash, HashSet<Vec<u8>>>` (doubles memory against `mem_budget`); collapse hardlinks at enumeration into one row (breaks per-path reporting and the state machine).
- **Boilerplate:**
```rust
// crates/core/src/pipeline/mod.rs  (moved from pass_prefix.rs:222-235, unchanged body)
/// sibling row index -> representative row index, for rows sharing a `file_id`.
pub(crate) fn hardlink_reps(rows: &[FileEntry]) -> HashMap<usize, usize> { /* as today */ }
```
```rust
// crates/core/src/pipeline/pass_prefix.rs
    // Identity per row id: siblings map to their representative's id, so the index
    // holds one entry per (prefix, inode).
    let mut index = MemoryIndex::new(cfg.mem_budget);
    let mut indexed: HashSet<(Hash, IdentKey)> = HashSet::new();
    for (id, h, fid) in queries::rows_with_prefix(ctx.read, volume_id, ctx.run_id)? {
        if indexed.insert((h, ident_key(fid.as_deref(), id))) {
            index.insert(h, id)?;
        }
    }
    // ...after hashing:
    for (row_idx, (id, h)) in large_with_idx.iter() {
        let r = &rows[*row_idx];
        if indexed.insert((*h, ident_key(r.file_id.as_deref(), r.id))) {
            index.insert(*h, *id)?;
        }
    }
    // classification: index.count(h) is now distinct inodes; siblings follow `large` as before.

/// `file_id` bytes, or the row id for rows without one (mirrors `COALESCE(file_id, id)`).
#[derive(Clone, PartialEq, Eq, Hash)]
enum IdentKey { File(Vec<u8>), Row(i64) }
fn ident_key(fid: Option<&[u8]>, id: i64) -> IdentKey {
    fid.map_or(IdentKey::Row(id), |f| IdentKey::File(f.to_vec()))
}
```
  (`large` needs the row index alongside `(id, hash)`; push `(i, r.id, h)` in the two loops at `pass_prefix.rs:61-92`.)
```sql
-- crates/core/src/db/queries.rs  rows_with_prefix: also return file_id
SELECT id, prefix_hash, file_id FROM files
 WHERE volume_id=?1 AND last_seen_run_id=?2 AND state IN (2,-4) AND prefix_hash IS NOT NULL;

-- singleton_prefixes: every row whose prefix is held by exactly one inode
SELECT id FROM files
 WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2
   AND prefix_hash IN (
     SELECT prefix_hash FROM files
      WHERE volume_id=?1 AND last_seen_run_id=?2 AND state=2 AND prefix_hash IS NOT NULL
      GROUP BY prefix_hash
     HAVING COUNT(DISTINCT COALESCE(file_id, id)) = 1);
```
```rust
// crates/core/src/pipeline/pass_full.rs
enum Slot {
    Reused(Hash),
    Job,
    Failed(ErrRow),
    /// Hardlink of an earlier row: takes that row's result, never read.
    Sibling(usize),
}

fn plan_rows(/* ... */) -> Result<(Vec<Slot>, Vec<Job>)> {
    let rep_of = super::hardlink_reps(rows);
    for (idx, r) in rows.iter().enumerate() {
        let slot = if let Some(&rep) = rep_of.get(&idx) {
            Slot::Sibling(rep)
        } else if let Some(h) = reusable_hash(ctx, r) { Slot::Reused(h) } else { /* as today */ };
        if !matches!(slot, Slot::Job) { emit_advance(/* ... */); }
        slots.push(slot);
    }
}

// finish_rows: keep `outcome_of: HashMap<usize, Result<Hash, ErrRow>>` for every judged row;
// Slot::Sibling(rep) => match outcome_of.get(&rep) {
//     Some(Ok(h)) => hashed.push((r.id, *h)),
//     Some(Err(e)) => errors.push(ErrRow { file_row_id: Some(r.id), path: Some(path), ..e.clone() }),
//     None => {} // rep was cancelled; row stays in state 2 for resume
// }
// hash_sequential: same rule with `rep_of` and an outcome map.
```
  Reps always precede siblings (`hardlink_reps` assigns the first index as rep), so the outcome exists when the sibling is judged.
- **Tests to add:** `pass_prefix` with two links of one inode plus an unrelated same-size file with a different prefix: both links end in -4 and `opens()` shows one read. Same with a third, content-equal inode: links and the third file reach state 3 and `pass_full` opens the hardlinked inode once (counting reader). DB fallback (`mem_budget` tiny): same states. `singleton_prefixes` unit test with hardlink-only group.
- **Risk / follow-ups:** sibling inherits "changed during read" from its rep, which is correct (same inode).

---

### T4.4 macOS volume UUID via getattrlist on the real volume root (closes GEN-02, GEN-14 partly)

- **Verification:**
  - GEN-02 Confirmed (line numbers moved): `crates/backend-generic/src/volume.rs:195-221`. `volattr: libc::ATTR_VOL_UUID` without `ATTR_VOL_INFO`, and the UUID read at `buf[20..36]`. Apple's getattrlist(2) (checked: keith.github.io/xcode-man-pages/getattrlist.2.html) says: "you must set ATTR_VOL_INFO in the volattr field if you request any other volume attributes ... even though it generates no result in the attribute buffer"; the buffer starts with a `u_int32_t` total length (including itself); attributes follow in documented order, each 4-byte aligned; `attribute_set_t` is present only for `ATTR_CMN_RETURNED_ATTRS`; `ATTR_VOL_UUID` is a `uuid_t`; and EINVAL is returned when volume attributes are requested "but path does not reference the root of the volume". So the UUID is at `buf[4..20]`. `buf.len() < 36` (line 215) is dead. Also `format_uuid` (`volume.rs:227-228`) uses `.unwrap()` in non-test code. `libc 0.2.155` has `ATTR_VOL_INFO = 0x80000000`.
  - GEN-14 Partially confirmed: `mount_root` (`volume.rs:28-38`) climbs by `st_dev`, so for `/Users/me` the "mount" is `/Users` (firmlink, different dev from `/`) and getattrlist on it fails EINVAL even after the bitmask fix. The dual-spelling consequence for folder chains is real but see the warning below.
- **Fix:** call `statfs(path)` (not `statfs(mount)`), take `f_mntonname` as the volume root for getattrlist, request `ATTR_VOL_INFO | ATTR_VOL_UUID` with `FSOPT_NOFOLLOW`, validate `length >= 20`, read `buf[4..20]`. Keep `VolumeInfo.mount` as the dev-climbed path in the root's own spelling for now. Do NOT replace `mount` with `f_mntonname` alone: `pass_enumerate.rs:62-75` keys `dir_ids` by the walker's raw paths, and a root `/Users/me` under mount `/System/Volumes/Data` would fail `relative_components`'s `starts_with`, so the walker's directories never find a parent and every file is dropped. Full GEN-14 needs root rewriting into the mount's namespace (follow-up).
- **Rejected alternatives:** `DADiskCopyDescription` (DiskArbitration, needs CoreFoundation bindings, new deps); `getattrlist` on the mount from `mount_root` (EINVAL on firmlinks).
- **Boilerplate:**
```rust
// crates/backend-generic/src/volume.rs  (macOS imp)
    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let mount = super::mount_root(path);
        let c_path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| dedupe_core::CoreError::Encoding("nul byte in path".into()))?;
        // SAFETY: `libc::statfs` is plain old data; all-zero is valid.
        let mut st: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c_path` is NUL-terminated and outlives the call; `st` is writable.
        if unsafe { libc::statfs(c_path.as_ptr(), &mut st) } != 0 {
            return Err(dedupe_core::CoreError::Io(std::io::Error::last_os_error()));
        }
        let fs_type = dedupe_core::model::FsType::parse(&cstr_array(&st.f_fstypename));
        // Volume attributes need the volume root itself (getattrlist(2), EINVAL otherwise).
        let vol_root = CString::new(cstr_bytes(&st.f_mntonname))
            .map_err(|_| dedupe_core::CoreError::Encoding("nul byte in mount".into()))?;
        let (uuid, uuid_kind) = match get_volume_uuid(&vol_root) {
            Some(u) => (format_uuid(&u), dedupe_core::model::UuidKind::FsUuid),
            None => { /* fsid fallback as today, with a SAFETY comment on the transmute */ }
        };
        // `mount` stays the dev-climbed path in the root's spelling (see GEN-14 note).
        // ...
    }

    /// `ATTR_VOL_UUID` of the volume mounted at `vol_root`. `None` on failure or all-zero.
    fn get_volume_uuid(vol_root: &CString) -> Option<[u8; 16]> {
        let mut attr = libc::attrlist {
            bitmapcount: libc::ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: 0,
            volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        };
        // Reply: u32 total length (includes itself), then uuid_t. No attribute_set_t,
        // because ATTR_CMN_RETURNED_ATTRS is not requested.
        let mut buf = [0u8; 4 + 16];
        // SAFETY: `vol_root` is NUL-terminated; `attr` and `buf` are live and writable
        // for the sizes passed.
        let rc = unsafe {
            libc::getattrlist(
                vol_root.as_ptr(),
                (&mut attr as *mut libc::attrlist).cast(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::FSOPT_NOFOLLOW,
            )
        };
        if rc != 0 {
            tracing::debug!(err = %std::io::Error::last_os_error(), "getattrlist ATTR_VOL_UUID failed");
            return None;
        }
        let len = u32::from_ne_bytes(buf[..4].try_into().ok()?) as usize;
        if len < buf.len() { return None; }
        let uuid: [u8; 16] = buf[4..20].try_into().ok()?;
        (uuid != [0u8; 16]).then_some(uuid)
    }

    /// Lower-case 8-4-4-4-12, no panics.
    fn format_uuid(b: &[u8; 16]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut s = String::with_capacity(36);
        for (i, byte) in b.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) { s.push('-'); }
            s.push(char::from(HEX[usize::from(byte >> 4)]));
            s.push(char::from(HEX[usize::from(byte & 0xf)]));
        }
        s
    }
```
- **Tests to add:** pure `format_uuid` test; macOS-only test (`#[cfg(target_os = "macos")]`): `identify("/")` and `identify(home dir)` return `UuidKind::FsUuid`, and identify of `/Users/<me>` equals identify of `/System/Volumes/Data/Users/<me>` in `uuid`.
- **Risk / follow-ups:** Existing macOS DBs have `fsid:` uuids, so after the fix each volume is a new row and is re-indexed once (document; v2 policy unchanged, no schema bump). GEN-14 full fix (root rewriting through firmlinks) remains open. Not run on a Mac here.

---

### T4.5 Linux: treat an all-zero fsid as unknown, derive identity from mountinfo (closes GEN-04)

- **Verification:** Confirmed. `crates/backend-generic/src/volume.rs:117-129` formats `fsid:{val0}{val1}` unconditionally; there is no zero check and the Linux `imp` never returns `UuidKind::None`, contrary to the module doc (`volume.rs:3-4`). FUSE's kernel statfs conversion leaves `f_fsid` zero (from review; not re-verified on a FUSE mount here, confirm with `stat -f -c %i`).
- **Fix:** if both fsid words are zero, look up the mount in `/proc/self/mountinfo` by `major:minor` of `st_dev` and use `none:{fstype}:{source}` (e.g. `none:fuse.sshfs:user@host:/srv`), which is stable across remounts and distinct per remote. If mountinfo has no match, use `none:{fs_type}:{mount path}`. Kind `UuidKind::None` (weak resume guard, already warned by design). Also extend `fs_type_from_magic` (CORE-19).
- **Rejected alternatives:** `none:` from `st_dev` (changes on every mount); mount path only (two different remotes mounted at the same place at different times would merge).
- **Boilerplate:**
```rust
// crates/backend-generic/src/volume.rs  (Linux imp)
        let (uuid, uuid_kind) = match fs_uuid {
            Some(u) => (u, UuidKind::FsUuid),
            None if st.f_fsid.val != [0, 0] => { /* fsid:... as today, FsidFallback */ }
            None => {
                let (fstype, source) = mountinfo_entry(dev)
                    .unwrap_or_else(|| (fs_type.as_str().to_owned(), mount.display().to_string()));
                tracing::warn!(mount = %mount.display(), %fstype, "no fs UUID and zero fsid; weak identity");
                (format!("none:{fstype}:{source}"), UuidKind::None)
            }
        };

    /// `(fstype, source)` of the mount whose `major:minor` equals `dev`.
    /// mountinfo: `id parent maj:min root mountpoint opts [optional...] - fstype source superopts`.
    fn mountinfo_entry(dev: u64) -> Option<(String, String)> {
        let want = format!("{}:{}", rustix::fs::major(dev), rustix::fs::minor(dev));
        let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
        text.lines().find_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            if pre.split(' ').nth(2)? != want { return None; }
            let mut f = post.split(' ');
            let fstype = f.next()?.to_owned();
            let source = unescape_octal(f.next()?); // "\040" -> ' '
            Some((fstype, source))
        })
    }
```
  (`pre.split(' ')` is safe for field 3 because fields 1-3 never contain spaces; the mount point is escaped.)
- **Tests to add:** pure parser test on a fixture mountinfo string with an sshfs line, an escaped mount point, and optional fields. Linux test (ignored, manual) on an sshfs mount.
- **Risk / follow-ups:** `rustix::fs::major/minor` exist in rustix 0.38 (`fs` feature) to my knowledge; confirm, else use `libc::major/minor`.

---

### T4.6 Windows identify: network, mapped and subst roots, prefix normalisation, buffer size (closes GEN-03, GEN-09, GEN-18; GEN-19 mount part)

- **Verification:**
  - GEN-03 Confirmed by code reading: `volume.rs:276-286` returns `Err` when `GetVolumeNameForVolumeMountPointW` fails, which it does for UNC roots and mapped drives (documented to take a local mount point). The `VolumeSerial` branch (`volume.rs:294-298`) needs success with an empty name, which does not occur. Unverified on Windows hardware.
  - GEN-09 Partially confirmed: `scan.rs:46-47` passes `canonicalize(root)` (verbatim `\\?\C:\...` on Windows), and `relative_components` (`pass_enumerate.rs:302-311`) falls back to splitting the whole root when the mount lacks the prefix, producing `?`, `C:` components. Whether `GetVolumePathNameW` keeps `\\?\` is not documented; unverified. `lookup.rs:259` calls `identify(path)` on the un-canonicalized path, while `lookup.rs:264` canonicalizes, so the two can disagree even if identify is consistent.
  - GEN-18 Confirmed: fixed 261-unit buffers at `volume.rs:268, 276`, `MAX_PATH` at `lib.rs:158`. `GetVolumePathNameW` docs advise a buffer at least as long as the input path.
  - GEN-19 (mount part) Confirmed: `volume.rs:303-309` converts with `String::from_utf16_lossy` before the codec.
- **Fix:** (1) buffers sized `max(input_len, MAX_PATH + 1)`. (2) GUID first; on failure, `GetVolumeInformationW(vol_path)` serial and fs name -> `serial:{serial:08x}:{root_lower}` with `UuidKind::VolumeSerial` (root included because several shares of one NAS volume report the same serial and would merge, the GEN-04 failure mode); if that fails too, `none:{fs}:{root_lower}` with `UuidKind::None`. (3) Return the mount in the same verbatim form as the input (pure helper, testable on every OS). (4) Build the mount via `OsString::from_wide` + `to_raw` (lossless). (5) `lookup.rs`: canonicalize before `identify`.
- **Rejected alternatives:** `WNetGetUniversalNameW` to map drive letters to UNC (adds the `mpr` link and a feature; `canonicalize` already returns `\\?\UNC\...` for mapped drives, to be verified); serial alone (merges shares).
- **Boilerplate:**
```rust
// crates/backend-generic/src/volume.rs  (Windows imp)
    pub fn identify(path: &Path) -> Result<VolumeInfo> {
        let wide = crate::win_sys::to_wide_nul(path.as_os_str());
        let cap = wide.len().max(MAX_PATH as usize + 1);
        let mut vol_path = vec![0u16; cap];
        let len = u32::try_from(cap).map_err(|e| CoreError::InvalidArg(e.to_string()))?;
        // SAFETY: `wide` is NUL-terminated; `vol_path` is writable for `len` units.
        if unsafe { GetVolumePathNameW(wide.as_ptr(), vol_path.as_mut_ptr(), len) } == 0 {
            return Err(CoreError::Io(std::io::Error::last_os_error()));
        }
        let root = until_nul(&vol_path); // &[u16], no NUL
        let (serial, fs_type) = volume_info(&vol_path);
        let root_key = String::from_utf16_lossy(root).to_lowercase(); // id text only, not a path
        let (uuid, uuid_kind) = match volume_guid(&vol_path) {
            Some(guid) => (guid, UuidKind::FsUuid),
            None if serial != 0 => (format!("serial:{serial:08x}:{root_key}"), UuidKind::VolumeSerial),
            None => {
                tracing::warn!(root = %root_key, "no volume GUID or serial; weak identity");
                (format!("none:{}:{root_key}", fs_type.as_str()), UuidKind::None)
            }
        };
        let mount_wide = super::match_verbatim(path.as_os_str().encode_wide().collect::<Vec<_>>().as_slice(), root);
        let mount = crate::codec::GenericPathCodec.to_raw(&std::ffi::OsString::from_wide(&mount_wide));
        Ok(VolumeInfo { uuid, uuid_kind, mount, fs_type, label: None })
    }

    /// `\\?\Volume{GUID}\` for a local mount point, `None` for UNC, mapped or subst roots.
    fn volume_guid(vol_path: &[u16]) -> Option<String> { /* today's call, Ok -> Some, 0 -> None */ }
```
```rust
// crates/backend-generic/src/volume.rs  (pure, cfg(any(windows, test)))
/// Spell `root` (from GetVolumePathNameW) the way `input` is spelled: verbatim (`\\?\`,
/// `\\?\UNC\`) or not, so `relative_components` can strip it as a byte prefix.
fn match_verbatim(input: &[u16], root: &[u16]) -> Vec<u16> {
    let w = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let (vb, unc_vb, unc) = (w(r"\\?\"), w(r"\\?\UNC\"), w(r"\\"));
    let in_vb = input.starts_with(&vb);
    let root_vb = root.starts_with(&vb);
    match (in_vb, root_vb) {
        (true, false) if root.starts_with(&unc) => [&unc_vb[..], &root[2..]].concat(),
        (true, false) => [&vb[..], root].concat(),
        (false, true) if root.starts_with(&unc_vb) => [&unc[..], &root[unc_vb.len()..]].concat(),
        (false, true) if !root[vb.len()..].starts_with(&w("Volume{")) => root[vb.len()..].to_vec(),
        _ => root.to_vec(),
    }
}
```
```rust
// crates/core/src/lookup.rs:259
    let native = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let info = vi.identify(&native)?;
```
  `lib.rs::volume_root` (`lib.rs:156-167`) gets the same buffer sizing.
- **Tests to add:** pure `match_verbatim` table test (drive, UNC, GUID root, already-matching). Windows manual checks: `identify` on `Z:\` (mapped SMB), `\\server\share\`, a `subst` drive, and a folder mount deeper than 260 chars; print `canonicalize(root)` vs `info.mount` and assert `relative_components` yields only the expected components.
- **Risk / follow-ups:** The exact return of `GetVolumePathNameW` for `\\?\UNC\...` and for subst drives is unverified; the helper covers both spellings. A volume mounted only at a folder (no letter) may canonicalize to a `\\?\Volume{...}` form, also unverified.

---

### T4.7 Restrict open-by-id to NTFS, verify the opened file, and derive the overlapped handle with ReOpenFile (closes WIO-01, WIO-02)

- **Verification:**
  - WIO-01 Confirmed (mechanism; exploitability Low confidence). `crates/backend-windows/src/handle.rs:206-221` (`hint_for`) opens any volume root, with no file-system check; `handle.rs:224-245` passes any 8-byte id to `OpenFileById` and never checks the result. Today walker ids on ReFS are truncated 64-bit indexes (`backend-generic/src/lib.rs:150`). After T4.2 ReFS ids become 16 bytes and are rejected by `file_id_to_frn`, which already removes the main trigger, but FAT/exFAT or third-party file systems that answer `FileIdType` remain unguarded.
  - WIO-02 Confirmed. `handle.rs:281-301`: sync open via `open_handle_for`, length from it, then `open_overlapped` (`handle.rs:248-251`) repeats id-then-path resolution; `self.opens` incremented once.
- **Fix:** cache `is_ntfs` with each hint (via `GetVolumeInformationByHandleW` on the hint handle) and skip open-by-id elsewhere. After `OpenFileById` succeeds, read `GetFileInformationByHandle` and require `index == frn as u64`, else close and fall back to the path. For overlapped, call `ReOpenFile(handle, GENERIC_READ, SHARE_ALL, pattern | FILE_FLAG_OVERLAPPED)` on the first handle: same file object target, no second resolution. Count the reopen in `opens`. `ReOpenFile` and `GetVolumeInformationByHandleW` exist in windows-sys 0.59 `Win32::Storage::FileSystem` (checked in the registry source).
- **Rejected alternatives:** pass the expected size in `ReadTarget` and compare after open (stronger, but touches every `ReadTarget` construction; reasonable follow-up); drop open-by-id entirely (loses the move-tolerant read the design wants).
- **Boilerplate:**
```rust
// crates/backend-windows/src/handle.rs
    use windows_sys::Win32::Storage::FileSystem::{GetVolumeInformationByHandleW, ReOpenFile,
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};

    /// Volume-root handle used only as the hint for `OpenFileById`. Only NTFS hints are cached
    /// as usable: other file systems either lack stable 64-bit ids or were never validated.
    struct VolumeHint(OwnedHandle);

    fn hint_for(&self, path: &Path) -> Option<Arc<VolumeHint>> {
        // ... as today, but:
        let opened = open_handle(&key, GENERIC_READ, SHARE_ALL, FILE_FLAG_BACKUP_SEMANTICS)
            .ok()
            .filter(is_ntfs)
            .map(|h| Arc::new(VolumeHint(h)));
        hints.insert(key, opened.clone());
        opened
    }

    fn is_ntfs(h: &OwnedHandle) -> bool {
        let mut name = [0u16; 32];
        // SAFETY: live handle; only the fs-name buffer is requested, sized as passed.
        let ok = unsafe {
            GetVolumeInformationByHandleW(h.raw(), null_mut(), 0, null_mut(), null_mut(),
                                          null_mut(), name.as_mut_ptr(), name.len() as u32)
        };
        ok != 0 && until_nul(&name) == "NTFS".encode_utf16().collect::<Vec<_>>().as_slice()
    }

    /// The handle really is file `frn` (guards file systems that ignore the sequence number).
    fn handle_is(handle: &OwnedHandle, frn: i64) -> bool {
        // SAFETY: integers only; all-zero valid.
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: live handle; `info` is a writable out-parameter of the right size.
        if unsafe { GetFileInformationByHandle(handle.raw(), &mut info) } == 0 { return false; }
        let index = (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow);
        u64::from_ne_bytes(frn.to_ne_bytes()) == index
    }

    // open_handle_for:
                    match open_by_id(&hint, frn, flags) {
                        Ok(handle) if handle_is(&handle, frn) => return Ok(handle),
                        Ok(_) => tracing::debug!("open by id returned another file; opening by path"),
                        Err(err) => tracing::debug!(error = %err, "open by file id failed; opening by path"),
                    }

    /// Same file, new handle with the overlapped flag. No second id or path resolution.
    fn reopen_overlapped(handle: &OwnedHandle, access: AccessPattern) -> io::Result<OwnedHandle> {
        // SAFETY: `handle` is live; ReOpenFile returns a new handle we own or INVALID_HANDLE_VALUE.
        let raw = unsafe {
            ReOpenFile(handle.raw(), GENERIC_READ, SHARE_ALL, pattern_flags(access) | FILE_FLAG_OVERLAPPED)
        };
        if raw == INVALID_HANDLE_VALUE { return Err(io::Error::last_os_error()); }
        // SAFETY: fresh handle from a successful ReOpenFile; the guard is its only closer.
        Ok(unsafe { OwnedHandle::from_raw(raw) })
    }

    // open_target: replace `self.open_overlapped(t, length)` with
                match reopen_overlapped(&handle, t.access)
                    .and_then(|h| OverlappedFile::new(h, self.read_ahead, self.chunk, length))
                {
                    Ok(file) => { self.opens.fetch_add(1, Ordering::Relaxed); return Ok(Box::new(file)); }
                    Err(err) => tracing::debug!(error = %err, "overlapped reopen failed; using a sync handle"),
                }
    // `open_overlapped` is deleted.
```
- **Tests to add:** Windows (ignored, manual): open-by-id of a stale FRN falls back to the path; on a FAT32/exFAT USB stick no `OpenFileById` is attempted (count via a debug counter or tracing capture); a large sequential file yields the same hash as the sync path and `opens()` grows by 2. Pure: none beyond existing `plan_open`.
- **Risk / follow-ups:** `ReOpenFile` with `FILE_FLAG_OVERLAPPED` is the documented use, but behaviour with `FILE_FLAG_SEQUENTIAL_SCAN` in the same mask is not re-checked; if refused, fall back to sync as the code already does. Any Windows test asserting an exact `opens()` count for large files must be updated.

---

### T4.8 One time-conversion convention in core (closes GEN-12, WIO-09)

- **Verification:**
  - GEN-12 Confirmed. `crates/backend-generic/src/meta.rs:115`: `saturating_mul(..) + ctime_nsec()` can overflow after saturation (panic in debug). `meta.rs:103-106`: `as_nanos() as i64` wraps past 2262; pre-1970 mtimes become 0 while Unix ctime stays negative.
  - WIO-09 Confirmed. `crates/backend-windows/src/filetime.rs:17-28` returns `None` -> mtime 0 on overflow, the walker wraps instead; the module doc's "matches" claim is false for far-future times.
- **Fix:** a pure `core::time` module with checked arithmetic, used by both backends. Convention: signed nanoseconds since the Unix epoch on every platform (pre-1970 negative); a value outside `i64` is `None`; mtime `None` falls back to 0, ctime stays `None`. Changing pre-1970 mtime from 0 to negative re-hashes such files once after upgrade; acceptable.
- **Boilerplate:**
```rust
// crates/core/src/time.rs  (new, `pub mod time;`)
use std::time::{SystemTime, UNIX_EPOCH};

const NS_PER_SEC: i64 = 1_000_000_000;
const FILETIME_UNIX_EPOCH_TICKS: i128 = 116_444_736_000_000_000;

/// Signed Unix nanoseconds; `None` outside the i64 range.
pub fn system_time_to_unix_ns(t: SystemTime) -> Option<i64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).ok(),
        Err(e) => i64::try_from(e.duration().as_nanos()).ok()?.checked_neg(),
    }
}

/// `secs` + `nsec` (as from `st_ctime`/`st_ctime_nsec`), checked.
pub fn unix_parts_to_ns(secs: i64, nsec: i64) -> Option<i64> {
    secs.checked_mul(NS_PER_SEC)?.checked_add(nsec)
}

/// FILETIME (100 ns ticks since 1601) to signed Unix nanoseconds, checked.
pub fn filetime_to_unix_ns(ft: u64) -> Option<i64> {
    let ticks = i128::from(ft) - FILETIME_UNIX_EPOCH_TICKS;
    i64::try_from(ticks.checked_mul(100)?).ok()
}
```
```rust
// crates/backend-generic/src/meta.rs
fn modified_ns(md: &Metadata) -> i64 {
    md.modified().ok().and_then(dedupe_core::time::system_time_to_unix_ns).unwrap_or(0)
}
#[cfg(unix)]
fn ctime_ns(md: &Metadata) -> Option<i64> {
    use std::os::unix::fs::MetadataExt;
    dedupe_core::time::unix_parts_to_ns(md.ctime(), md.ctime_nsec())
}
// Windows ctime: T4.2 (ChangeTime via filetime_to_unix_ns); fallback `created()` via system_time_to_unix_ns.

// crates/backend-windows/src/filetime.rs
pub use dedupe_core::time::filetime_to_unix_ns;
pub fn mtime_ns(ft: u64) -> i64 { filetime_to_unix_ns(ft).unwrap_or(0) }
pub fn ctime_ns(ft: u64) -> Option<i64> { filetime_to_unix_ns(ft) }
```
- **Tests to add:** shared vectors in `core/src/time.rs`: epoch, 1 ns before epoch, `FILETIME` of 1601 (None), year 2262 boundary on both sides, `i64::MAX` secs (None, no panic); and an equality test that `filetime_to_unix_ns(ft)` equals `system_time_to_unix_ns(UNIX_EPOCH +/- d)` for the same instants. Update `filetime.rs` tests that expect pre-1970 `None`.
- **Risk / follow-ups:** `$MFT` cache stores raw FILETIMEs, so no cache bump is needed for this item.

---

### T4.9 Read-path robustness: no-follow non-blocking open, EINTR retry (closes GEN-13, GEN-16)

- **Verification:** GEN-13 Confirmed: `crates/backend-generic/src/reader.rs:52, 58` use `File::open` (follows symlinks, blocks on a FIFO); `extents.rs:56` likewise. GEN-16 Confirmed: `reader.rs:30-47` returns `Interrupted` as an error and `core/src/hash.rs:159-169` stops on it (only `backend-windows/src/sector.rs:81` retries).
- **Fix:** one `open_regular(path)` helper: on Unix `custom_flags(O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)`, then `file.metadata()?.is_file()` or `ErrorKind::InvalidInput`; Windows keeps `File::open` (the walker never yields reparse points as files). Loop on `Interrupted` in `FsHandle::read_at`.
```rust
// crates/backend-generic/src/reader.rs
fn open_regular(path: &Path) -> std::io::Result<File> {
    let mut o = std::fs::OpenOptions::new();
    o.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // O_NONBLOCK: a FIFO swapped in after enumeration must not hang a worker.
        o.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let f = o.open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a regular file"));
    }
    Ok(f)
}

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize> {
        loop {
            match self.read_once(offset, buf) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                r => return Ok(r?),
            }
        }
    }
```
  (`libc` is already a dependency of backend-generic on Unix; `O_NONBLOCK` has no effect on regular-file reads.)
- **Tests to add:** Unix: `open` on a symlink to a regular file fails; `open` on a FIFO returns `InvalidInput` without hanging (run under a timeout). EINTR: a `FileHandle` double is not possible at this layer; covered by review.

---

### T4.10 SAFETY comments and lint for unsafe blocks in volume.rs (closes GEN-17)

- **Verification:** Confirmed: no `// SAFETY:` at `volume.rs:165, 166, 178, 206, 269, 277, 327`; workspace lints (`Cargo.toml:20-22`) do not enable `undocumented_unsafe_blocks`.
- **Fix:** add SAFETY comments (T4.4 and T4.6 boilerplate already include them) and `undocumented_unsafe_blocks = "deny"` under `[workspace.lints.clippy]`. Keep the `transmute::<libc::fsid_t, [i32; 2]>` (libc keeps `val` private on Apple) with a SAFETY note: both are 8 bytes, `repr(C)`, integers only. Run the Windows cross-check clippy after enabling the lint.

---

### T4.11 Strict WTF-8 decode (closes GEN-19, codec part)

- **Verification:** Confirmed: `crates/backend-generic/src/codec.rs:105-146` `break`s on truncation, skips bad leads, ignores continuation validity, and `saturating_sub(0x10000)` at line 137 maps overlong forms to arbitrary units.
- **Fix:** make `wtf8::decode` return `Result<Vec<u16>, usize>` (offset of the first bad byte), validate continuation bytes (`0b10xx_xxxx`) and ranges (reject overlong and > U+10FFFF). `raw_to_os_string` keeps the infallible `PathCodec` signature: on `Err` it `tracing::warn!`s with the offset and decodes with U+FFFD for bad sequences, so corrupt DB bytes are visible instead of silently becoming another valid path. Mount conversion fixed in T4.6.
- **Tests to add:** decode rejects truncated 3-byte, bad continuation, overlong 4-byte, > U+10FFFF; existing round-trip tests still pass.

---

### T4.12 `FsType::parse` labels (closes CORE-19)

- **Verification:** Confirmed: `crates/core/src/model.rs:243-254` maps `"hfs"` to `Apfs`; `"msdos"` (macOS FAT), `"ntfs3"`, `"fuseblk"` fall to `Other`. Linux `fs_type_from_magic` (`volume.rs:140-151`) knows only ext/xfs/btrfs. Only a label today.
- **Fix:** add `FsType::Hfs` (`as_str` "hfs"; `volumes.fs_type` is text, no schema change), map `"hfs" | "hfs+"` to it, `"msdos"` to `Fat32`, `"ntfs3"` to `Ntfs`; leave `"fuseblk"` as `Other` (cannot tell ntfs-3g from exfat-fuse). Linux magic: `0x5346_544E` (NTFS_SB_MAGIC, "NTFS"), `0x7366_746E` (ntfs3), `0x4d44` (MSDOS), `0x2011_BAB0` (exFAT). Magic values from memory of `linux/magic.h` and the ntfs3 driver; confirm before merging.
- **Tests to add:** table test for `FsType::parse` and `fs_type_from_magic`.

---

## T5. The `lookup` command and `lookup --insert`

**Findings covered:** CORE-02, CORE-03, CORE-04, CORE-05, CORE-06, CORE-07, DB-17

**Root causes:**
- Lookup never works out which DB row (if any) *is* the query file. With no `(volume_id, folder_id, name)` key for the query, it cannot leave out itself or its hardlinks (CORE-03). It also cannot tell "insert new" from "update existing" (CORE-02), and it resolves the folder late, only inside `persist`, using a byte-prefix mount strip that fails on verbatim or differently-cased Windows paths (CORE-06 part 2). One function, `resolve_query_key`, called early, fixes all three.
- `persist` is a second, ad hoc write path. It writes outside the writer, with no transaction, no inter-process exclusion and `MAX(id)+1` ids. It reuses the destructive `upsert_file` and writes `state=5` with no guard. It runs only on the "matches" branch. These cover CORE-02, CORE-05 and CORE-06 part 1.
- Any `hash_*` error is treated as "volume unmounted", and that result counts as a match (CORE-04).
- The loop is ordered as "build path, then filter", and the code fully hashes the query even when no candidate can confirm it (CORE-07).
- `SqliteIndex` keeps a trait impl and helpers that production never calls (DB-17). This one is unrelated to the others and is a cleanup.

**Recommended order of work:**
1. T5.1 query key plus self/hardlink exclusion (CORE-03, CORE-06 path part). Every later fix uses the key.
2. T5.2 candidate loop rework (CORE-04, CORE-07).
3. T5.3 `--insert` rewrite: lock, one IMMEDIATE transaction, guarded statements, insert on every outcome (CORE-02, CORE-05, CORE-06 part 1).
4. T5.4 CLI wiring (lock, new report shape).
5. T5.5 DB-17 cleanup. It can be done any time.

Cross-topic dependencies:
- T3 provides the inter-process lock. Here it is referred to as `db::lock::acquire(path) -> Result<DbLock>`, an exclusive OS file lock tied to the DB path. **This is an assumption about T3's API.**
- PIPE-15 (`relative_components`) should adopt the same `strip_mount` helper introduced in T5.1. Otherwise scan and lookup will disagree about folder chains.
- GEN-07 (Windows `ContentReader::metadata` returns `file_id: None`) limits hardlink exclusion on Windows until it is fixed.
- DB-10 (lookup's size query cannot use `idx_files_size`) is the other half of the lookup latency story. It is referenced, not redesigned, here.

---

### T5.1 Resolve the query's DB key up front, exclude self and hardlinks (closes CORE-03, CORE-06 path half)

- **Verification:**
  - CORE-03 **Confirmed.** `size_candidates` (lookup.rs:206-229) returns every live row of that size. The loop at lookup.rs:100-121 has no id, path or `file_id` exclusion, and `cli/src/lookup.rs:38-52` prints whatever comes back. An indexed file always matches itself, because its stored prefix and full hash equal the query's.
  - CORE-06 (path half) **Partially confirmed.** `persist` canonicalizes (lookup.rs:264), which on Windows yields `\\?\C:\...`. `relative_components` (pass_enumerate.rs:302-311) is a byte `starts_with` with a silent "use whole root" fallback. The stored mount comes from `GetVolumePathNameW` on the *canonicalized* scan root (scan.rs:46-47, volume.rs:261-305). If that call keeps the `\\?\` prefix, the two forms agree and the bug does not trigger. If it returns `C:\`, it does. I could not confirm which form `GetVolumePathNameW` returns for a verbatim input. A drive-letter case mismatch is still possible either way, so the fix is warranted. It is cheap and also hardens PIPE-15.
- **Fix:**
  - Add `resolve_query_key`, which runs before candidate selection. It calls `vi.identify(path)`, then `find_volume_id`, then canonicalizes the path. It strips the stored mount using the component-aware, verbatim-aware, Windows-case-insensitive `strip_mount`, then walks folders with `find_folder`, then calls `find_by_path`. The result is `QueryKey { volume_id, folder_id: Option<i64>, name, existing: Option<FileEntry> }`.
  - Candidates are dropped when `c.id == existing.id`, or when `(c.volume_id, file_id) == (key.volume_id, meta.file_id)` with `file_id` being `Some`.
  - Comparing DB keys rather than reconstructed path bytes avoids path-spelling problems entirely.
- **Rejected alternatives:**
  - Comparing the reconstructed candidate path with the canonical query path. It has the same verbatim and case problems, and it needs a path build per candidate (CORE-07).
  - Reporting self and hardlinks with a "self"/"hardlink" tag. That changes the JSON schema for little gain, so it is left as an optional follow-up.
- **Boilerplate:**

```rust
// crates/core/src/path.rs  (next to strip_verbatim_drive, which becomes pub(crate))

/// Bytes of `path` below `mount`, or `None` when `path` is not under `mount`.
/// Strips `\\?\` before a drive letter on both sides. For Windows-style paths
/// (separator_for == '\\' or a drive designator), it compares ASCII
/// case-insensitively. It requires a component boundary, so `/data2` is not under `/data`.
pub(crate) fn strip_mount<'a>(path: &'a [u8], mount: &[u8]) -> Option<&'a [u8]> {
    let p = strip_verbatim_drive(path);
    let m = strip_verbatim_drive(mount);
    if m.is_empty() {
        return Some(p);
    }
    if p.len() < m.len() {
        return None;
    }
    let windows = separator_for(m) == b'\\' || matches!(m, [d, b':', ..] if d.is_ascii_alphabetic());
    let head = &p[..m.len()];
    let same = if windows { head.eq_ignore_ascii_case(m) } else { head == m };
    if !same {
        return None;
    }
    let rest = &p[m.len()..];
    let mount_ends_sep = matches!(m.last(), Some(b'/' | b'\\'));
    match rest.first() {
        None => Some(rest),
        Some(b'/') => Some(rest),
        Some(b'\\') if windows => Some(rest),
        Some(_) if mount_ends_sep => Some(rest),
        Some(_) => None,
    }
}
```

```rust
// crates/core/src/lookup.rs

/// Where the query file sits in the index. `folder_id`/`existing` are `None`
/// when the parent folder was never scanned.
struct QueryKey {
    volume_id: i64,
    folder_id: Option<i64>,
    name: RawPath,
    existing: Option<crate::model::FileEntry>,
}

/// `Ok(None)` when the query's volume is not indexed.
fn resolve_query_key(
    conn: &Connection,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
) -> Result<Option<QueryKey>> {
    let info = vi.identify(path)?;
    let Some(volume_id) = find_volume_id(conn, &info.uuid)? else {
        return Ok(None);
    };
    let mount = queries::get_volume_mount(conn, volume_id)?.unwrap_or_else(RawPath::empty);
    // canonicalize stays in core for now (same as scan.rs:46; PIPE-15 tracks moving it).
    let native = std::fs::canonicalize(path)?;
    let (parent, name) = codec.split_component(&native);
    let folder_id = match crate::path::strip_mount(parent.as_bytes(), mount.as_bytes()) {
        Some(rel) => find_folder_by_rel(conn, volume_id, rel)?,
        None => {
            tracing::debug!(path = %native.display(), "query path is not under the stored mount");
            None
        }
    };
    let existing = match folder_id {
        Some(fid) => queries::find_by_path(conn, volume_id, fid, &name)?,
        None => None,
    };
    Ok(Some(QueryKey { volume_id, folder_id, name, existing }))
}

/// Replaces find_folder_by_path: takes the already mount-stripped bytes.
fn find_folder_by_rel(conn: &Connection, volume_id: i64, rel: &[u8]) -> Result<Option<i64>> {
    let Some(mut cur) = queries::find_folder(conn, volume_id, 0, &RawPath::empty())? else {
        return Ok(None);
    };
    for comp in rel.split(|&b| b == b'/' || b == b'\\').filter(|c| !c.is_empty()) {
        match queries::find_folder(conn, volume_id, cur, &RawPath::new(comp.to_vec()))? {
            Some(id) => cur = id,
            None => return Ok(None),
        }
    }
    Ok(Some(cur))
}

/// True for the query's own row or another link to the same inode.
fn is_self_or_link(c: &Candidate, key: Option<&QueryKey>, meta: &crate::traits::FileMeta) -> bool {
    let Some(key) = key else { return false };
    if key.existing.as_ref().is_some_and(|e| e.id == c.id) {
        return true;
    }
    c.volume_id == key.volume_id
        && meta.file_id.is_some()
        && c.file_id.as_deref() == meta.file_id.as_deref()
}
```

  `Candidate` gains `file_id: Option<Vec<u8>>`, which means adding `file_id` to the `size_candidates` SELECT. Splitting on `\` in `find_folder_by_rel` keeps the current behaviour. PIPE-15 / CORE-09 decide whether to split on `\` only for Windows-style paths, and this helper should follow that decision.
- **Tests to add:**
  - `lookup` of an already-scanned file whose only twin is itself returns `NoMatch`.
  - With a twin, it returns exactly the twin.
  - A hardlink of an indexed file (Unix) returns `NoMatch`.
  - `strip_mount` unit tests: `C:\` vs `\\?\C:\x`; `c:\` vs `C:\x`; `/data` vs `/data2/x` returns `None`; `/` vs `/a`; empty mount; `\\?\UNC\srv\share` is unchanged.
- **Risk / follow-ups:**
  - On Windows, hardlink exclusion only works once GEN-07 is fixed. Self exclusion by row id works now.
  - `vi.identify` now runs on every lookup, which costs one statfs or `GetVolumePathNameW`. That is negligible.

### T5.2 Rework the candidate loop: classify read failures, filter before building paths, skip pointless full hashes (closes CORE-04, CORE-07)

- **Verification:**
  - CORE-04 **Confirmed.** In lookup.rs:108-114 and 143-149, `Err(_) => None`. In lookup.rs:119 and 158-170, `None` becomes a `MatchRow { freshness: Unmounted }` that goes into `Matches`. Mount state is never checked. Plan P1.8 steps 4, 6 and 7 require a mounted check and "never report Missing as a plain match".
  - CORE-07 **Confirmed.** `reconstruct_file_path` (a fresh prepare plus a recursive CTE, queries.rs:455-480) runs at lookup.rs:104 for every candidate before the stored-prefix comparison. `hash_full` of the query (lookup.rs:128-132) runs whenever `unmounted` is non-empty, even when `survivors` is empty.
  - Extra issue found: a small candidate (size <= prefix_total) without `full_hash` is read twice, because the code ignores the small-file rule for candidates, where prefix equals full (lookup.rs:141-149).
- **Fix:**
  1. First check the stored prefix with no path or I/O. Rows with a stored prefix that differs are dropped immediately.
  2. Build a path only for rows that need I/O (no stored prefix) or that survive. Use `queries::PathCache`. It is per volume, because it caches one mount, so keep a `HashMap<i64, PathCache>`.
  3. On a hash error:
     - Volume not mounted (identify the stored mount and compare the uuid with `volumes.uuid`, cached per volume): the row becomes `Unmounted` and goes in an `unconfirmed` list.
     - Volume mounted and the error is NotFound: the row is `Missing`, has no evidence, and is dropped.
     - Volume mounted and any other error: the row becomes `Unreadable` (new variant) in `unconfirmed`.
  4. Only hash-confirmed rows go into `Matches`. Unconfirmed rows are reported separately, so a script that treats `matches` as "safe to delete" never sees a size-only hit.
  5. Compute `q_full` only when `survivors` is non-empty, or when `--insert` needs it (see T5.3). For small files, `cfull = cprefix`.
  6. A confirmed row whose re-stat says `Missing` is moved to `unconfirmed` and does not count as a plain match (P1.8 step 7).
- **Rejected alternatives:**
  - Keeping `Unmounted` inside `Matches` and only fixing the label. That still reports size-only rows as matches, which is the safety problem.
  - Dropping unmounted candidates entirely. P1.8 explicitly keeps them as "can't be confirmed", and users with offline drives want to know.
  - A batch path query (one CTE for all survivors). `PathCache` already exists and has been reviewed.
- **Boilerplate:**

```rust
// crates/core/src/lookup.rs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    Stale,
    Missing,
    Unmounted,
    /// Volume mounted, file present, but it could not be read (EACCES, sharing violation).
    Unreadable,
}

/// Replaces the bare `LookupOutcome` return.
#[derive(Clone, Debug)]
pub struct LookupReport {
    /// `Matches` holds hash-confirmed rows only.
    pub outcome: LookupOutcome,
    /// Same size, never hashed: `Unmounted`, `Unreadable` or `Missing` (re-stat after a hash match).
    pub unconfirmed: Vec<MatchRow>,
    pub insert: InsertStatus, // T5.3
}

fn is_not_found(e: &CoreError) -> bool {
    matches!(e, CoreError::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
}

/// Per-volume mount check, cached: the stored mount exists and still identifies as this volume.
fn volume_mounted(
    conn: &Connection,
    vi: &dyn VolumeIdentity,
    codec: &dyn PathCodec,
    cache: &mut HashMap<i64, bool>,
    volume_id: i64,
) -> Result<bool> {
    if let Some(&m) = cache.get(&volume_id) {
        return Ok(m);
    }
    let mounted = match (
        queries::get_volume_mount(conn, volume_id)?,
        queries::get_volume_uuid(conn, volume_id)?,
    ) {
        (Some(mount), Some(uuid)) => vi
            .identify(&codec.raw_to_path(&mount))
            .is_ok_and(|info| info.uuid == uuid),
        _ => false,
    };
    cache.insert(volume_id, mounted);
    Ok(mounted)
}

/// What a failed on-demand hash means for a candidate.
fn classify_failure(e: &CoreError, mounted: bool) -> Option<Freshness> {
    match (mounted, is_not_found(e)) {
        (false, _) => Some(Freshness::Unmounted),
        (true, true) => None, // Missing with no evidence: drop
        (true, false) => Some(Freshness::Unreadable),
    }
}
```

  Loop shape for steps 4-7, replacing lookup.rs:95-171:

```rust
let mut paths: HashMap<i64, queries::PathCache> = HashMap::new();
let mut mounted: HashMap<i64, bool> = HashMap::new();
let mut survivors: Vec<(Candidate, RawPath)> = Vec::new();
let mut unconfirmed: Vec<MatchRow> = Vec::new();
let mut on_demand: Vec<OnDemand> = Vec::new(); // T5.3

for c in candidates.into_iter().filter(|c| !is_self_or_link(c, key.as_ref(), &meta)) {
    let Some(csize) = c.size_bytes() else { continue };
    // Cheap reject first: stored prefix differs, no path, no I/O.
    if c.prefix_hash.is_some_and(|h| h != q_prefix) {
        continue;
    }
    let cpath = paths.entry(c.volume_id).or_default().file_path(conn, c.folder_id, &c.name)?;
    if c.prefix_hash.is_none() {
        let native = codec.raw_to_path(&cpath);
        match hash_prefix(reader, &native, csize, cfg) {
            Ok((h, _)) => {
                on_demand.push(OnDemand::prefix(&c, h, small));
                if h != q_prefix {
                    continue;
                }
            }
            Err(e) => {
                let m = volume_mounted(conn, vi, codec, &mut mounted, c.volume_id)?;
                if let Some(f) = classify_failure(&e, m) {
                    unconfirmed.push(MatchRow { volume_id: c.volume_id, path: cpath, freshness: f });
                }
                continue;
            }
        }
    }
    survivors.push((c, cpath));
}

// Full hash of the query only if something can confirm it (or --insert needs it, T5.3).
let need_q_full = !survivors.is_empty() || (insert && key_is_insertable);
let q_full = match (small, need_q_full) {
    (true, _) => Some(q_prefix),
    (false, true) => Some(hash_full(reader, path, size)?),
    (false, false) => None,
};
// For each survivor: cfull = stored full_hash, else q_prefix-equivalent when small,
// else hash_full on demand, with the same classify_failure handling. Equal: re-stat with
// freshness(); Missing goes to `unconfirmed`, otherwise into `matches`.
```

  The unmounted check runs only after a failed read, so a healthy lookup does no extra identify calls.
- **Tests to add:**
  - A state -3 candidate whose file was deleted after the scan, on a mounted volume, does not appear in `matches` or `unconfirmed`.
  - A candidate on a volume whose stored uuid no longer matches (fixture: rewrite `volumes.uuid`) lands in `unconfirmed` as `Unmounted`.
  - Counting reader (`opens()`): with N same-size candidates that have stored, non-matching prefixes, there are 0 candidate opens and only 1 query open, with no full hash.
  - A small -3 candidate survivor is opened exactly once.
  - Unix-only, `#[cfg(unix)]` in a test file: a chmod 000 candidate becomes `Unreadable`.
- **Risk / follow-ups:**
  - The `Matches` / JSON shape changes: a new `unconfirmed` array and the `unreadable` freshness string. This is user-visible, so document it in the README.
  - The `is_not_found` check depends on backend open errors being surfaced as `CoreError::Io`. That holds for the generic backend's `std::fs` calls, but I have not checked the `HandleReader` path.
  - Still slow: the size query itself is a full scan until DB-10 adds `files(size) WHERE state NOT IN (-1,-2)`.

### T5.3 `--insert`: lock, one IMMEDIATE transaction, guarded non-destructive writes, insert on every outcome (closes CORE-02, CORE-05, CORE-06 insert half)

- **Verification:**
  - CORE-02 **Confirmed.** `persist` builds `FileEntry::enumerated` (hashes `None`, state 0, lookup.rs:270-283). `queries::upsert_file`'s `ON CONFLICT(volume_id,folder_id,name) DO UPDATE` (queries.rs:199-231) overwrites `prefix_hash`, `full_hash`, `state` and `last_seen_run_id`. Combined with CORE-03, the self-match triggers this on any indexed file. The computed `q_prefix` / `q_full` are never stored.
  - CORE-05 **Confirmed.**
    - The statements in lookup.rs:240-284 run on the caller's connection in autocommit mode, one implicit transaction each.
    - `IdAllocator::seed(conn)?.file()` is `MAX(id)+1` (ids.rs:16-28), which a running scan's pre-assigned, uncommitted ids can collide with. The collision is worse than the finding says: the writer's `Enumerated` has `ON CONFLICT(id) DO UPDATE` (writer.rs:228-247), so the scan silently overwrites the lookup row rather than dying (see DB-03).
    - `state=5` is set from any state (lookup.rs:243, 251).
    - `dup_groups` is not refreshed, and report reads `dup_groups` (report.rs:210).
  - CORE-06 (insert half) **Confirmed.** `persist` is reached only from `if insert` after the `matches.is_empty()` early return (lookup.rs:173-179), and every early `NoMatch` return (lookup.rs:86-88, 123-125) also skips it. Skips caused by an unknown volume or folder return `Ok(())` silently (lookup.rs:260-268).
- **Fix, with chosen design (a): inter-process lock plus a single `BEGIN IMMEDIATE` transaction on the CLI's RW connection, with no writer thread.**
  - The CLI takes `db::lock::acquire(&db_path)` (T3's API, assumed) *before* opening the DB read-write and keeps it until the end of `--insert`.
    - If a scan holds the lock, `--insert` fails fast with "a scan is running on this database; retry when it finishes". That removes the id race, the interleaving race and DB-03's overwrite.
    - Read-only lookups take no lock, because WAL readers are safe.
  - All reads and hashing happen first. Then a single `conn.transaction_with_behavior(TransactionBehavior::Immediate)` applies every write and commits. A failure part-way leaves nothing.
  - Ids: the INSERT omits `id`, so SQLite assigns `max(rowid)+1` under the write lock. Get it with `RETURNING id`, which needs SQLite >= 3.35. The bundled rusqlite SQLite is newer, but **check that the `rusqlite` feature in use is `bundled`**. `IdAllocator` is no longer used by lookup.
  - Candidate on-demand hashes are written only to rows still in the state and metadata we read, through a guarded UPDATE: `WHERE id=? AND state IN (-3,-4) AND size=? AND mtime_ns=?`. The rules:
    - Prefix only, row -3: store `prefix_hash` and `prefix_bytes`, and **keep** the state. Promoting -3 to -4 would claim a per-volume fact that lookup's global candidate set does not establish.
    - Full hash known: store the hashes, set `prefix_bytes`, and move to state 5. This happens only when the candidate's re-stat is `Fresh`, so a hash of changed content is never stored against old metadata.
    - These rows are terminal from a finished run, and lookup is not a run. P1.4's "forward only within a run" is therefore not crossed, and the next rescan resets to 0 and reuses the hashes per P1.7.1. **Record this in plan P1.4 / P1.8 as an explicit rule.**
  - The query row is persisted on **every** outcome (`NoMatch` included) when its volume and folder are indexed. It gets the terminal state a scan would have given it:
    - -3 when there were no size candidates
    - -4 when there was a prefix but no prefix partner
    - 5 when the full hash is known

    For a large file that needs `q_full` only for the insert, this costs one extra full read, which `--insert` accepts.
  - Existing row (`key.existing`), CORE-02:
    - If the row is fresh (same size, mtime, ctime, file_id) and in state 3/4/5: **no-op**.
    - If it is fresh but less complete (-3/-4): fill hashes with the same guarded UPDATE.
    - If it is stale: overwrite metadata *and* set the newly computed hashes and state in one UPDATE keyed by id. It never writes NULL hashes over valid ones unless the content has changed.
  - The touched `(volume_id, full_hash)` groups are recomputed inside the same transaction with a targeted per-hash recompute. This avoids a full volume recompute, so `report` reflects the insert right away.
  - The return value is an `InsertStatus`. The CLI prints a `note:` on stderr for every `Skipped*` variant (human output) and includes it in JSON.
- **Rejected alternatives:**
  - (b) A short-lived writer thread via `spawn_writer` + `Batch`. The writer's `Enumerated` upsert is exactly the destructive SQL, and its `ON CONFLICT(id)` is DB-03. It would need new guarded variants, and ids would still come from `IdAllocator`. That is a thread and a channel for 1 to N rows, and it still needs the lock for exclusion. "Single writer" is a rule about one process's scan. With the T3 lock, lookup is the only writer anyway.
  - Refusing when `latest_running_run` exists, with no lock. A scan killed with -9 leaves a `running` row forever, and the check is racy between check and write.
  - AUTOINCREMENT. It needs a schema change and does nothing about a concurrent scan's pre-assigned ids.
  - Fixing only `queries::upsert_file` to keep hashes. Its other callers (tests) want overwrite semantics. The real bug is that lookup should not upsert blindly.
- **Schema:** no change. It stays at v2.
- **Boilerplate:**

```rust
// crates/core/src/lookup.rs

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertStatus {
    NotRequested,
    Inserted { id: i64 },
    Updated { id: i64 },
    /// Row already indexed, fresh and fully hashed: left alone.
    Unchanged { id: i64 },
    SkippedVolumeNotIndexed,
    SkippedFolderNotIndexed,
}

/// One on-demand hash to persist, with the metadata it was computed against.
struct OnDemand {
    id: i64,
    size: i64,
    mtime_ns: i64,
    prefix: Option<Hash>,
    full: Option<Hash>,
}

/// Everything `--insert` will write, computed before the transaction opens.
struct InsertPlan<'a> {
    key: Option<&'a QueryKey>,
    meta: &'a crate::traits::FileMeta,
    q_prefix: Option<Hash>,
    q_full: Option<Hash>,
    on_demand: Vec<OnDemand>,
}

/// State a scan would have given the query row.
fn query_state(q_prefix: Option<Hash>, q_full: Option<Hash>) -> State {
    match (q_prefix, q_full) {
        (_, Some(_)) => State::Done,
        (Some(_), None) => State::UniquePrefix,
        (None, None) => State::UniqueSize,
    }
}

/// Applies an InsertPlan in one IMMEDIATE transaction. The caller must hold
/// `db::lock::acquire(db_path)` (T3) for the whole lookup.
pub(crate) fn apply_insert(conn: &mut Connection, plan: &InsertPlan<'_>, prefix_total: i64) -> Result<InsertStatus> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut touched: Vec<(i64, Hash)> = Vec::new();

    for d in &plan.on_demand {
        let (sql, done) = match d.full {
            Some(_) => ("UPDATE files SET prefix_hash=COALESCE(?1,prefix_hash), full_hash=?2, \
                         prefix_bytes=CASE WHEN size<=?6 THEN size ELSE ?6 END, state=5 \
                         WHERE id=?3 AND state IN (-3,-4) AND size=?4 AND mtime_ns=?5", true),
            None => ("UPDATE files SET prefix_hash=?1, \
                      prefix_bytes=CASE WHEN size<=?6 THEN size ELSE ?6 END \
                      WHERE id=?3 AND state=-3 AND prefix_hash IS NULL AND size=?4 AND mtime_ns=?5", false),
        };
        let n = tx.execute(sql, params![
            d.prefix.as_ref().map(|h| h.as_bytes().as_slice()),
            d.full.as_ref().map(|h| h.as_bytes().as_slice()),
            d.id, d.size, d.mtime_ns, prefix_total,
        ])?;
        if n == 1 && done {
            if let (Some(f), Some(vol)) = (d.full, volume_of(&tx, d.id)?) {
                touched.push((vol, f));
            }
        }
    }

    let status = match plan.key {
        None => InsertStatus::SkippedVolumeNotIndexed,
        Some(QueryKey { folder_id: None, .. }) => InsertStatus::SkippedFolderNotIndexed,
        Some(key @ QueryKey { folder_id: Some(fid), .. }) => {
            let state = query_state(plan.q_prefix, plan.q_full);
            let run = latest_run(&tx, key.volume_id)?;
            let status = match &key.existing {
                Some(e) if is_fresh(e, plan.meta) && matches!(e.state, State::FullHashed | State::Verified | State::Done) => {
                    InsertStatus::Unchanged { id: e.id }
                }
                Some(e) => {
                    // Stale or less complete: overwrite metadata + hashes by id (never by upsert).
                    update_query_row(&tx, e.id, plan, state, run, prefix_total)?;
                    InsertStatus::Updated { id: e.id }
                }
                None => {
                    let id = insert_query_row(&tx, key, *fid, plan, state, run, prefix_total)?;
                    InsertStatus::Inserted { id }
                }
            };
            if let (State::Done, Some(f)) = (state, plan.q_full) {
                if !matches!(status, InsertStatus::Unchanged { .. }) {
                    touched.push((key.volume_id, f));
                }
            }
            status
        }
    };

    touched.sort_unstable_by_key(|(v, h)| (*v, *h.as_bytes()));
    touched.dedup();
    let now = crate::model::now_unix_ns();
    for (vol, h) in &touched {
        queries::recompute_group_for_hash(&tx, *vol, h, now)?;
    }
    tx.commit()?;
    Ok(status)
}

fn is_fresh(e: &crate::model::FileEntry, m: &crate::traits::FileMeta) -> bool {
    e.size == m.size && e.mtime_ns == m.mtime_ns && e.ctime_ns == m.ctime_ns && e.file_id == m.file_id
}

fn insert_query_row(/* tx, key, folder_id, plan, state, run, prefix_total */) -> Result<i64> {
    // INSERT INTO files(volume_id,folder_id,name,file_id,nlink,size,mtime_ns,ctime_ns,attrs,
    //   prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,last_seen_run_id)
    // VALUES(...) RETURNING id
    // No ON CONFLICT: under the lock with `existing == None`, a conflict is a real error.
    todo!()
}

fn update_query_row(/* tx, id, plan, state, run, prefix_total */) -> Result<()> {
    // UPDATE files SET size=?, mtime_ns=?, ctime_ns=?, file_id=?, nlink=?, attrs=?,
    //   prefix_hash=?, full_hash=?, prefix_bytes=?, state=?, err=NULL, scanned_at=?,
    //   last_seen_run_id=MAX(last_seen_run_id, ?) WHERE id=?
    todo!()
}
```

```rust
// crates/core/src/db/queries.rs  (next to recompute_groups; same SQL scoped to one hash)

/// Recompute one `(volume_id, full_hash)` group after an out-of-run change (lookup --insert).
pub fn recompute_group_for_hash(conn: &Connection, volume_id: i64, full_hash: &Hash, now: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM dup_groups WHERE volume_id=?1 AND full_hash=?2",
        params![volume_id, full_hash.as_bytes()],
    )?;
    conn.execute(
        "INSERT INTO dup_groups(volume_id,full_hash,size,member_count,distinct_inodes,\
         wasted_logical,wasted_physical,sharing_kind,computed_at) \
         SELECT volume_id, full_hash, size, COUNT(*), COUNT(DISTINCT COALESCE(file_id, id)), \
                size*(COUNT(DISTINCT COALESCE(file_id, id))-1), NULL, 0, ?3 \
         FROM files WHERE volume_id=?1 AND full_hash=?2 AND state IN (3,4,5) \
         GROUP BY full_hash, size HAVING COUNT(DISTINCT COALESCE(file_id, id)) > 1",
        params![volume_id, full_hash.as_bytes(), now],
    )?;
    Ok(())
}
```

  Public entry point. It keeps one function that the CLI calls, and the read and write halves stay private:

```rust
// crates/core/src/lookup.rs
pub fn lookup(
    conn: &mut Connection, // &mut only so --insert can open a Transaction; read-only path never writes
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    vi: &dyn VolumeIdentity,
    path: &Path,
    cfg: &Config,
    insert: bool,
) -> Result<LookupReport>;
```

  The `prefix_bytes` / `prefix_total` value comes from `cfg.prefix_total()`. CORE-17 (read the prefix sizes from `meta`) applies to this value as well.
- **Tests to add:**
  - `--insert` on an indexed, unchanged file: `full_hash`, `prefix_hash` and `state` are byte-identical before and after, and `report` still lists its group (direct CORE-02 regression).
  - `--insert` of a new unique file (no size partner) inserts a row in state -3.
  - A new file with a twin inserts a row in state 5 with both hashes, and `report` immediately shows a 2-member group without a rescan.
  - A file on a non-indexed folder returns `SkippedFolderNotIndexed` and writes no row.
  - A -3 candidate hashed on demand gets `prefix_hash` and `full_hash` with state 5. A -3 candidate whose metadata changed after the scan (touch it) is not updated.
  - Atomicity: inject a failure by dropping the `dup_groups` table in a test DB. The transaction rolls back and no on-demand hash is persisted.
  - With a CLI test: `lookup --insert` while another process holds `db::lock` exits non-zero with the "scan is running" message.
  - Rescan after `--insert` reuses the inserted hashes, verified with the counting reader showing no re-read.
- **Risk / follow-ups:**
  - Depends on T3's lock existing and on `scan` taking it for its whole run. Until then, keep a fallback: refuse `--insert` when `latest_running_run` is `Some`. Document that it is best-effort only.
  - The `&mut Connection` signature change ripples into the CLI and tests.
  - On Windows, the inserted row has `file_id NULL` until GEN-07 lands, so the next rescan re-hashes it (the GEN-07 impact).

### T5.4 CLI wiring (supports CORE-03, CORE-04, CORE-05, CORE-06)

- **Verification:** `cli/src/lookup.rs:22-47` opens RW for `--insert` with no exclusion and prints only `LookupOutcome`.
- **Fix:**
  - For `--insert`: `let _lock = dedupe_core::db::lock::acquire(&db).context("another dedupe process is using this database")?;` before `Db::open`. The lock lives until the function returns.
  - Pass `&mut conn`.
  - Print `unconfirmed` as a separate section ("could not confirm (N)") and add it as an `"unconfirmed"` array in JSON.
  - Print `InsertStatus` as `note: not inserted: volume not indexed` (or `folder not indexed`) on stderr. In JSON, emit `"insert": "inserted" | "updated" | "unchanged" | "skipped_volume" | "skipped_folder"`.
  - Add `"unreadable"` to `freshness_str`.
  - Also replace `expect("lookup serializes")` (cli/src/lookup.rs:80) with `?` via anyhow, per the no-`expect` rule.
- **Tests to add:**
  - Snapshot the JSON shape in `crates/cli/tests` for each outcome.
  - Check that `insert` is absent or `not_requested` without `--insert`.

### T5.5 Remove `impl Index for SqliteIndex` and its dead helpers (closes DB-17), Low

- **Verification:** **Confirmed.**
  - Production uses only `SqliteIndex::new` + `prefix_singletons` (pass_prefix.rs:115-116).
  - `Index::insert` always queries prefix partners (index/sqlite.rs, `impl Index` block).
  - `singletons()` maps `Err` to an empty iterator.
  - `prefix_groups` is documented as "streamed ... bounded" but returns a `Vec`, and only its test calls it. `queries::prefix_partners_run` and `queries::prefix_groups` have no production callers either.
- **Fix:** delete rather than repair. Nothing needs a polymorphic `Index` over the DB.
  - Delete `impl Index for SqliteIndex`, `SqliteIndex::prefix_groups`, `queries::prefix_partners_run` and `queries::prefix_groups`.
  - Reword the module docs to say "prefix-pass over-budget fallback: committed singleton query".
  - Keep the `Index` trait for `MemoryIndex`, and fix `index/mod.rs` so it says "prefix pass", not "`prefix_map` / `full_map`".
  - Adjust the sqlite.rs unit test to assert only `prefix_singletons`.
  - Consider whether `SqliteIndex` itself should collapse into a free function, `queries::singleton_prefixes(ctx.read, volume_id, run_id)`. That would be the most direct option, but it is optional.
- **Rejected alternative:** making `singletons` return `Result` and parameterizing on prefix or full. That fixes an API no one calls.
- **Tests:** the existing `prefix_singletons_and_partners_are_run_scoped`, trimmed to the singleton assertion.

---

## T6. Windows $MFT fast path, MFT cache and Win32 I/O hygiene

**Findings covered:** MFT-01, MFT-02, MFT-03, MFT-04, MFT-05, MFT-06, MFT-07, MFT-08, MFT-09, MFT-10, MFT-11, MFT-12, MFT-13, MFT-14, MFT-15, MFT-16, MFT-17, WIO-03, WIO-06, WIO-07, WIO-08, WIO-10, WIO-11, WIO-12, WIO-13.

All paths are relative to the workspace root `/mnt/c/Users/haris/Downloads/Software/PythonPrograms/rusty-deDeuper`. Nothing was built or run. Win32 behaviour that I could not confirm is marked **(unconfirmed)**.

**Root causes:**
- The hot path goes through `ntfs::Ntfs::file` (ntfs-0.4.0 `src/ntfs.rs:85-110`), which re-reads record 0 and its base-record-only `$DATA` runlist on every call, behind a one-slot `SectorReader`. This causes MFT-01 and MFT-02, and it makes MFT-14 and MFT-15 (extra per-record reads) cost more.
- "Partial MFT result" is not treated as a failure. A record error (MFT-03), a BAAD record (MFT-08), a missing or non-directory root (MFT-12), and a stale parent (MFT-07) all end in a silent gap that the scan reports as success, gets persisted in the cache, and that core's rescan then deletes as stale.
- The root and its classification are not taken from the same place as the walker. Reparse classification ignores the name-surrogate bit (MFT-04). The root FRN follows the link but the volume comes from the unresolved string (MFT-05), which leads into MFT-12.
- The cache is trusted on journal continuity alone (MFT-09, WIO-03, MFT-10). The cache code also has some unneeded copies and API surface (MFT-11, MFT-16).
- The Win32 handle and OVERLAPPED wrappers are hand-rolled where std already provides them (WIO-06, WIO-07, WIO-08). The small error-variant and doc items follow from that (WIO-10, WIO-12, WIO-13, MFT-17).

**Recommended order of work:**
1. T6.1 Fail closed: zero-tolerance record errors, BAAD as error, root checks, never cache a degraded tree (MFT-03, MFT-08, MFT-12). This is the data-loss fix. It is small and needs no new I/O code.
2. T6.2 Resolve the root once, by handle (MFT-05, WIO-11).
3. T6.3 Reparse classification by name-surrogate bit (MFT-04).
4. T6.4 Detect `$MFT` with `$ATTRIBUTE_LIST` and fall back (MFT-02, stage A).
5. T6.5 Stage A, a two-slot `SectorReader` (pinned record-0 chunk plus a larger streaming chunk) (MFT-01). Stage B, an own bulk MFT reader, comes after measuring (MFT-01, MFT-02 stage B, MFT-14, MFT-15).
6. T6.6 Parent sequence check (MFT-07), together with the cache format v2 bump.
7. T6.7 Cache trust: post-read delta, dirty bit, spot check (WIO-03, MFT-09).
8. T6.8 std `OwnedHandle` and raw-pointer OVERLAPPED (WIO-07, WIO-06).
9. T6.9 Medium and Low items (MFT-06, MFT-10, MFT-11, MFT-13, MFT-14, MFT-15, MFT-16, MFT-17, WIO-08, WIO-10, WIO-12, WIO-13).

---

### T6.1 Fail closed on any MFT gap and never persist a degraded tree (closes MFT-03, MFT-08, MFT-12)

- **Verification:**
  - MFT-03 Confirmed.
    - `crates/backend-windows/src/mft_walk.rs` `read_tree` counts `Err` and continues until `records_skipped_errors > record_error_limit` (`(n/20).max(1000)`).
    - `enumerator.rs` logs the count only at `tracing::debug!` and pushes no notice.
    - `walk_mft_volume` calls `store_cache` with no check on `stats.records_skipped_errors`.
    - `apply_delta` re-reads only journal-touched records, so a gap persists across later scans.
  - MFT-08 Confirmed. `records.rs` `extract_record` matches `Err(NtfsError::InvalidFileSignature { .. }) => return Ok(None)`. The crate error carries `actual: [u8; 4]` (ntfs-0.4.0 `file.rs:509-521`), so "BAAD" and garbage are indistinguishable from zeros today.
  - MFT-12 Confirmed. `tree.rs` `emit_items` returns `Ok(())` when `records.get(&root)` is `None` or `kind_of(root) != Directory`. `push` drops records 0..=23 except 5.
- **Fix:**
  1. Retry a failed record once. A torn read of a live record shows up as a fixup mismatch. The retry must bypass the chunk cache: add `SectorReader::invalidate()`.
  2. After the retry, any remaining record error ends the MFT attempt with `MftFailure::Fallback`. Zero tolerance replaces the 5% limit, because a single skipped directory record hides a subtree that core then deletes as stale.
  3. An all-zero signature is free space. "BAAD" or any other non-`FILE` signature becomes `RecordError::Corrupt`. Under rule 2 that falls back, and the notice tells the user to run chkdsk.
  4. Check the root before emission. A missing root, or a root that is not a directory, ends with `Fallback`, not an empty success.
  5. With rule 2 in place a degraded tree never reaches `store_cache`. Keep an explicit `debug_assert!`-free guard anyway: store only when `stats.records_skipped_errors == 0`.
- **Rejected alternatives:**
  - Keep the 5% tolerance and add a notice: the rows are still deleted by core's stale pass, so a notice does not stop the data loss.
  - Mark subtrees incomplete through core: the `Enumerator` trait has no partial-result channel, so this needs a core API change plus pass_enumerate scoping. It is the right long-term fix for the walker too (see follow-ups), but too large for this topic.
  - Classify failed records as dir or file from the raw header and tolerate failed files: still deletes that file's row, and adds complexity.
- **Boilerplate:**
```rust
// crates/backend-windows/src/sector.rs
impl<R: Read + Seek> SectorReader<R> {
    /// Drops cached bytes so the next read goes to the source. Used to retry a torn record.
    pub fn invalidate(&mut self) {
        self.cache_end = self.cache_start;
    }
}
```
```rust
// crates/backend-windows/src/records.rs  (extract_record, signature arm)
let file = match ntfs.file(fs, record) {
    Ok(file) => file,
    // Never written: free space.
    Err(NtfsError::InvalidFileSignature { actual: [0, 0, 0, 0], .. }) => return Ok(None),
    // "BAAD" (multi-sector transfer failed) or garbage: a damaged slot, not free space.
    Err(NtfsError::InvalidFileSignature { actual, .. }) if &actual == b"BAAD" => {
        return Err(RecordError::Corrupt("record is marked BAAD; run chkdsk"))
    }
    Err(NtfsError::InvalidFileSignature { .. }) => {
        return Err(RecordError::Corrupt("record has an unknown signature"))
    }
    Err(err) => return Err(err.into()),
};
```
```rust
// crates/backend-windows/src/mft_walk.rs
pub trait RecordSource {
    fn record_count(&self) -> u64;
    fn record(&mut self, number: u64) -> Result<Option<RecordFacts>, RecordError>;
    /// Forget any cached bytes before a retry. Default: nothing cached.
    fn invalidate(&mut self) {}
}

impl<R: Read + Seek> RecordSource for NtfsRecordSource<R> {
    // ...
    fn invalidate(&mut self) {
        self.fs.invalidate();
    }
}

/// One retry, for a record caught mid-write (fixup mismatch on a live volume).
fn read_with_retry<S: RecordSource>(
    source: &mut S,
    number: u64,
) -> Result<Option<RecordFacts>, RecordError> {
    match source.record(number) {
        Err(RecordError::Io(_)) | Err(RecordError::Ntfs(_)) | Err(RecordError::Corrupt(_)) => {
            source.invalidate();
            source.record(number)
        }
        ok => ok,
    }
}

pub enum MftError {
    BudgetExceeded(BudgetExceeded),
    /// A record could not be read after one retry. Any gap can hide a subtree, so the
    /// whole MFT result is discarded and the caller falls back to the walker.
    RecordFailed { record: u64, error: RecordError },
    /// The scan root record is absent, filtered, or not a directory.
    RootUnusable(&'static str),
    Cancelled,
    Visit(CoreError),
}
// Display: RecordFailed => "MFT record {record} could not be read: {error}"
//          RootUnusable(why) => "scan root is not usable on the MFT path: {why}"

// read_tree loop body (replaces the error-limit branch; delete record_error_limit and
// MIN_RECORD_ERROR_LIMIT):
match read_with_retry(source, number) {
    Ok(Some(facts)) => tree.push(facts).map_err(MftError::BudgetExceeded)?,
    Ok(None) => {}
    Err(error) => {
        stats.records_skipped_errors += 1;
        return Err(MftError::RecordFailed { record: number, error });
    }
}
```
```rust
// crates/backend-windows/src/tree.rs
impl VolumeTree {
    /// Checks the scan root before anything is visited. An empty success would let the
    /// rescan delete every row under the root.
    pub fn check_root(&self) -> Result<(), &'static str> {
        match self.records.get(&self.root) {
            None => Err("root record is not in the index (metafile range or unreadable)"),
            Some(f) if kind_of(f) != EntryKind::Directory => Err("root record is not a directory"),
            Some(_) => Ok(()),
        }
    }
}
// emit_items: keep the early returns but make them unreachable in practice by calling
// check_root first; leave a comment pointing at check_root.
```
```rust
// crates/backend-windows/src/enumerator.rs  (walk_mft_volume, after obtain_tree)
let (tree, mut stats, delta) = obtain_tree(/* ... */)?;
tree.check_root()
    .map_err(|why| MftFailure::Fallback(MftError::RootUnusable(why).to_string()))?;
let unchanged = matches!(delta, Some(DeltaStats { entries: 0, .. }));
// Only a complete tree is ever written. read_tree already fails on any record error;
// this guard keeps that true if the policy is ever relaxed.
if stats.records_skipped_errors == 0 {
    if let Some(site) = site.as_ref().filter(|_| !unchanged) {
        if !cancel.is_cancelled() {
            store_cache(site, &tree, notices);
        }
    }
}
// classify(): RecordFailed and RootUnusable fall into `other => Fallback(...)`, so the
// existing notice "fell back to the standard walker: MFT record N could not be read: ..."
// is what the user sees.
```
- **Tests to add:**
  - `read_tree_should_fail_when_one_record_errors_twice`: FakeSource with one failing slot returns `RecordFailed`.
  - `read_tree_should_succeed_when_record_fails_once_then_reads`: FakeSource counts calls and fails only the first time.
  - `extract_record_should_return_corrupt_when_signature_is_baad`: in-memory NTFS image with the record header patched to "BAAD". The crate test-image approach is in `records.rs` tests.
  - `check_root_should_err_when_root_is_metafile`, `check_root_should_err_when_root_is_symlink_dir`.
  - `walk_mft_volume` cache-not-written test through `obtain_tree` with a failing source (portable part).
- **Risk / follow-ups:**
  - A volume with one permanently BAAD record now always uses the walker until chkdsk runs. That is the correct trade (walker cost, no data loss), and the notice names the cause.
  - Out of topic but same class: `backend-generic/src/walk.rs` uses `filter_map(|e| e.ok())`, so an unreadable directory under the walker is also dropped silently and later deleted as stale. Hand this to the walker or core topic.

### T6.2 Resolve the scan root once, by handle (closes MFT-05, WIO-11)

- **Verification:**
  - MFT-05 Partially confirmed.
    - `enumerator.rs` `walk_mft_volume` picks the volume with `volume_root(root)` (`GetVolumePathNameW` on the string).
    - `volume_io.rs` `frn_of` opens with `FILE_FLAG_BACKUP_SEMANTICS` and no `FILE_FLAG_OPEN_REPARSE_POINT`, so it follows a junction to its target.
    - Nothing compares the root handle's volume with the raw volume.
    - Whether `GetVolumePathNameW` follows junctions is **(unconfirmed)**. The fix below is correct either way.
  - WIO-11 Confirmed.
    - `caps.rs` `probe_capabilities` opens the raw volume and drops it.
    - `walk_mft_volume` opens it again.
    - `choose_mode` checks `admin` before `raw_volume_openable`.
- **Fix:**
  - Open the root once, following links. This matches walkdir 2.5's default `follow_root_links(true)` and the current `frn_of` behaviour.
  - From that one handle, read the file index and `dwVolumeSerialNumber` with `GetFileInformationByHandle`, and the GUID volume path with `GetFinalPathNameByHandleW(VOLUME_NAME_GUID)`.
  - Open the raw volume from the GUID root. `raw_volume_path` already accepts `\\?\Volume{GUID}\`.
  - Cross-check the low 32 bits of the NTFS serial against `dwVolumeSerialNumber`, and fall back on mismatch. That `dwVolumeSerialNumber` is the low half of the 64-bit NTFS serial is **(unconfirmed)**; comparing only the low 32 bits is safe whichever way it turns out.
  - Keep the emitted path prefix as the unresolved `root`, which is what walkdir yields.
  - `probe_capabilities` takes the resolved root and returns the opened `RawVolume`, so it is reused. `choose_mode` treats `raw_volume_openable` as decisive and uses `admin` only to pick the message.
- **Rejected alternatives:**
  - Refuse reparse roots outright: walkdir follows them, so the two modes would disagree.
  - Open with `OPEN_REPARSE_POINT`: yields a symlink record and lands in MFT-12.
- **Boilerplate:**
```rust
// crates/backend-windows/src/volume_io.rs  (win32 module)
use windows_sys::Win32::Storage::FileSystem::{GetFinalPathNameByHandleW, VOLUME_NAME_GUID};

/// The scan root as the file system sees it after following any link.
pub struct ResolvedRoot {
    /// File reference of the (followed) root directory.
    pub frn: u64,
    /// `dwVolumeSerialNumber` of the volume that really holds the root.
    pub serial_low: u32,
    /// `\\?\Volume{GUID}\`, the raw volume to read.
    pub volume_root: Vec<u16>,
}

pub fn resolve_root(path: &Path) -> io::Result<ResolvedRoot> {
    let handle = open_handle(
        &to_wide_nul(path.as_os_str()),
        FILE_READ_ATTRIBUTES,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
        FILE_FLAG_BACKUP_SEMANTICS, // follows junctions/symlinks, like walkdir's root
    )?;
    let info = by_handle_info(&handle)?; // factor out of frn_of
    let mut buf = vec![0u16; 512];
    let len = loop {
        let cap = u32::try_from(buf.len()).map_err(io::Error::other)?;
        // SAFETY: handle is live; buf is writable for `cap` u16 units and that count is passed.
        let n = unsafe { GetFinalPathNameByHandleW(handle.raw(), buf.as_mut_ptr(), cap, VOLUME_NAME_GUID) };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        let n = usize::try_from(n).map_err(io::Error::other)?;
        if n < buf.len() {
            break n;
        }
        buf.resize(n + 1, 0); // n is the required size including NUL
    };
    Ok(ResolvedRoot {
        frn: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        serial_low: info.dwVolumeSerialNumber,
        volume_root: guid_volume_prefix(&buf[..len]).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "final path has no volume GUID prefix")
        })?,
    })
}
```
```rust
// crates/backend-windows/src/volume_io.rs  (portable, unit-tested)
/// `\\?\Volume{GUID}\rest` -> `\\?\Volume{GUID}\`. `None` for any other shape.
pub fn guid_volume_prefix(final_path: &[u16]) -> Option<Vec<u16>> {
    const PREFIX_LEN: usize = r"\\?\Volume{".len() + 36 + "}".len(); // 49
    let head = String::from_utf16(final_path.get(..PREFIX_LEN)?).ok()?;
    let guid = head.strip_prefix(r"\\?\Volume{")?.strip_suffix('}')?;
    is_guid(guid).then(|| format!("{head}\\").encode_utf16().collect())
}
```
```rust
// crates/backend-windows/src/enumerator.rs  (walk_mft_volume head)
let resolved = resolve_root(root).map_err(at_stage("resolve the scan root"))?;
let volume = open_raw_volume(Path::new(&OsString::from_wide(&resolved.volume_root)))
    .map_err(at_stage("open raw volume"))?;
let serial = ntfs_volume_serial(&volume).map_err(at_stage("read NTFS volume data"))?;
if serial as u32 != resolved.serial_low {
    return Err(MftFailure::Fallback(
        "scan root resolves to a different volume than the one opened".to_owned(),
    ));
}
let root_record = record_number_of(resolved.frn);
// root_prefix stays codec.path_to_raw(root): paths keep the caller's spelling.
```
```rust
// crates/backend-windows/src/caps.rs
pub fn choose_mode(caps: &Capabilities, no_mft: bool) -> Mode {
    if no_mft { return Mode::Fallback(FallbackReason::Disabled); }
    if !caps.ntfs { return Mode::Fallback(FallbackReason::NotNtfs); }
    if !caps.raw_volume_openable {
        // `admin` only picks the message; the open itself is decisive.
        return Mode::Fallback(if caps.admin {
            FallbackReason::RawVolumeUnavailable
        } else {
            FallbackReason::NotElevated
        });
    }
    Mode::Mft
}
// win32::probe_capabilities(path) -> (Capabilities, Option<(ResolvedRoot, RawVolume)>):
// resolve_root once, GetVolumeInformationW on resolved.volume_root, and keep the opened
// RawVolume to hand to walk_mft_volume instead of reopening it.
```
- **Tests to add:**
  - Unit tests for `guid_volume_prefix`: valid path, a bare `\\?\Volume{...}` with no tail, a drive-letter path giving `None`, and a bad GUID.
  - `choose_mode` table tests for the reordered rules.
  - Windows `#[ignore]`: junction `C:\tmp\link -> D:\data` compared against the walker. This needs two volumes. With one volume, test a same-volume junction root and check that items match the walker.
- **Risk / follow-ups:**
  - `handle.rs` `hint_for` also uses `volume_root(path)` for open-by-id. It has the same junction issue; that belongs to the WIO-01 owner.

### T6.3 Reparse classification by the name-surrogate bit (closes MFT-04)

- **Verification:** MFT-04 Confirmed.
  - `tree.rs` `kind_of` maps `(true, true, _)` to `Symlink` for any tag, and maps files to Symlink only for `SYMLINK | MOUNT_POINT`.
  - std's Windows `FileType` sets `is_symlink = attrs & REPARSE_POINT != 0 && tag & 0x2000_0000 != 0`, with `is_dir = !is_symlink && is_directory`. This is from my reading of std's `sys/pal/windows/fs.rs`; recheck it against the 1.92 rust-src.
  - walkdir with `follow_links(false)` therefore descends cloud-tagged directories and reports LX_SYMLINK files as symlinks.
- **Fix:** Classify every record (file or directory) as a link only when its tag is a name surrogate.
  - If the tag could not be read (`reparse_tag == None` with the reparse attribute set), keep the conservative choice: a directory is a `Symlink` (not descended) and a file is a `RegularFile`, as today.
  - Placeholder attribute bits still map to `attrs::PLACEHOLDER` unchanged.
- **Rejected alternative:** an allow-list of cloud tags. The list is open-ended (`0x9000_x01A` with variable x), and std uses the bit.
- **Boilerplate:**
```rust
// crates/backend-windows/src/tree.rs
/// Microsoft reparse-tag bit: the reparse point names another file system object.
/// std (and so walkdir) treats exactly these as symlinks on Windows.
const IO_REPARSE_TAG_NAME_SURROGATE: u32 = 0x2000_0000;

pub fn kind_of(f: &RecordFacts) -> EntryKind {
    let reparse = has_reparse_point(f);
    let surrogate = f.reparse_tag.is_some_and(|t| t & IO_REPARSE_TAG_NAME_SURROGATE != 0);
    // Tag unreadable: do not descend into a directory we cannot classify.
    let unknown_dir_reparse = f.is_dir && reparse && f.reparse_tag.is_none();
    match (f.is_dir, reparse && surrogate) {
        (_, true) => EntryKind::Symlink,
        (true, false) if unknown_dir_reparse => EntryKind::Symlink,
        (true, false) => EntryKind::Directory,
        (false, false) => EntryKind::RegularFile,
    }
}
// Remove IO_REPARSE_TAG_MOUNT_POINT / IO_REPARSE_TAG_SYMLINK constants (both carry the bit),
// and update the emit_items doc ("Directories with a name-surrogate reparse point ...").
```
- **Tests to add:**
  - The cloud tag `0x9000_601A` on a directory gives `Directory`, and its children are emitted. The existing test using `CLOUD_TAG` must flip its expectation.
  - LX_SYMLINK `0xA000_001D` on a file gives `Symlink`.
  - A directory with the reparse bit and no tag gives `Symlink`.
  - The Windows self-check gets a OneDrive-style folder (see T6.9 MFT-13).
- **Risk / follow-ups:**
  - The walker's `FindFirstFile` on a not-yet-populated cloud folder may trigger the cloud provider to populate it. The MFT path sees only local children, so the two modes can legitimately differ for unhydrated folders. Document this in CLAUDE.md Gotchas.

### T6.4 Refuse a `$MFT` whose runlist spills into an attribute list (closes MFT-02, stage A)

- **Verification:** MFT-02 Confirmed against the crate.
  - ntfs-0.4.0 `src/ntfs.rs:93-107` carries the comment "this code assumes that the MFT does not have an Attribute List!". It uses `find_resident_attribute(Data)` on record 0 only.
  - A seek past the last base-record run gives `data_position() == None`, which becomes `InvalidFileRecordNumber`. Today that becomes a skipped record (and after T6.1, a fallback, but only when that record is reached and after a long read).
- **Fix:** At `NtfsRecordSource::open`, parse record 0. If it has an `$ATTRIBUTE_LIST`, return a new `RecordError::Unsupported`, which `at_stage("open NTFS records")` turns into an immediate fallback with a clear notice. Stage B (T6.5) removes the restriction by resolving the full runlist itself.
- **Rejected alternative:** catch `InvalidFileRecordNumber` per record. That only fails late, after most of the read is done.
- **Boilerplate:**
```rust
// crates/backend-windows/src/records.rs
pub enum RecordError {
    Ntfs(NtfsError),
    Io(io::Error),
    Corrupt(&'static str),
    /// A valid layout this reader does not handle. The caller falls back to the walker.
    Unsupported(&'static str),
}

/// Fails when `$MFT` (record 0) has an `$ATTRIBUTE_LIST`. ntfs 0.4.0 locates records from
/// the base record's `$DATA` runlist only, so records in later extents would be unreadable.
pub fn ensure_mft_runlist_in_base<T: Read + Seek>(ntfs: &Ntfs, fs: &mut T) -> Result<(), RecordError> {
    let mft = ntfs.file(fs, 0)?;
    for attribute in mft.attributes_raw() {
        if attribute?.ty()? == NtfsAttributeType::AttributeList {
            return Err(RecordError::Unsupported(
                "$MFT is fragmented into an attribute list; the MFT fast path does not support it",
            ));
        }
    }
    Ok(())
}
```
```rust
// crates/backend-windows/src/mft_walk.rs  (NtfsRecordSource::open, after the record-size check)
ensure_mft_runlist_in_base(&ntfs, &mut fs)?;
```
- **Tests to add:**
  - A portable test with a synthetic NTFS image whose record 0 carries a `0x20` attribute; it expects `Unsupported`.
  - Windows manual check: run `fsutil file layout C:\$MFT` on a large old volume and look for an attribute list.
- **Risk / follow-ups:** Heavily fragmented MFTs lose the fast path until stage B lands. That is a correct, visible degradation.

### T6.5 MFT read throughput: pinned record-0 chunk now, own bulk reader next (closes MFT-01; stage B also closes MFT-02 fully, MFT-14, MFT-15)

- **Verification:** MFT-01 Confirmed.
  - `records.rs` `extract_record` calls `ntfs.file(fs, record)`, which parses record 0 and decodes its runlist every call (ntfs-0.4.0 `ntfs.rs:85-110`).
  - `sector.rs` `SectorReader` holds one cache (`cache`, `cache_start`, `cache_end`) of `READ_CHUNK_BYTES = 1 << 20` (`mft_walk.rs`).
  - Past the first MiB, every record alternates between two chunks. Each `extract_record` also issues `base_reference_of` (a fresh seek and read) and `file_name_namespace` (an extra value read per name).
  - The wall-clock magnitude is unmeasured.
- **Fix, stage A (small, ship with T6.1):**
  - Give `SectorReader` two slots. Slot P is pinned to the chunk that holds `ntfs.mft_position()` (record 0). Slot S streams everything else with a larger chunk (4 MiB).
  - After this, each record costs a memcpy of record 0 plus CPU for the runlist seek, and the target data comes from a sequential 4 MiB window. I/O drops from about 2 MiB per record to about 1 KiB per record.
  - Also drop the redundant reads (MFT-14, MFT-15; see T6.9).
- **Fix, stage B (after measuring stage A on a 1M+ record volume):** replace the `ntfs` crate on the full-walk path with a small raw reader:
  - Resolve the `$MFT` runlist once, including extents listed in record 0's `$ATTRIBUTE_LIST`. This lifts the T6.4 refusal.
  - Read `$MFT` sequentially in large aligned runs.
  - Apply the fixup per record and walk attributes in the buffer.
  - Merge extension records into their base record by base reference while streaming, so no attribute-list random reads are needed.
  - Keep the `ntfs` crate only for `apply_delta`'s random re-reads, or reuse the same parser with `record_offset()`.
- **Rejected alternatives:**
  - Use `NtfsFile` with our own buffer: `NtfsFile::new` is `pub(crate)` in 0.4.0, so it is impossible without forking.
  - Upgrade the crate: there is no newer release that lifts the attribute-list assumption **(unconfirmed)**, and the pin is deliberate.
  - `FSCTL_ENUM_USN_DATA`: coherent and fast, but it returns no size or timestamps, so it cannot replace record reads.
- **Boilerplate, stage A:**
```rust
// crates/backend-windows/src/sector.rs
/// One cached aligned window of the source.
struct Slot {
    buf: Vec<u8>,
    start: u64,
    end: u64, // == start when empty
}

impl Slot {
    fn contains(&self, pos: u64) -> bool { (self.start..self.end).contains(&pos) }
}

pub struct SectorReader<R> {
    inner: R,
    sector: u64,
    chunk: u64,
    len: u64,
    pos: u64,
    /// Streaming window for sequential record reads.
    stream: Slot,
    /// Window pinned to one hot offset (the `$MFT` record 0 that ntfs::Ntfs::file re-reads).
    pinned: Slot,
    pin_at: Option<u64>,
}

impl<R: Read + Seek> SectorReader<R> {
    /// Keeps the chunk that holds `offset` in its own slot, so reads of it never evict the
    /// streaming window.
    pub fn pin(&mut self, offset: u64) { self.pin_at = Some(offset - offset % self.chunk); }

    pub fn invalidate(&mut self) {
        self.stream.end = self.stream.start;
        self.pinned.end = self.pinned.start;
    }

    fn slot_for(&mut self, pos: u64) -> io::Result<&Slot> {
        let start = pos - pos % self.chunk;
        let use_pinned = self.pin_at == Some(start);
        let hit = if use_pinned { self.pinned.contains(pos) } else { self.stream.contains(pos) };
        if !hit {
            // fill_slot = today's `fill` body, writing into the chosen Slot.
            let (inner, sector, chunk, len) = (&mut self.inner, self.sector, self.chunk, self.len);
            let slot = if use_pinned { &mut self.pinned } else { &mut self.stream };
            fill_slot(inner, slot, sector, chunk, len, pos)?;
        }
        Ok(if use_pinned { &self.pinned } else { &self.stream })
    }
}
// Read::read: `let slot = self.slot_for(self.pos)?;` then copy as today.
```
```rust
// crates/backend-windows/src/mft_walk.rs
const READ_CHUNK_BYTES: u64 = 4 << 20;

// NtfsRecordSource::open, after Ntfs::new:
if let Some(mft) = ntfs.mft_position().value() {
    fs.pin(mft.get());
}
```
- **Boilerplate, stage B (skeleton with the load-bearing parsing):**
```rust
// crates/backend-windows/src/raw_mft.rs  (portable; no unsafe, no cfg)
/// One `$MFT` extent: `clusters` clusters starting at `lcn`, covering VCNs from `vcn`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Extent { pub vcn: u64, pub lcn: u64, pub clusters: u64 }

/// Decodes an NTFS mapping-pairs array. `None` on any malformed or sparse run (the
/// `$MFT` is never sparse).
pub fn decode_runlist(mut bytes: &[u8], start_vcn: u64) -> Option<Vec<Extent>> {
    let mut out = Vec::new();
    let (mut vcn, mut lcn) = (start_vcn, 0i64);
    loop {
        let (&header, rest) = bytes.split_first()?;
        if header == 0 { return Some(out); }
        let (len_size, off_size) = (usize::from(header & 0x0F), usize::from(header >> 4));
        if len_size == 0 || len_size > 8 || off_size == 0 || off_size > 8 { return None; }
        let clusters = le_uint(rest.get(..len_size)?);
        let delta = le_int(rest.get(len_size..len_size + off_size)?);
        lcn = lcn.checked_add(delta)?;
        out.push(Extent { vcn, lcn: u64::try_from(lcn).ok()?, clusters });
        vcn = vcn.checked_add(clusters)?;
        bytes = rest.get(len_size + off_size..)?;
    }
}

fn le_uint(b: &[u8]) -> u64 { b.iter().rev().fold(0, |acc, &x| (acc << 8) | u64::from(x)) }
fn le_int(b: &[u8]) -> i64 {
    let unsigned = le_uint(b);
    let shift = 64 - 8 * b.len() as u32; // b.len() in 1..=8, checked by the caller
    ((unsigned << shift) as i64) >> shift // sign-extend
}

/// Applies the update sequence array in place. NTFS uses a 512-byte stride regardless of the
/// physical sector size. Returns `Err` on a torn record (a sector tail that is not the USN).
pub fn apply_fixup(rec: &mut [u8]) -> Result<(), &'static str> {
    const STRIDE: usize = 512;
    let usa_ofs = usize::from(u16_at(rec, 0x04).ok_or("short header")?);
    let usa_cnt = usize::from(u16_at(rec, 0x06).ok_or("short header")?);
    if usa_cnt == 0 || (usa_cnt - 1) * STRIDE > rec.len() { return Err("bad USA count"); }
    let usn = u16_at(rec, usa_ofs).ok_or("USA out of bounds")?;
    for i in 1..usa_cnt {
        let tail = i * STRIDE - 2;
        if u16_at(rec, tail) != Some(usn) { return Err("fixup mismatch (torn record)"); }
        let fix = u16_at(rec, usa_ofs + 2 * i).ok_or("USA out of bounds")?;
        rec.get_mut(tail..tail + 2).ok_or("USA out of bounds")?.copy_from_slice(&fix.to_le_bytes());
    }
    Ok(())
}

/// Facts from one fixed-up record. `base == 0` for a base record. Attributes found in an
/// extension record are merged into the base by the caller.
pub struct RawRecord { pub base: u64, pub sequence: u16, pub flags: u16, pub attrs: Vec<RawAttr> }
pub enum RawAttr {
    StdInfo { attrs: u32, ctime: u64, mtime: u64 },          // 0x10: +0x00, +0x08, +0x20
    FileName { parent_ref: u64, namespace: u8, name: Vec<u16> }, // 0x30: +0x00, +0x41, +0x42
    UnnamedData { size: Option<u64> },                       // 0x80: resident len, or non-resident +0x30 when lowest_vcn == 0
    ReparseTag(Option<u32>),                                 // 0xC0: first u32 when resident
    AttributeList,                                           // 0x20
}
pub fn parse_record(rec: &[u8]) -> Result<Option<RawRecord>, &'static str> { todo!("walk attrs from +0x14 until 0xFFFF_FFFF; bounds-check every length") }

/// Streams `$MFT` slots in record order using large reads over the extents.
pub struct BulkMftReader<R> { /* inner: SectorReader<R>, extents, record_size, cluster, buf */ }
impl<R: Read + Seek> BulkMftReader<R> {
    /// Byte offset on the volume of record `n`, from the resolved extents (for delta re-reads).
    pub fn record_offset(&self, n: u64) -> Option<u64> { todo!() }
}
```
  Merge rule for stage B, at the call site in `read_tree`:
  - Keep a `HashMap<u64 /*base record*/, Vec<RawAttr>>` for extension records that arrive before their base, and attach them when the base is seen. In practice, extension records usually have higher numbers than their base, so the map stays small.
  - Size comes only from the unnamed `$DATA` instance with `lowest_vcn == 0`.
- **Tests to add:**
  - Stage A: a `SectorReader` test with a counting fake source. Alternating reads of offset 0 and offsets beyond 4 MiB issue one fill for the pinned slot, plus one per 4 MiB of streamed data.
  - Stage B:
    - `decode_runlist` vectors: a negative delta, 8-byte fields, a terminator, a sparse run giving `None`.
    - `apply_fixup` with a good record, a torn record, and a bad USA count.
    - A `parse_record` golden test against the existing ntfs-crate path on the same synthetic image, with `RecordFacts` equal for every record.
  - Benchmark: stage A against stage B on a 1M-record image (user-run, deferred).
- **Risk / follow-ups:**
  - Stage B is the largest item in T6, at roughly 600-900 lines with tests. Gate it on a measured need.
  - Stage B also removes the `ntfs` dependency from the hot path, which retires MFT-14 and MFT-15 automatically.

### T6.6 Check the parent sequence number (closes MFT-07)

- **Verification:** MFT-07 Confirmed. `records.rs` `file_names` stores `parent_directory_reference().file_record_number()` only. `tree.rs` matches `NameFact.parent` against record numbers in `index_dir_children` and in the file phase.
- **Fix:**
  - Store the full 64-bit reference in `NameFact.parent`, and match on record plus sequence. A sequence of 0 is treated as "unchecked" **(unconfirmed whether NTFS ever writes 0 here; a wildcard is the safe reading)**.
  - Key `index_dir_children` by the full reference, and store `sequence` in `DirNode`.
  - The cache already writes `parent` as `u64`, so only the meaning changes. Bump the cache `FORMAT_VERSION` to 2, so old files are discarded with `BadVersion` and a full read follows. The DB schema is not affected.
- **Rejected alternative:** a separate `parent_seq: u16` field. It is a bigger diff in the cache codec for no gain.
- **Boilerplate:**
```rust
// crates/backend-windows/src/records.rs  (file_names)
let parent = name.parent_directory_reference();
names.push(NameFact {
    // Full reference: record number in the low 48 bits, sequence above.
    parent: parent.file_record_number() | (u64::from(parent.sequence_number()) << 48),
    // ...
});
```
```rust
// crates/backend-windows/src/tree.rs
pub struct NameFact {
    /// File reference of the directory that holds this name (record | sequence << 48).
    pub parent: u64,
    // ...
}

fn parent_record(r: u64) -> u64 { r & RECORD_NUMBER_MASK }
fn parent_seq(r: u64) -> u16 { (r >> 48) as u16 }

/// A name's parent matches the stored directory only when the sequence agrees, so a name
/// whose parent record was freed and reused becomes an orphan instead of moving.
fn parent_matches(name_parent: u64, dir_sequence: u16) -> bool {
    let seq = parent_seq(name_parent);
    seq == 0 || seq == dir_sequence
}

struct DirNode { path: RawPath, depth: usize, sequence: u16 }

// index_dir_children: key by parent_record(name.parent), push (record, name, name.parent).
// reach_directories: when expanding `dir`, skip entries where
//     !parent_matches(entry_parent_ref, dirs[&dir].sequence)
// and record the child's own sequence (records[&child].sequence) in its DirNode.
// emit_items file phase:
let Some(parent) = dirs
    .get(&parent_record(name.parent))
    .filter(|d| d.depth < MAX_DEPTH && parent_matches(name.parent, d.sequence))
else { continue };
```
```rust
// crates/backend-windows/src/mft_cache.rs
const FORMAT_VERSION: u32 = 2; // v2: NameFact.parent is a full file reference
```
- **Tests to add:**
  - A tree with a directory record whose sequence is 7 and a file name pointing at it with sequence 6: the file is not emitted.
  - The same case with sequence 0 in the reference: the file is emitted.
  - A cache v1 file is rejected with `BadVersion`.
- **Risk / follow-ups:** An orphan is still silently skipped, but that is a file in flux during a live read; it is not data loss from a healthy volume. Add an `orphans` counter to `MftStats` for debugging.

### T6.7 Cache trust: post-read delta, dirty bit, sampled verification (closes WIO-03, MFT-09)

- **Verification:**
  - WIO-03 Partially confirmed. The mechanism is in the code:
    - `open_raw_volume` opens with flags 0.
    - `cache_site` queries `next_usn` before the read.
    - `store_cache` stamps with that value.
    - Changes during the read are replayed next time. Changes made before the stamp but not yet flushed to disk are not.
    - Whether DASD reads are incoherent with NTFS's dirty `$MFT` pages is **(unconfirmed)**. Forcing a flush with `FlushFileBuffers` on a volume needs `GENERIC_WRITE` on the volume handle **(per docs as I recall)**, which a read-only tool should not take.
  - MFT-09 Confirmed in code: `validate` checks serial, geometry, journal id and USN range only.
    - That ntfs-3g and the Linux ntfs3 driver do not maintain `$UsnJrnl` is **(unconfirmed, believed true)**.
- **Fix:**
  1. **Post-read delta (WIO-03).** After a full read, query the journal again (`after`) and apply `[stamp_before, after)` to the fresh tree with `apply_delta`. Records changed during the read are then re-read after their change was journaled. Stamp the cache with `stamp_before`, not `after`: a later scan replays a few entries twice, which is idempotent.
  2. **Dirty bit (MFT-09).** Run `FSCTL_IS_VOLUME_DIRTY` on the raw volume. If the volume is dirty, discard the cache and do not write one.
  3. **Sampled verification (MFT-09).** After `refresh_cached`, re-read `SPOT_CHECK_RECORDS` records spread evenly across `$MFT` (plus the scan root record) and compare them with the refreshed tree. Any difference discards the cache and forces a full read.
     - This catches broad offline edits and clones. It cannot catch a single offline edit.
     - Document that limit for `--mft-cache`: after editing a drive on another OS, use the rebuild flag.
- **Rejected alternatives:**
  - `$LogFile` restart-area LSN comparison: offline writers' behaviour toward it is not documented **(unconfirmed)**.
  - Locking the volume: it blocks other processes.
  - Opening `GENERIC_WRITE` to flush: a write-capable handle in a read-only tool.
- **Boilerplate:**
```rust
// crates/backend-windows/src/enumerator.rs
const SPOT_CHECK_RECORDS: u64 = 4096;

// obtain_tree: Plan::Full branch, after read_tree succeeds and when a cache site exists:
fn catch_up_full_read<S: RecordSource>(
    tree: &mut VolumeTree,
    source: &mut S,
    volume: &RawVolume,
    stamp_before: &JournalInfo,
) -> std::result::Result<(), String> {
    let after = query_journal(volume).map_err(|e| e.to_string())?;
    if after.journal_id != stamp_before.journal_id {
        return Err("USN journal was recreated during the read".into());
    }
    let entries = read_journal(volume, after.journal_id, stamp_before.next_usn, after.next_usn)
        .map_err(|e| e.to_string())?;
    apply_delta(tree, source, &entries, usize::MAX).map(|_| ()).map_err(|e| e.to_string())
}
// On Err: emit the tree for this scan (it is as good as before), but do not store the cache.
// Note: NtfsRecordSource owns the RawVolume; give RecordSource a `volume(&self) -> &RawVolume`
// accessor (windows-only impl) or keep a second handle from T6.2's probe.

/// Re-reads evenly spaced records and compares them with the refreshed tree.
fn spot_check<S: RecordSource>(tree: &VolumeTree, source: &mut S, root: u64) -> std::result::Result<(), String> {
    let n = source.record_count();
    let step = (n / SPOT_CHECK_RECORDS).max(1);
    for number in (0..n).step_by(step as usize).chain(std::iter::once(root)) {
        let live = source.record(number).map_err(|e| e.to_string())?;
        if !tree.matches_live(number, live.as_ref()) {
            return Err(format!("record {number} differs from the cache"));
        }
    }
    Ok(())
}
```
```rust
// crates/backend-windows/src/tree.rs
impl VolumeTree {
    /// Whether `live` (after the same filtering `push` applies) equals the stored record.
    pub fn matches_live(&self, record: u64, live: Option<&RecordFacts>) -> bool {
        let filtered = live.and_then(|f| normalized(f)); // metafile/in_use/DOS-name filter of `push`, factored out
        self.records.get(&record) == filtered.as_ref()
    }
}
```
```rust
// crates/backend-windows/src/volume_io.rs  (win32)
use windows_sys::Win32::System::Ioctl::FSCTL_IS_VOLUME_DIRTY;
const VOLUME_IS_DIRTY: u32 = 0x0000_0001;
pub fn volume_is_dirty(vol: &RawVolume) -> io::Result<bool> {
    let mut out = [0u8; 4];
    let n = ioctl_output(&vol.handle, FSCTL_IS_VOLUME_DIRTY, &mut out)?;
    let flags = u32::from_le_bytes(out);
    Ok(n == 4 && flags & VOLUME_IS_DIRTY != 0)
}
// cache_site(): if volume_is_dirty(volume) == Ok(true) { notices.push("MFT cache is off: volume is marked dirty"); return None; }
```
- **Tests to add:**
  - Portable: `spot_check` with a FakeSource where one sampled record differs from the cached tree gives `Err`.
  - `catch_up` logic via `apply_delta` on a fake: an entry in `[before, after)` causes a re-read.
  - Windows `#[ignore]`: create files in a loop during the MFT read and assert the warm scan sees them.
- **Risk / follow-ups:**
  - The spot check costs about 4k random record reads per cached scan, which is cheap next to a full read.
  - Residual risk: a single offline edit, and dirty-page incoherence for changes before `stamp_before`. Document both in CLAUDE.md Gotchas.

### T6.8 std `OwnedHandle` and raw-pointer OVERLAPPED (closes WIO-07, WIO-06)

- **Verification:**
  - WIO-07 Confirmed.
    - `backend-generic/src/lib.rs` defines `OwnedHandle(HANDLE)` with `from_raw`/`raw`.
    - `handle.rs` has `unsafe impl Send/Sync for VolumeHint`, and `SyncFile::new` does `mem::forget` plus `File::from_raw_handle`.
    - `overlapped.rs` has `unsafe impl Send for OverlappedBackend`.
  - WIO-06 Confirmed as latent UB.
    - `wait_for(handle, &s.overlapped)` takes `&OVERLAPPED` while the kernel may write `Internal`/`InternalHigh`.
    - `submit` and `cancel_all` take `&mut Slot` over in-flight slots.
    - `Slot.buf: Box<[u8]>` is mutated by the kernel while a `&mut` path to it exists.
- **Fix:**
  - **WIO-07.** Make `win_sys::OwnedHandle` a re-export of `std::os::windows::io::OwnedHandle`. Build it with `HandleOrInvalid` (for `CreateFileW` and `OpenFileById`) or `HandleOrNull` (for `CreateEventW`). `File::from(owned)` replaces the forget handoff. `as_raw_handle()` replaces `.raw()`.
    - windows-sys 0.59 `HANDLE` is `*mut c_void`, matching std's `RawHandle`.
    - `VolumeHint` and `RawVolume` become `Send + Sync` automatically.
    - `OverlappedBackend` still needs `unsafe impl Send`, because of raw OVERLAPPED pointers after the WIO-06 change.
  - **WIO-06.** Allocate OVERLAPPED and buffers as raw heap memory owned by the slot (`Box::into_raw` and `NonNull`, freed in `Drop` after `cancel_all`), and hand only `*mut` or `*const` to the kernel. Never form `&`/`&mut` to them while `InFlight`. `data()` creates a shared slice only for `Idle` slots.
- **Rejected alternative:** `UnsafeCell` wrappers inside `Box`. They work, but a raw pointer plus manual drop says the ownership rule more directly, and it is the same amount of `unsafe`.
- **Boilerplate:**
```rust
// crates/backend-generic/src/lib.rs  (win_sys)
pub use std::os::windows::io::OwnedHandle;
use std::os::windows::io::{AsRawHandle, HandleOrInvalid, HandleOrNull, FromRawHandle};

pub fn open_handle(wide_path: &[u16], access: u32, share: u32, flags: u32) -> io::Result<OwnedHandle> {
    if wide_path.last() != Some(&0) { return Err(io::Error::from(io::ErrorKind::InvalidInput)); }
    // SAFETY: as today. CreateFileW returns INVALID_HANDLE_VALUE on failure, which is the
    // convention HandleOrInvalid encodes.
    let raw = unsafe { CreateFileW(wide_path.as_ptr(), access, share, std::ptr::null(), OPEN_EXISTING, flags, null_mut()) };
    // SAFETY: `raw` is either INVALID_HANDLE_VALUE or a new handle nothing else owns.
    let checked = unsafe { HandleOrInvalid::from_raw_handle(raw) };
    OwnedHandle::try_from(checked).map_err(|_| io::Error::last_os_error())
}
// Callers: `handle.raw()` -> `handle.as_raw_handle()`.
```
```rust
// crates/backend-windows/src/handle.rs
struct VolumeHint(OwnedHandle);          // no unsafe impls needed
struct SyncFile { file: File }
impl SyncFile { fn new(handle: OwnedHandle) -> Self { Self { file: File::from(handle) } } }
// open_by_id: wrap the OpenFileById result with HandleOrInvalid, as in open_handle.
// caps.rs is_elevated: OpenProcessToken writes a raw handle on success:
//   let token = unsafe { OwnedHandle::from_raw_handle(token) };
```
```rust
// crates/backend-windows/src/overlapped.rs
use std::ptr::NonNull;

struct Slot {
    /// Heap OVERLAPPED owned by this slot. Only raw pointers to it are formed while a read
    /// is pending, because the kernel writes it then.
    overlapped: NonNull<OVERLAPPED>,
    event: OwnedHandle,
    /// Heap buffer of `cap` bytes, same rule as `overlapped`.
    buf: NonNull<u8>,
    cap: usize,
    state: SlotState,
    valid: usize,
}

impl Slot {
    fn new(chunk: usize) -> io::Result<Self> {
        let event = create_event()?;
        let ov = Box::new(OVERLAPPED { /* zeros */ hEvent: event.as_raw_handle(), ..zeroed_overlapped() });
        let buf: Box<[u8]> = vec![0u8; chunk].into_boxed_slice();
        Ok(Self {
            overlapped: NonNull::from(Box::leak(ov)),
            buf: NonNull::from(Box::leak(buf)).cast(),
            cap: chunk,
            event, state: SlotState::Idle, valid: 0,
        })
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // SAFETY: OverlappedBackend::drop ran cancel_all first, so no read is pending and the
        // kernel holds neither pointer. Both came from Box::leak with these exact layouts.
        unsafe {
            drop(Box::from_raw(self.overlapped.as_ptr()));
            drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(self.buf.as_ptr(), self.cap)));
        }
    }
}

// submit: write offset/hEvent through `self.overlapped.as_ptr()` (raw writes, slot Idle),
//         ReadFile(handle, s.buf.as_ptr(), bytes, null_mut(), s.overlapped.as_ptr())
// complete / cancel_all: wait_for(handle, s.overlapped.as_ptr())
fn wait_for(handle: HANDLE, overlapped: *const OVERLAPPED) -> io::Result<usize> { /* same body */ }

fn data(&self, slot: usize) -> &[u8] {
    match self.slots.get(slot) {
        // SAFETY: an Idle slot has no pending read, so the kernel does not write the buffer;
        // `valid <= cap` is maintained by `complete`.
        Some(s) if matches!(s.state, SlotState::Idle) => unsafe {
            std::slice::from_raw_parts(s.buf.as_ptr(), s.valid.min(s.cap))
        },
        _ => &[],
    }
}
```
- **Tests to add:** The existing `tests/handle_reader.rs` covers behaviour. Add a portable `static_assertions`-style check that `VolumeHint: Send + Sync` without `unsafe impl`, using a const fn assertion. Miri cannot run FFI, so the review is the proof.
- **Risk / follow-ups:**
  - Field order matters. `Slot` must drop after `OverlappedBackend::drop` has run `cancel_all`; it does, because the struct drop runs before its fields.
  - Remove `win_sys::OwnedHandle::from_raw` and update `extents.rs`, `volume_io.rs` and `caps.rs` callers in the same change. Run the Windows cross-check clippy.

### T6.9 Medium and Low items

- **MFT-06 (Medium) Confirmed.** In `tree.rs`, `record_cost` uses 128 bytes plus 2 bytes per unit plus 40 bytes per name. `names` grows from `Vec::new()`, `retain` never shrinks it, and the emission maps are explicitly unbudgeted.
  - Fix:
    - In `records.rs`, build `names` with `Vec::with_capacity(count)` after counting `$FILE_NAME` attributes. Call `shrink_to_fit()` after the DOS `retain` in `push`.
    - Raise `RECORD_OVERHEAD_BYTES` to a measured value, around 192.
    - Add `DIR_PATH_OVERHEAD` times the directory count to a pre-emission check, so emission fails with `BudgetExceeded` (and falls back) before allocating paths.
    - Fix the "Records under one root" doc to "every record on the volume".
  - Stage B in T6.5 (`Vec<RecordFacts>` sorted, since records arrive in order) is the structural fix.
- **MFT-10 (Low) Confirmed.** The `%LOCALAPPDATA%` cache can be forged by an unelevated process of the same user.
  - Fix: when elevated, store under `%ProgramData%\dedupe\mft` with a DACL limited to Administrators and SYSTEM, and refuse to read a cache whose owner is not Administrators or SYSTEM. At minimum, document the trust model in the `--mft-cache` help.
- **MFT-11 (Medium) Confirmed.** In `mft_cache.rs`, `read` uses `fs::read` and then `decode` into a `Vec`, then `from_facts`.
  - Fix: `read_into_tree(path, root, budget) -> Result<(CachedIndex header, VolumeTree), CacheError>` streams through a `BufReader` wrapped in a hashing reader (mirroring `HashingWriter`), pushes each decoded record straight into `VolumeTree`, and checks the trailer at the end.
  - If the trailer fails, the tree is dropped. Budget errors surface early.
  - Keep `decode(&[u8])` for tests.
- **MFT-13 (Medium) Confirmed.** `tests/mft_vs_walk.rs` says the walker reports `None` and 1 for file id and nlink, which CLAUDE.md now contradicts. It uses `CacheConfig::default()` and `notices.drain().is_empty()`, and it has no reparse, long-path or many-link cases.
  - Fix:
    - Compare `file_id` and `nlink` and stop masking `HARDLINK`. A mismatch here is the CLAUDE.md open question, so make it a separate, clearly named assertion.
    - Assert `!notices.walker_used()`.
    - Expose `MftStats` (via `EnumNotices`, or a test hook) and assert `records_skipped_errors == 0`.
    - Add a second pass with `with_cache(CacheConfig { dir: Some(tempdir), rebuild: false })`: cold, mutate (rename, append, delete), then warm, comparing with the walker each time.
    - Add cases: a directory junction, a junction as scan root (T6.2), a 100-hardlink file (forces `$ATTRIBUTE_LIST`), ADS, compressed and sparse files (`fsutil sparse setflag`), a path over 260 characters, `fsutil file setCaseSensitiveInfo`, and a OneDrive folder when present (skip if absent).
- **MFT-14 (Low) Confirmed.** ntfs-0.4.0 `NtfsFileName::new` validates the namespace (`structured_values/file_name.rs:86-107`), so `file_name_namespace`'s extra read is redundant.
  - Fix: delete `file_name_namespace` and `FILE_NAME_NAMESPACE_OFFSET`. Map `name.namespace()` (crate enum) to `NameSpace` with an exhaustive `match`. Fix the module doc line.
- **MFT-15 (Low) Confirmed.** `RecordFacts.in_use` is always `true` from `extract_record`, and `base_reference_of` re-reads 8 bytes.
  - Fix: remove `in_use` from `RecordFacts`, the `push` filter and the cache flag. Fold this into the cache v2 bump of T6.6: drop `FLAG_IN_USE` and keep the bit reserved as must-be-zero.
  - With stage A the base-reference read is a cache hit. Stage B reads it from the buffer, so no separate change is needed now.
- **MFT-16 (Low) Confirmed.** `mft_cache.rs` `encode` (with `.expect`) and `write_atomic` are `pub` and used only by tests. The enumerator uses `write_atomic_stream` and `read`.
  - Fix: mark both `#[cfg(test)]`, or make `encode` return `Result<Vec<u8>, CacheError>` and `pub(crate)`.
- **MFT-17 / WIO-13 (Low) Confirmed** (`lib.rs:5`, same line, one fix).
  - Replace with: "Win32 calls are confined to `cfg(windows)` items in `caps`, `volume_io`, `handle`, `overlapped` and `usn` (its journal ioctls go through `RawVolume`). `enumerator` is Windows-only but makes no direct Win32 calls."
- **WIO-08 (Low) Confirmed.** `volume_io.rs` `RawVolume::read` uses `u32::try_from(chunk.len()).unwrap_or(u32::MAX)`.
  - Fix: `let want = u32::try_from(chunk.len()).unwrap_or(MAX_ALIGNED_READ);` with `const MAX_ALIGNED_READ: u32 = u32::MAX & !0xFFFF;` (a multiple of every sector size up to 64 KiB).
- **WIO-10 (Low) Confirmed.** `usn.rs` `read_journal` returns `Malformed` for both "stopped short" and the round cap.
  - Fix: add `UsnError::Incomplete { reached: i64, wanted: i64 }` ("USN journal ended at {reached} before {wanted}; records were lost") and `UsnError::TooManyRounds`. Return them at the two sites. Existing tests that expect `Malformed` for those paths must change; the tests for parse-level `Malformed` stay.
- **WIO-12 (Low) Confirmed.** `readahead.rs` `read_at` returns `Ok(0)` for `offset >= file_len` without I/O. `pass_verify.rs` probes at `size`.
  - Fix: in `ReadAhead::read_at`, when `offset >= self.file_len`, issue one real read through the backend (`submit` and `complete` on a free slot for `buf.len().min(chunk)` bytes at `offset`). If it returns bytes, update `file_len` to `offset + n` and return them.
  - Simpler alternative: re-query `GetFileSizeEx` via a backend `len()` method and return `Ok(0)` only if it is unchanged. The first is preferred because it matches the sync path exactly.
  - Test: a fake backend whose source grew after construction; `read_at(file_len)` returns 1.

---

Notes on unconfirmed items:
- `GetVolumePathNameW` behaviour on junctions.
- Whether `dwVolumeSerialNumber` equals the low 32 bits of the NTFS 64-bit serial.
- DASD read coherence with dirty `$MFT` pages.
- Whether offline Linux writers update `$UsnJrnl`.
- `FSCTL_IS_VOLUME_DIRTY` semantics after a foreign mount.
- The exact std `FileType` rule in 1.92.

Each needs a Windows hardware check, or a read of the 1.92 rust-src for the std rule.

---

## T7. CLI safety, path handling and output

**Findings covered:** CLI-01, CLI-02, CLI-03, CLI-04, PIPE-14, CLI-06, CLI-08, CLI-09, CLI-10, CLI-11, CLI-12, CLI-13, CLI-20, CLI-21, CLI-22, CLI-23, CLI-24, CORE-09, CORE-15

**Root causes:**
- One lossy rendering function (`RawPath::to_display`: `from_utf8_lossy` + NFC, no escaping) serves terminals, JSON and error text alike -> CLI-02, CLI-03.
- Paths compared or rebuilt as strings rather than in canonical/OS form -> CLI-04 = PIPE-14, CLI-08(b), CLI-10, CORE-09, CORE-15, CLI-21.
- "Is this a dedupe database?" is checked only by discovery, not by the commands that write or delete -> CLI-08(a), CLI-09, CLI-10.
- Panicking stdout/args APIs used in the CLI (`std::env::args`, `println!`) -> CLI-01, CLI-06.
- CLI arguments not validated before core uses them (roots on resume, default DB location, drive-relative paths, size parsing) -> CLI-11, CLI-12, CLI-20, CLI-21, CLI-22, CLI-24.

**Recommended order of work:**
1. T7.3 DB identity guard (CLI-09, CLI-08) - stops data loss and foreign-file writes.
2. T7.4 DB self-skip (CLI-04, PIPE-14).
3. T7.5 safe `report --output` (CLI-10).
4. T7.1 terminal escaping and T7.2 lossless JSON (CLI-02, CLI-03) together, since both replace `to_display`.
5. T7.6 stdout helper (CLI-06) and T7.7 `cli_args` (CLI-01).
6. T7.8 root validation and resume roots (CLI-11, CLI-12, CLI-21, CLI-24).
7. Low items: T7.9 to T7.14.

---

### T7.1 One terminal-safe display function (closes CLI-02, and the display half of CLI-03)
- **Verification:** CLI-02 Confirmed. `RawPath::to_display` (crates/core/src/path.rs:36-39) is `from_utf8_lossy` + `.nfc()` with no escaping. Raw output sinks: crates/core/src/report.rs:274 (`write_group`, used by `to_human` and `stream_human`), crates/cli/src/lookup.rs:93, crates/cli/src/db.rs:53 (`mount`). JSON (report.rs:86, lookup.rs:75) is escape-safe because serde_json escapes C0 controls, but see T7.2.
- **Fix:** split the one function into two purpose-named ones in `core/src/path.rs`:
  - `RawPath::to_terminal() -> Cow<str>`: for any human-facing output (stdout human report, lookup, db info, and stderr messages naming a scanned path). Walks `bytes.utf8_chunks()` (stable since 1.79, no unsafe, no cfg). Valid text is copied, except "dangerous" scalars which become `\u{XXXX}` (or `\t`, `\n`, `\r`, `\e`-style short forms for the common C0). Invalid bytes become `\xNN`. Escaped set: C0 (U+0000-001F), DEL (U+007F), C1 (U+0080-009F), bidi controls (U+061C, U+200E, U+200F, U+202A-202E, U+2066-2069), and line/paragraph separators (U+2028, U+2029). Returns `Cow::Borrowed` when nothing needs escaping (the common case, zero allocation for valid UTF-8 paths that contain nothing to escape).
  - Drop NFC entirely. NFC was the only normalization in the codebase (path.rs:8,38; `unicode-normalization` used nowhere else, core/Cargo.toml:18). Stored paths are already never normalized (codec is byte-exact). NFC in display hides the real difference between two members (`é` NFC vs NFD look identical), and every modern terminal composes NFD on its own. Remove the dependency.
  - Keep the `\\?\X:` strip for display, but only when it is safe (see T7.2 `simplify_verbatim`).
  - Backslash is NOT escaped: Windows paths are full of `\`, and the human output is not a machine contract (JSON is). Document that `a\x1b` as a literal name and an escaped ESC look the same in human output.
- **Comparison with other tools (partially confirmed from memory, not re-checked online):** GNU coreutils `ls` (since 8.25) defaults to `--quoting-style=shell-escape` on a TTY, printing `$'a\033b'`. That is copy-pasteable in bash but meaningless on Windows, so it is rejected here. My understanding is that ripgrep and fd print path bytes raw to the terminal (not certain; verify before quoting this in docs). Rust's own `Debug` for str/Path (`escape_debug`) escapes quotes and backslashes too, which would double every Windows separator, so it is rejected.
- **Rejected alternatives:** escape only when stdout is a TTY (a `-o report.txt` later `cat`ed is the same attack, so always escape human output); `{:?}` of the path (doubles `\`, adds quotes); shell-escape quoting (Unix-only).
- **Boilerplate:**
```rust
// crates/core/src/path.rs
use std::borrow::Cow;
use std::fmt::Write as _;

impl RawPath {
    /// Lossless where possible, terminal-safe always. For human output and messages only.
    /// Control characters, bidi overrides and invalid UTF-8 are shown as escapes, so a
    /// filename cannot move the cursor, rewrite earlier lines or reorder text.
    pub fn to_terminal(&self) -> Cow<'_, str> {
        escape_terminal(simplify_verbatim(&self.0))
    }
}

/// True for scalars that must never reach a terminal raw.
fn needs_escape(c: char) -> bool {
    matches!(c,
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'
        | '\u{061c}' | '\u{200e}' | '\u{200f}'
        | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
        | '\u{2028}' | '\u{2029}')
}

pub(crate) fn escape_terminal(bytes: &[u8]) -> Cow<'_, str> {
    // Fast path: valid UTF-8 with nothing to escape borrows.
    if let Ok(s) = std::str::from_utf8(bytes) {
        if !s.chars().any(needs_escape) {
            return Cow::Borrowed(s);
        }
    }
    let mut out = String::with_capacity(bytes.len() + 8);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            match c {
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                c if needs_escape(c) => {
                    let _ = write!(out, "\\u{{{:x}}}", c as u32); // writing to String cannot fail
                }
                c => out.push(c),
            }
        }
        for b in chunk.invalid() {
            let _ = write!(out, "\\x{b:02x}");
        }
    }
    Cow::Owned(out)
}
```
  Call sites: report.rs:274 `writeln!(out, "    {}", m.path.to_terminal())`; lookup.rs:93; db.rs:53; resume.rs:109 (`VolumeMismatch.mount`). Delete `to_display` once no caller remains (backend-windows test helpers at mft_walk.rs:431, mft_cache.rs:1124/1133, tree.rs:520/528 can switch to `String::from_utf8_lossy(path.as_bytes())`). CLI stderr messages that print user `Path`s (`using database: ...`, `create {}`) can use a small CLI helper `fn esc(p: &Path) -> String` that runs `escape_terminal` over `p.as_os_str().as_encoded_bytes()` (portable, stable, safe).
- **Tests to add:** unit tests on `escape_terminal`: ESC + CSI sequence -> `\u{1b}[2K`; `\n` in a name -> `\n` literal, single line; U+202E -> escaped; `b"a\xffb"` -> `a\xffb`; plain `C:\Users\me` borrowed and unchanged (assert `Cow::Borrowed`); NFD input is NOT recomposed (replaces `display_normalizes_nfc`). CLI integration (Unix, `#[cfg(unix)]`): create `a\x1b[2Kb` and a newline-named duplicate, run `report --format human`, assert stdout contains no `0x1b` byte and line count equals header + groups + members.
- **Risk / follow-ups:** human output for NFD names changes byte-wise (no longer NFC); acceptable since it is display only. Tracing logs (`tracing::warn!` with paths) are a separate sink; leave as follow-up.

### T7.2 Lossless JSON path fields (closes CLI-03)
- **Verification:** CLI-03 Confirmed. JSON `path` = `to_display()` at crates/core/src/report.rs:86 and crates/cli/src/lookup.rs:75, so it is NFC-normalized (path.rs:38) and U+FFFD-lossy. Core does NOT normalize stored paths; NFC is applied only in `to_display`. Two different non-UTF-8 names in one group collapse to the same string.
- **Fix:** each JSON member gets:
  - `"path"`: the exact string when the bytes are valid UTF-8 (no NFC, no escaping beyond JSON's own). When the bytes are not valid UTF-8 (Unix non-UTF-8, or Windows WTF-8 with a lone surrogate), this is the lossy form.
  - `"path_bytes"`: present ONLY when `path` is lossy. Lowercase hex of the raw bytes (Unix: native bytes; Windows: WTF-8, as documented on `RawPath`). Hex rather than base64 because no base64 crate is in the tree and hex is 6 lines; Python consumers do `os.fsdecode(bytes.fromhex(x))` on Unix.
  - Rationale for "only when lossy": the common case keeps today's shape, so `jq -r '.groups[].members[].path'` keeps working, and a consumer that cares checks `has("path_bytes")`.
  - JSON-in-strings cannot carry lone surrogates from a Rust `String`, so a WTF-8 escape inside `path` is not possible with serde_json; this is why a separate field is needed.
  - Verbatim prefix: JSON `path` uses `simplify_verbatim`, which strips `\\?\` from `\\?\C:\...` only when the result means the same file under Win32 normalization: total length under 260, no component ending in `.` or space, no reserved device name (`CON`, `NUL`, `COM1`...). Otherwise the prefix is kept. These are the rules the `dunce` crate applies (reference only; hand-rolled to avoid a dependency). Today the strip is unconditional, which is lossy for long paths and trailing-dot names.
  - Add a top-level `"format_version": 2` (or similar) so consumers can detect the change. Optional.
- **Rejected alternatives:** always emit base64/hex (noisy, breaks every existing consumer); `path_lossy: bool` alone (tells you it is wrong but not what is right); emit Python-style surrogateescape `\udcff` (serde_json cannot produce lone surrogates from `String`, and it is Python-specific).
- **Boilerplate:**
```rust
// crates/core/src/path.rs
impl RawPath {
    /// JSON fields for this path: exact `path`, plus `path_bytes` (hex) only when the
    /// bytes are not valid UTF-8 and `path` is therefore lossy.
    pub fn json_fields(&self, obj: &mut serde_json::Map<String, serde_json::Value>) {
        let bytes = simplify_verbatim(&self.0);
        match std::str::from_utf8(bytes) {
            Ok(s) => {
                obj.insert("path".into(), s.into());
            }
            Err(_) => {
                obj.insert("path".into(), String::from_utf8_lossy(bytes).into_owned().into());
                obj.insert("path_bytes".into(), to_hex(&self.0).into());
            }
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Strip `\\?\` before a drive letter only when the short form names the same file.
pub(crate) fn simplify_verbatim(bytes: &[u8]) -> &[u8] {
    let [b'\\', b'\\', b'?', b'\\', d, b':', b'\\', ..] = bytes else { return bytes };
    if !d.is_ascii_alphabetic() || bytes.len() - 4 >= 260 {
        return bytes;
    }
    let short = &bytes[4..];
    let safe = short[3..].split(|&b| b == b'\\').all(|comp| {
        !comp.ends_with(b".") && !comp.ends_with(b" ") && !is_reserved_dos_name(comp)
    });
    if safe { short } else { bytes }
}

fn is_reserved_dos_name(comp: &[u8]) -> bool {
    // Stem before the first '.', ASCII case-insensitive: CON PRN AUX NUL COM0-9 LPT0-9.
    todo!()
}
```
```rust
// crates/core/src/report.rs, to_json member mapping (before -> after)
// before: json!({ "volume_id": m.volume_id, "path": m.path.to_display() })
// after:
let mut o = serde_json::Map::new();
o.insert("volume_id".into(), m.volume_id.into());
m.path.json_fields(&mut o);
serde_json::Value::Object(o)
```
  Same change in crates/cli/src/lookup.rs:73-77.
- **Tests to add:** unit: NFD bytes round-trip unchanged in `path` with no `path_bytes`; `b"/x/\xff"` -> lossy `path` + `path_bytes == "2f782fff"`; `\\?\C:\a` -> `C:\a`; `\\?\C:\a.` and a 300-byte path keep the prefix; `\\?\UNC\srv\s\x` unchanged. Integration (`#[cfg(unix)]`, Linux only since macOS APFS rejects non-UTF-8): two duplicate files named `\xff` and `\xfe` -> JSON has two distinct `path_bytes`, and decoding them opens the real files.
- **Risk / follow-ups:** README JSON schema section must document `path_bytes`. Windows JSON paths for long or trailing-dot names now keep `\\?\` (correct, but visible).

### T7.3 Dedupe-database identity guard: application_id + refuse foreign files (closes CLI-09, CLI-08)
- **Verification:**
  - CLI-09 Confirmed. `Db::open` (crates/core/src/db/mod.rs:22-27) runs `apply_pragmas` first, which switches `journal_mode=WAL` (a persistent write to the file header), then `apply_migrations` (schema.rs:28-38) treats `user_version == 0` as fresh and runs the whole schema. Reached from `db vacuum` (cli/db.rs:16), `lookup --insert` (lookup.rs:29-32, and `insert_db_target` accepts any `--db` or any `--dir <file>`, main.rs:394-410), and `scan --db`.
  - CLI-08 Confirmed. `resolve_existing_db` checks only `path.exists()` for `--db` (main.rs:367-373) and accepts any file via `--dir <file>` (main.rs:376-378). `drop` builds siblings with `format!("{}{suffix}", db.display())` (db.rs:28), lossy for non-UTF-8; the non-existent lossy path is skipped by `p.exists()`, so it prints `dropped` and exits 0. `-journal` is not removed.
  - Side note (not in scope): `file_db_enables_wal` (db/mod.rs:84) asserts `current_version == 1` while `EXPECTED_VERSION` is 2; that test should fail today.
- **Fix:**
  1. Stamp every new DB with `PRAGMA application_id = 0x44445550` ("DDUP" in ASCII). application_id lives in the SQLite header (offset 68) and is the documented way to mark a file format. No table change, so no schema bump: keep v2. Existing v2 DBs (application_id 0) are accepted when `user_version == 2` and a `meta` table with `hash_algo` exists, and get stamped on their next RW open.
  2. New core function `db::classify(conn) -> DbKind { Empty, Dedupe, Foreign }` run BEFORE any pragma that writes. Empty = `application_id == 0 && user_version == 0 && sqlite_schema has 0 rows`. Dedupe = `application_id == APP_ID`, or legacy v2 as above. Anything else is Foreign. A non-SQLite file errors with `SQLITE_NOTADB` on the first query, mapped to Foreign as well.
  3. `Db::open` order becomes: open, `busy_timeout`, `classify` (refuse Foreign with `CoreError::NotDedupeDb { path }`), `apply_pragmas`, `apply_migrations` (which now also sets application_id on creation).
  4. `db drop` (and `vacuum`) call a CLI check `discover::is_dedupe_file(path)` that accepts any dedupe DB by application_id or `meta.hash_algo`, regardless of `user_version`, because the SchemaVersion error tells users to "delete it and scan again", so `drop` must still work on a v1 DB. Refuse with "not a dedupe database: <path>" otherwise. `--dir <file>` goes through the same check.
  5. Build siblings with `OsString::push`, include `-journal`, and print `dropped` only after the main file is actually gone.
- **Rejected alternatives:** checking only `user_version` (most foreign files are 0, which is the bug); checking file name `.dedupe.db` (explicit `--db` names are legitimate); a magic row in `meta` alone (cannot be read before deciding whether to create tables without first checking `sqlite_schema` anyway, and application_id is cheaper and standard).
- **Boilerplate:**
```rust
// crates/core/src/db/schema.rs
/// "DDUP". Marks the file as a dedupe database in the SQLite header.
pub const APPLICATION_ID: i64 = 0x4444_5550;

#[derive(Debug, PartialEq, Eq)]
pub enum DbKind { Empty, Dedupe, Foreign }

/// Read-only inspection; issues no writes, so it is safe before any pragma.
pub fn classify(conn: &Connection) -> Result<DbKind, CoreError> {
    let probe = || -> rusqlite::Result<(i64, i64, i64, bool)> {
        let app: i64 = conn.query_row("PRAGMA application_id", [], |r| r.get(0))?;
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let objs: i64 = conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))?;
        let meta: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='meta')",
            [], |r| r.get(0))?;
        Ok((app, ver, objs, meta))
    };
    let (app, ver, objs, meta) = match probe() {
        Ok(v) => v,
        Err(rusqlite::Error::SqliteFailure(e, _))
            if e.code == rusqlite::ErrorCode::NotADatabase => return Ok(DbKind::Foreign),
        Err(e) => return Err(e.into()),
    };
    Ok(match (app, ver, objs, meta) {
        (0, 0, 0, _) => DbKind::Empty,
        (APPLICATION_ID, _, _, _) => DbKind::Dedupe,
        (0, v, _, true) if v == EXPECTED_VERSION => DbKind::Dedupe, // legacy v2, stamped on open
        _ => DbKind::Foreign,
    })
}

// apply_migrations, fresh branch: also stamp the header.
//     tx.pragma_update(None, "application_id", APPLICATION_ID)?;
// and for a legacy v2 (app id 0) RW open: conn.pragma_update(None, "application_id", APPLICATION_ID)?;
```
```rust
// crates/core/src/db/mod.rs
pub fn open(path: &Path, cfg: &Config) -> Result<Self, CoreError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    if schema::classify(&conn)? == schema::DbKind::Foreign {
        return Err(CoreError::NotDedupeDb { path: path.display().to_string() });
    }
    apply_pragmas(&conn, cfg)?;
    schema::apply_migrations(&conn)?;
    Ok(Self { conn })
}

// crates/core/src/error.rs
#[error("{path} is not a dedupe database (refusing to modify it); pass a different --db")]
NotDedupeDb { path: String },
```
```rust
// crates/cli/src/db.rs, Drop arm
let db = crate::resolve_existing_db(args.db, args.dir)?;
if !crate::discover::is_dedupe_file(&db) {
    bail!("not a dedupe database: {}; refusing to delete it", db.display());
}
for suffix in ["", "-wal", "-shm", "-journal"] {
    let mut os = db.as_os_str().to_os_string();
    os.push(suffix);
    let p = std::path::PathBuf::from(os);
    match fs::remove_file(&p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !suffix.is_empty() => {}
        Err(e) => return Err(e).with_context(|| format!("remove {}", p.display())),
    }
}

// crates/cli/src/discover.rs
/// Any dedupe DB, any schema version: header application_id, or a `meta` row `hash_algo`.
/// Opened read-only; never runs migrations. Used by `db drop` so old DBs can be removed.
pub fn is_dedupe_file(path: &Path) -> bool { todo!("open RO without check_version, query as above") }
```
  Note `Db::open_readonly` calls `check_version`, so `is_dedupe_file` opens a plain `Connection::open_with_flags(READ_ONLY)` itself.
- **Tests to add:** core: a file with a foreign table (`CREATE TABLE files(x)`) -> `Db::open` returns `NotDedupeDb` and the file's `journal_mode` is still `delete` and its schema unchanged; a text file -> `NotDedupeDb`; a fresh path -> created with application_id set; a v2 DB with app id 0 + meta -> accepted and stamped. CLI: `db drop --dir some.txt --yes` fails and the file still exists; `db drop` on a v1-version dedupe DB succeeds; `lookup --insert --db foreign.sqlite` fails.
- **Risk / follow-ups:** README lines claiming "never deletes anything" should say "never deletes scanned files; `db drop` deletes only a dedupe database". Concurrent use of a DB being dropped is not checked (follow-up: try `BEGIN EXCLUSIVE` first).

### T7.4 DB self-skip by canonical path, plus default-name skip (closes CLI-04, PIPE-14)
- **Verification:** both Confirmed, same bug. crates/core/src/pipeline/pass_enumerate.rs:29-39 builds the skip set from `std::path::absolute(db)` (no symlink resolution, no `\\?\`, original case), while roots are `std::fs::canonicalize`d (crates/core/src/pipeline/scan.rs:46) and both the walker and the `$MFT` enumerator yield paths prefixed by that canonical root (backend-windows/src/enumerator.rs:191-193). `Path` equality is component-wise, and `Prefix::VerbatimDisk` vs `Prefix::Disk` compare unequal, so `C:\x\my.db` never matches `\\?\C:\x\my.db`. The default DB is built from the canonical first root (main.rs:318-323), so only `--db` is affected. `-journal` is not in the set. The skip set is also rebuilt per volume.
- **Fix:** build the skip set once in `run_scan`, right after `Db::open` (the DB file exists, and in WAL mode `-wal`/`-shm` exist while the writer connection is open):
  - canonicalize the DB file itself (gives true case and `\\?\` on Windows, resolves symlinks on macOS `/tmp`);
  - canonicalize each existing sibling (`-wal`, `-shm`, `-journal`) individually, because on case-insensitive volumes SQLite names the sibling from the string the user typed, which can differ in case from the canonical main file; fall back to `canonical_parent.join(canonical_name + suffix)` for siblings that do not exist yet;
  - store it in `Ctx` (`db_skip: &HashSet<PathBuf>`) instead of recomputing in `pass_enumerate`.
  - Separately, skip any file whose name is `.dedupe.db`, `.dedupe.db-wal`, `.dedupe.db-shm`, `.dedupe.db-journal` (move `DEFAULT_DB_NAME` from cli/main.rs:316 into core `config`). This closes the "old `.dedupe.db` in a subfolder shows up as a duplicate" note in CLI-04. This is a product choice; the alternative is to leave other indexes visible.
  - `std::fs::canonicalize` is portable std, so core keeps no `cfg` and no unsafe.
- **Rejected alternatives:** compare by `(volume_id, file_id)` (best in theory, but `FileMeta.file_id` formats differ between walker and `$MFT` and the Windows walker may leave it `None`, see CLAUDE.md gotchas; core cannot read a file id without a backend trait); case-insensitive string compare (needs knowing the volume's case rules, which core does not have); `same-file` crate per walked file (one extra open per file).
- **Boilerplate:**
```rust
// crates/core/src/pipeline/scan.rs, after `let db = db::Db::open(db_path, cfg)?;`
let db_skip = db_skip_set(db_path);

/// The DB and its sidecars in the same canonical form the walk yields, so a scan never
/// indexes its own database. Called after open, when the DB file exists.
fn db_skip_set(db: &Path) -> HashSet<PathBuf> {
    let canon_db = std::fs::canonicalize(db).unwrap_or_else(|_| db.to_path_buf());
    let mut set = HashSet::from([canon_db.clone()]);
    for suf in ["-wal", "-shm", "-journal"] {
        for base in [db, canon_db.as_path()] {
            let mut os = base.as_os_str().to_os_string();
            os.push(suf);
            let p = PathBuf::from(os);
            set.insert(std::fs::canonicalize(&p).unwrap_or(p));
        }
    }
    set
}

// crates/core/src/pipeline/mod.rs
pub struct Ctx<'a> {
    // ...existing fields
    pub db_skip: &'a HashSet<PathBuf>,
}

// crates/core/src/pipeline/pass_enumerate.rs, File arm (before -> after)
// before: if skip.as_ref().is_some_and(|s| s.contains(&native)) || cfg.exclude.is_excluded(&native)
// after:
if ctx.db_skip.contains(&native) || is_dedupe_db_name(&name) || cfg.exclude.is_excluded(&native) {
    return Ok(());
}

/// `.dedupe.db` and its SQLite sidecars, wherever they sit under a root.
fn is_dedupe_db_name(name: &RawPath) -> bool {
    let n = name.as_bytes();
    let base = crate::config::DEFAULT_DB_NAME.as_bytes();
    n.strip_prefix(base)
        .is_some_and(|rest| matches!(rest, b"" | b"-wal" | b"-shm" | b"-journal"))
}
```
  (`name` is already computed by `split_component` a line later; move that call up.)
- **Tests to add:** `#[cfg(unix)]` integration: root `r/`, symlink `link -> r`, `scan r --db link/idx.db` -> no row whose name starts with `idx.db` (query `files` by name). Same with `--db r/../r/idx.db`. A stale `.dedupe.db` copied into `r/sub/` and `r/sub2/` is not reported as a duplicate group. Windows (manual): `scan C:\data --db C:\DATA\x.db`.
- **Risk / follow-ups:** a hard link to the DB under another name is still indexed (would need file-id compare). The name-based skip hides a user file literally named `.dedupe.db` that is not an index (acceptable).

### T7.5 Safe `report --output`: same-file refusal + temp file and rename (closes CLI-10)
- **Verification:** Confirmed. crates/cli/src/report.rs:35-38 `File::create(path)` truncates with no comparison against the resolved DB (line 21). In JSON mode the file is created before `from_db_with` runs (line 41), so an error or Ctrl-C leaves a 0-byte file; in human mode a cancel leaves a partial file (lines 49-53).
- **Fix:** resolve the DB first, then refuse when the output is the DB or a sidecar, compared on canonical paths (canonicalize the output if it exists, else canonicalize its parent and join the file name). Write to a `tempfile::NamedTempFile::new_in(parent)` and `persist(path)` only on success. On error or cancel the temp file is dropped (deleted) and the previous output, if any, is untouched. `tempfile` is already a workspace dependency (Cargo.toml:46) and a cli dev-dependency; move it to `[dependencies]` in crates/cli/Cargo.toml (no new crate in the lockfile). `persist` replaces an existing file on both Unix (`rename`) and Windows (`MoveFileExW` with replace), as I understand tempfile 3.x; not re-verified in source.
- **Rejected alternatives:** hand-rolled `<name>.tmp` + `fs::rename` (needs its own cleanup guard and unique naming); `same-file` crate for identity (available transitively via walkdir, but canonical compare already handles the realistic slip and also works when the output does not exist yet).
- **Boilerplate:**
```rust
// crates/cli/src/report.rs
let db_path = crate::resolve_existing_db(args.db, args.dir)?;
if let Some(out) = &args.output {
    refuse_db_as_output(out, &db_path)?;
}
// ...
let (mut out, tmp): (Box<dyn Write>, Option<tempfile::NamedTempFile>) = match &args.output {
    Some(path) => {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("create temporary file in {}", dir.display()))?;
        (Box::new(BufWriter::new(tmp.reopen()?)), Some(tmp))
    }
    None => (Box::new(BufWriter::new(std::io::stdout().lock())), None),
};
// ... build and write as today; on any error `?` returns and `tmp` drops (deleted) ...
drop(out); // flush + close before rename (Windows cannot rename an open file)
if let (Some(tmp), Some(path)) = (tmp, &args.output) {
    tmp.persist(path).with_context(|| format!("write {}", path.display()))?;
}

/// Refuse `-o` pointing at the database or its sidecars.
fn refuse_db_as_output(out: &Path, db: &Path) -> anyhow::Result<()> {
    let canon = |p: &Path| -> Option<PathBuf> {
        std::fs::canonicalize(p).ok().or_else(|| {
            let parent = std::fs::canonicalize(p.parent()?).ok()?;
            Some(parent.join(p.file_name()?))
        })
    };
    let Some(out_c) = canon(out) else { return Ok(()) };
    for suf in ["", "-wal", "-shm", "-journal"] {
        let mut os = db.as_os_str().to_os_string();
        os.push(suf);
        if canon(Path::new(&os)).as_deref() == Some(out_c.as_path()) {
            anyhow::bail!("--output {} is the database (or its {suf} file); choose another file", out.display());
        }
    }
    Ok(())
}
```
  Human mode on cancel: keep printing "output incomplete (cancelled)" for stdout; with `-o`, print "cancelled; {path} not written" and drop the temp.
- **Tests to add:** `report -o .dedupe.db` fails and the DB still opens and has the same groups; `report -o out.json` with a forced failure (DB with wrong version via `--db`) leaves no `out.json`, and an existing `out.json` is unchanged; success path writes the full JSON.
- **Risk / follow-ups:** the temp file inherits default permissions, not those of an existing output file (minor).

### T7.6 One stdout writer that treats BrokenPipe as success (closes CLI-06)
- **Verification:** Confirmed. `println!`/`print!` at crates/cli/src/scan.rs:287-288 (`emit`), lookup.rs:50-51, db.rs:18,26,33,42-62. `report` uses `writeln!` (report.rs:42) but a broken pipe there becomes `Error: write report: Broken pipe` exit 1, also wrong.
- **Fix:** a CLI helper `out::print(s: &str) -> anyhow::Result<()>` that writes through `stdout().lock()`, flushes, and maps `ErrorKind::BrokenPipe` to `Ok(())`. Route every stdout write through it (`emit` returns `Result`; `db info` builds a `String` then prints once). For `report`, which streams through `Box<dyn Write>` and core's `stream_human` (errors come back as `CoreError::Io`), add one check in `main`: if any error in the chain is an `io::Error` of kind `BrokenPipe`, exit 0 silently. That second net also catches anything missed.
- **Rejected alternatives:** reset SIGPIPE to `SIG_DFL` via `libc::signal` (needs `unsafe` in the CLI, Unix-only, and kills the process without letting the scan writer finish); nightly `-Zon-broken-pipe=kill` (unstable).
- **Boilerplate:**
```rust
// crates/cli/src/out.rs (new, small module)
use std::io::{ErrorKind, Write};

/// Write to stdout. A closed pipe (`| head`) is a normal end, not an error.
pub fn print(s: &str) -> anyhow::Result<()> {
    let mut lock = std::io::stdout().lock();
    match lock.write_all(s.as_bytes()).and_then(|()| lock.flush()) {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => Err(e.into()),
        _ => Ok(()),
    }
}

/// True when the failure is only a closed stdout pipe.
pub fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|c| {
        c.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == ErrorKind::BrokenPipe)
            || matches!(c.downcast_ref::<dedupe_core::CoreError>(),
                        Some(dedupe_core::CoreError::Io(e)) if e.kind() == ErrorKind::BrokenPipe)
    })
}

// crates/cli/src/main.rs
fn main() -> std::process::ExitCode {
    // ...
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if out::is_broken_pipe(&e) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("Error: {e:?}"); ExitCode::FAILURE }
    }
}
```
  (`CoreError::Io` uses `#[error("io: {0}")]` with `#[from]`, so `source()` already yields the io error and the plain downcast arm may suffice; the explicit match is a belt for that.) Switching `main` to `ExitCode` also gives T6/CLI-07 a place for exit 130 on cancel.
- **Tests to add:** `#[cfg(unix)]` integration: spawn `dedupe report --json` with stdout piped, close the read end immediately, assert exit status 0 and stderr has no "panicked". Same for `scan --no-db` on a small tree and `lookup`.
- **Risk / follow-ups:** `eprintln!` still panics if stderr is closed; rare, leave.

### T7.7 `cli_args` from `args_os`, quoted (closes CLI-01)
- **Verification:** Confirmed. crates/cli/src/scan.rs:166 `std::env::args().collect::<Vec<_>>().join(" ")` panics on a non-Unicode argument; join drops quoting. Also `bench/src/main.rs:51` and `xtask/src/main.rs:4` use `env::args()` (dev tools; low impact).
- **Fix:** build from `args_os`, lossy per argument, and quote with `{:?}` only when the argument has whitespace, quotes, a backslash on Unix, or was lossy. The value is informational (`scan_runs.cli_args`), so a lossy-but-readable record is enough.
- **Boilerplate:**
```rust
// crates/cli/src/scan.rs
fn recorded_args() -> String {
    std::env::args_os()
        .map(|a| match a.to_str() {
            Some(s) if !s.is_empty() && !s.contains(|c: char| c.is_whitespace() || c == '"' || c == '\'') => s.to_owned(),
            _ => format!("{:?}", a), // OsStr Debug escapes invalid bytes, never panics
        })
        .collect::<Vec<_>>()
        .join(" ")
}
// ScanOptions { cli_args: recorded_args(), .. }
```
- **Tests to add:** `#[cfg(target_os = "linux")]` integration: `scan $'/tmp/x/\xff-dir'` (via `OsStr::from_bytes`) exits 0 and `scan_runs.cli_args` contains `\xFF` escaped.
- **Risk:** none.

### T7.8 Validate roots up front; resume must match roots (closes CLI-11, CLI-12, CLI-24)
- **Verification:**
  - CLI-11 Confirmed. crates/core/src/pipeline/scan.rs:40-43: with `opts.resume`, `roots` are ignored and `plan_resume` loads roots from `run_roots` (resume.rs:101-125). Roots still drive the default DB and the storage probe (cli scan.rs:57-74).
  - CLI-12 Confirmed. `scan_default_db` (main.rs:318-323) joins `.dedupe.db` onto the canonical first root without checking it is a directory; later roots are canonicalized with a bare `?` (core scan.rs:46), so `CoreError::Io` names no path. The probe (scan.rs:71-74) runs before any root is validated.
  - CLI-24 Confirmed. discover.rs:24 suggests `dedupe scan <root>` from a folder that scan does not write to; discover.rs:40 `Io` omits the folder.
- **Fix:**
  - CLI: `validate_roots(&[PathBuf]) -> Result<Vec<PathBuf>>` runs first in `scan::run`: canonicalize each with `with_context("scan root {}")`, so a typo names itself. Pass the canonical list on (core canonicalizes again, harmless).
  - Default DB: if the first root is not a directory, bail "the first root is a file; pass --db <PATH> to choose where the database goes". On `Db::open` failure for a default path, add the hint "the first root may be read-only; pass --db <PATH>". Do the hint by context, not by probing writability (permission bits do not reflect ACLs or ro mounts).
  - Core: map the bare `?` at scan.rs:46 to `CoreError::NotFound(format!("scan root {}: {e}", root.display()))` (or a new `Io { path, source }` variant).
  - Resume: make roots optional when `--resume` is set (`#[arg(required_unless_present = "resume")]`); with no roots, `--db` is required (no first root to derive the default from). When roots are given, core compares the canonical raw roots with the plan's roots as sets and returns `CoreError::InvalidArg("--resume continues run N over <roots>; the given roots differ ...")` on mismatch, unless `--force`. Probe storage on the plan's first root in that case (the CLI can read roots via a new `queries::roots_for_latest_running_run` or simply probe after the mismatch check passes).
  - CLI-24: `NotFound` text becomes "no dedupe database in {dir}; run `dedupe report <root>` for the folder you scanned, or pass --db <root>/.dedupe.db". `Io` becomes `Io(PathBuf, io::Error)` with "list database folder {dir}: {err}".
- **Rejected alternatives:** warn-only on resume mismatch (the user explicitly believes B was scanned; a warning in a scrolled stderr is not enough); silently scan the new roots after resuming (changes resume semantics).
- **Boilerplate:**
```rust
// crates/cli/src/main.rs, ScanArgs
#[arg(required_unless_present = "resume")]
pub roots: Vec<PathBuf>,

// crates/cli/src/scan.rs, first lines of run()
let roots = validate_roots(&args.roots)?;
if args.resume && roots.is_empty() && args.db.is_none() {
    bail!("--resume without roots needs --db <PATH>");
}

fn validate_roots(roots: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    roots.iter().map(|r| {
        crate::check_not_drive_relative(r, "root")?;          // T7.10
        std::fs::canonicalize(r).with_context(|| format!("scan root {}", r.display()))
    }).collect()
}

// crates/core/src/pipeline/scan.rs, resume branch
let plan = resume::plan_resume(&db.conn, cfg, svc.volume, svc.codec)?;
if !roots.is_empty() && !cfg.force {
    let given: BTreeSet<RawPath> = roots.iter()
        .map(|r| std::fs::canonicalize(r).map(|c| svc.codec.path_to_raw(&c)))
        .collect::<std::io::Result<_>>()?;
    let planned: BTreeSet<RawPath> = plan.volumes.iter().flat_map(|v| v.roots.iter().cloned()).collect();
    if given != planned {
        return Err(CoreError::InvalidArg(format!(
            "--resume continues run {} over {}; it does not match the roots given (pass no roots, or --force)",
            plan.run_id,
            planned.iter().map(|r| r.to_terminal()).collect::<Vec<_>>().join(", "))));
    }
}
```
- **Tests to add:** scan A, kill via `--stop-after-pass 1` (test-hooks), then `scan B --db idx --resume` fails naming A; `scan A --db idx --resume` succeeds; `scan --db idx --resume` (no roots) succeeds. `scan photo.jpg` fails with the `--db` hint; `scan good typo` names `typo` and prints no storage note. `report /typo` names `/typo`.
- **Risk / follow-ups:** `--force` now has a second meaning (override root mismatch); document it in the flag help.

---

### Low-severity items

### T7.9 Progress `--progress always` claim (closes CLI-13)
- **Verification:** Partially confirmed. crates/cli/src/progress.rs:17-18 promises drawing "even if stderr is not a terminal", and both sinks use `ProgressDrawTarget::stderr()` (lines 44, 178). I could not open the vendored indicatif 0.17.11 source (outside the project; reading it needs user approval), so the claim that a `Term` target skips drawing when `!is_term()` is from memory, not verified. The test-gap half is not reproduced: `should_draw_truth_table` (progress.rs:210-224) already unit-tests the `json`/`Always` wiring directly.
- **Fix:** change the doc to "Draw even when output is not a TTY-detected default; has no effect when stderr is redirected to a file" or, if drawing to files is really wanted, use `ProgressDrawTarget::term_like(Box::new(console::Term::stderr()))` for `Always`. Note `ProgressDrawTarget::term(Term::stderr(), hz)` (the review's suggestion) likely hits the same `is_term` check; verify in `indicatif/src/draw_target.rs` before choosing. Recommended: fix the doc (cheapest, honest).
- **Tests:** none new beyond the existing truth table.

### T7.10 Drive-relative check: Windows only, every path argument (closes CLI-21)
- **Verification:** Confirmed. `is_drive_relative` (main.rs:328-336) is applied only in `folder_or_cwd` (main.rs:339-350), on every OS.
- **Fix:** `fn check_not_drive_relative(p: &Path, what: &str) -> anyhow::Result<()>` that returns `Ok(())` when `!cfg!(windows)`, otherwise bails with the existing message. Call it for scan roots, `--db` (all commands), `--dir`, `-o`, and `lookup <FILE>`. Use `p.as_os_str().as_encoded_bytes()` instead of `to_string_lossy` (also fixes the `&arg[..1]` slice on a lossy string). Not a clap `value_parser` (those take `&str`, which would reject non-UTF-8 paths).
- **Tests:** unit on Unix: `c:backup` passes; keep the pure `is_drive_relative` tests.

### T7.11 `--mem-budget` with `try_from` (closes CLI-22)
- **Verification:** Confirmed, crates/cli/src/scan.rs:126-129 `b as usize`.
- **Fix:** mirror `buf_size` (scan.rs:76-83):
```rust
mem_budget: args.mem_budget
    .map(|b| usize::try_from(b).with_context(|| format!("--mem-budget {b} does not fit in usize on this platform")))
    .transpose()?
    .unwrap_or(defaults.mem_budget),
```

### T7.12 Exact decimal size parsing (closes CLI-20)
- **Verification:** Confirmed. crates/cli/src/size.rs:24-29 multiplies an `f64` mantissa and floors; `1.5B` is accepted.
- **Fix:** split `number` at `.`; `int: u64`, `frac` digits (cap at 19 digits); `bytes = int.checked_mul(mult)? + frac.checked_mul(mult)? / 10^digits` with `u128` intermediates to avoid overflow; reject a fraction when `mult == 1`. Drops `U64_LIMIT`.
```rust
let (int, frac) = number.split_once('.').unwrap_or((number, ""));
if !frac.is_empty() && multiplier == 1 { return Err(format!("size '{s}': bytes cannot be fractional")); }
let int: u128 = if int.is_empty() { 0 } else { int.parse().map_err(|_| invalid_size(s))? };
let scale = 10u128.checked_pow(u32::try_from(frac.len()).map_err(|_| invalid_size(s))?).ok_or_else(|| invalid_size(s))?;
let frac_v: u128 = if frac.is_empty() { 0 } else { frac.parse().map_err(|_| invalid_size(s))? };
let bytes = int * u128::from(multiplier) + frac_v * u128::from(multiplier) / scale;
u64::try_from(bytes).map_err(|_| invalid_size(s))
```
- **Tests:** `1.001KB == 1001`; loop over all `0.001..=4.999` KB three-decimal inputs equals integer math; `1.5B` rejected; `18446744073709551616` rejected.

### T7.13 `BarSink` cleanup and comment (closes CLI-23)
- **Verification:** Confirmed. No `Drop` for `BarSink` (progress.rs:36-54), unlike `ReportBar` (198-203). `bar_sink` is built regardless of `db_path` (scan.rs:98-115), and on Windows its `MultiProgress` feeds `mft_observer`, so `--no-db` draws the `$MFT` bar, contrary to the comment at scan.rs:98.
- **Fix:** add `impl Drop for BarSink { fn drop(&mut self) { for (_, b) in self.bars.drain() { b.finish_and_clear(); } let _ = self.multi.clear(); } }`. Keep the `$MFT` bar for `--no-db` (useful, it is the slow part) and change the comment to "Phase bars only exist on the DB path; on Windows the `$MFT` read bar also draws for --no-db".

### T7.14 Exclude normalization (closes CORE-09)
- **Verification:** Confirmed. crates/core/src/exclude.rs:45-51 replaces `\` on every OS, uses `to_string_lossy`, strips only `//?/X:`, and gives no warning for relative patterns.
- **Fix:** rewrite separators only for Windows-form paths (reuse `crate::path::separator_for` on `path.as_os_str().as_encoded_bytes()`, portable and safe). For Unix-form paths pass `path` itself to `GlobSet::is_match` (globset matches Unix paths by bytes, so non-UTF-8 stays lossless; not re-verified in globset source). Map `//?/UNC/` to `//` and `//?/X:` to `X:`. In `ExcludeSet::new`, return warnings for patterns that cannot match an absolute path (no leading `/`, `*`, or `X:`), and print them in the CLI as `note: --exclude 'node_modules' matches nothing; did you mean '**/node_modules'?`.
```rust
fn normalize(path: &Path) -> Cow<'_, Path> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if crate::path::separator_for(bytes) != b'\\' {
        return Cow::Borrowed(path);
    }
    let text = path.to_string_lossy().replace('\\', "/");
    let text = if let Some(rest) = text.strip_prefix("//?/UNC/") { format!("//{rest}") }
        else if let Some(rest) = text.strip_prefix("//?/").filter(|r| r.as_bytes().get(1) == Some(&b':')) { rest.to_owned() }
        else { text };
    Cow::Owned(PathBuf::from(text))
}
```
- **Tests:** `/data/a\b.txt` not matched by `/data/a/**`; `\\?\UNC\srv\s\x` matched by `//srv/s/**`; relative pattern yields a warning.

### T7.15 `RawPath::parent` for UNC and verbatim roots (closes CORE-15)
- **Verification:** Confirmed latent. crates/core/src/path.rs:72-83 special-cases only `X:\`; `\\srv\share\a` walks to `\\srv` then `\`, `\\?\C:\a` to `\\?\C:`. `parent` and `component` have no production callers (only tests; `join_component` is used by backend-windows tree.rs).
- **Fix:** add `fn root_len(bytes) -> usize` recognizing `/`, `X:\`, `\\?\X:\`, `\\?\UNC\srv\share\`, `\\srv\share\` (and `\\?\Volume{...}\`); in `parent`, return `None` when the path is at or inside its root, and `Some(root)` (with its trailing separator) when the last separator falls inside the root. Alternative if still unused at implementation time: delete `parent` (YAGNI) and its tests.
```rust
pub fn parent(&self) -> Option<RawPath> {
    let sep = separator_for(&self.0);
    let root = root_len(&self.0);
    let end = self.0.iter().rposition(|&b| b != sep)? + 1;
    if end <= root { return None; }
    let i = self.0[..end].iter().rposition(|&b| b == sep)?;
    Some(RawPath(if i < root { self.0[..root].to_vec() } else if i == 0 { vec![sep] } else { self.0[..i].to_vec() }))
}
```
- **Tests:** `successors(parent)` chains for `\\srv\share\a\b` ending at `\\srv\share\`, `\\?\C:\a` ending at `\\?\C:\`, `\\?\UNC\srv\share\x` ending at `\\?\UNC\srv\share\`; existing Unix and `C:\` tests unchanged.

---

## T8. Performance and memory at scale (1M-10M files)

**Findings covered:** DB-09, DB-10, DB-11, PIPE-13, DB-12, PIPE-10, PIPE-12, GEN-06, GEN-08, GEN-10, WIO-04, GEN-15, GEN-22, PIPE-20

**Root causes:**
- SQL written without checking the planner: the recursive step of the stale-deletion CTE does not bind `folders.volume_id`, so the `UNIQUE(volume_id, parent_id, name)` autoindex cannot be used (DB-09). The lookup query does not bind `files.volume_id`, so the partial `idx_files_size(volume_id, size)` cannot be used (DB-10).
- Read-side helpers call `prepare`/`query_row` (rusqlite 0.32 `query_row` calls `prepare` internally, not the cache) and build SQL with `format!`. The statement cache is also at its default capacity of 16, which the writer alone almost fills (DB-11, PIPE-13).
- `pass_prefix` was written as one sequential loop that collects everything first: a hash-map index that copies partners on every insert, a budget that is checked only after the whole index is built, outcomes for every row, a cloned `large`, no worker pool, and a fresh `buf_size` buffer for every file (DB-12, PIPE-10, PIPE-12).
- Per-file OS calls that could be done once per directory or once per volume: the second open in the Windows walker, and `GetVolumePathNameW` plus a global mutex on every extent lookup and every open-by-id (GEN-06, GEN-10, WIO-04).
- Probe and extent edge cases: an anonymous `st_dev` (major 0) on btrfs, ZFS, bcachefs and overlay; FIEMAP flags that are ignored; `diskutil` run with no deadline (GEN-08, GEN-15, GEN-22). Progress takes a mutex once per row (PIPE-20).

**Recommended order of work:**
1. T8.1 DB-09 (no schema change, biggest worst case: O(F^2) inside one writer transaction).
2. T8.2 DB-11/PIPE-13 (mechanical, no behaviour change).
3. T8.3 DB-10 (the lookup rewrite needs no schema change; the covering index waits for the next schema bump).
4. T8.4 DB-12/PIPE-10 (prefix classification by sort, exact budget).
5. T8.5 PIPE-12 (shared pool and reusable buffers). Do it after T8.4, because both restructure `pass_prefix::run`.
6. T8.9 PIPE-20 (small; also do it before T8.5, so the pooled prefix path uses the batched advancer from the start).
7. T8.6 GEN-06, then T8.7 GEN-10/WIO-04 (Windows; run the Windows cross-check after each).
8. T8.8 GEN-08, then T8.10 GEN-22 (shares the timeout helper with the ZFS branch of GEN-08), then T8.11 GEN-15.

Note: several line numbers in the review no longer match the code (for example `pass_prefix.rs` is 282 lines and the review cites :367-426). The line numbers below are from the current tree.

---

### T8.1 Index-driven, materialized stale-subtree deletion (closes DB-09)
- **Verification:** DB-09 Confirmed. `db/writer.rs:373-405` (`Batch::DeleteStale`) and the duplicate `db/queries.rs:427-449` (`delete_stale_under_roots`, used only by a test at :1023) both recurse with `JOIN sub s ON f.parent_id = s.id`. In `schema/v2.sql` the only index containing `parent_id` is the autoindex of `UNIQUE(volume_id, parent_id, name)`. `volume_id` is unconstrained in the recursive step, and the DB never has `sqlite_stat1` (no `ANALYZE`), so skip-scan is not considered. Each dequeued folder therefore triggers `SCAN f` over all folders. The CTE is evaluated twice, and `UNION ALL` visits a subtree twice when the roots overlap (a root nested inside another root).
- **Fix:**
  1. Bind the volume in the recursive step and force the join order with `CROSS JOIN` (in SQLite, `CROSS JOIN` keeps the left table as the outer loop). The autoindex then gives `SEARCH f USING COVERING INDEX sqlite_autoindex_folders_1 (volume_id=? AND parent_id=?)`. It is covering because `id` is the rowid.
  2. Use `UNION` instead of `UNION ALL`, so overlapping roots and any accidental cycle are visited once.
  3. Materialize the subtree once into a TEMP table (`temp_store=MEMORY` is already set in `db/mod.rs:55`), then run both DELETEs against it.
  4. Keep one implementation in `queries.rs` and have the writer call it.

  No schema change. Cost becomes O(subtree x log F).
- **Rejected alternatives:** a new `CREATE INDEX folders(parent_id)` needs a schema bump and adds write cost for no gain over the existing autoindex. Running `ANALYZE` to enable skip-scan depends on statistics and is fragile.
- **Boilerplate:**
```rust
// crates/core/src/db/queries.rs  (replaces delete_stale_under_roots; writer calls this)
/// Deletes files, then folders, not seen by `run_id` under `root_folder_ids`.
/// Returns the number of deleted file rows. Must run inside the writer's transaction.
pub fn delete_stale_subtree(
    conn: &Connection,
    run_id: i64,
    volume_id: i64,
    root_folder_ids: &[i64],
) -> Result<u64> {
    if root_folder_ids.is_empty() {
        return Ok(0);
    }
    conn.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS stale_sub(id INTEGER PRIMARY KEY); \
         DELETE FROM temp.stale_sub;",
    )?;
    {
        let mut seed = conn.prepare_cached("INSERT OR IGNORE INTO temp.stale_sub(id) VALUES (?1)")?;
        for id in root_folder_ids {
            seed.execute([id])?;
        }
    }
    // Constant SQL, so it is cacheable. The roots are already in stale_sub.
    conn.prepare_cached(
        "WITH RECURSIVE sub(id) AS ( \
           SELECT id FROM temp.stale_sub \
           UNION \
           SELECT f.id FROM sub s CROSS JOIN folders f \
             ON f.volume_id = ?1 AND f.parent_id = s.id \
         ) \
         INSERT OR IGNORE INTO temp.stale_sub(id) SELECT id FROM sub",
    )?
    .execute([volume_id])?;
    let files = conn
        .prepare_cached(
            "DELETE FROM files WHERE volume_id=?1 AND last_seen_run_id < ?2 \
             AND folder_id IN (SELECT id FROM temp.stale_sub)",
        )?
        .execute(params![volume_id, run_id])?;
    conn.prepare_cached(
        "DELETE FROM folders WHERE volume_id=?1 AND last_seen_run_id IS NOT NULL \
         AND last_seen_run_id < ?2 AND id IN (SELECT id FROM temp.stale_sub)",
    )?
    .execute(params![volume_id, run_id])?;
    conn.execute("DELETE FROM temp.stale_sub", [])?;
    Ok(files as u64)
}

// crates/core/src/db/writer.rs, in apply(): Batch::DeleteStale arm becomes
Batch::DeleteStale { run_id, volume_id, root_folder_ids } => {
    crate::db::queries::delete_stale_subtree(tx, run_id, volume_id, &root_folder_ids)?;
}
```
  Expected `EXPLAIN QUERY PLAN` for the recursive INSERT. Before: `SCAN f` inside `RECURSIVE STEP`. After: `SEARCH f USING COVERING INDEX sqlite_autoindex_folders_1 (volume_id=? AND parent_id=?)`. The files DELETE should show `SEARCH files USING INDEX idx_files_folder (folder_id=?)` or `idx_files_seen`. Either is fine.
- **Tests to add:**
  - A queries.rs unit test with overlapping roots (a parent root and a nested root): stale rows are deleted once and fresh rows survive.
  - A test that rows of another volume with colliding `parent_id` values are untouched.
  - A test that runs `EXPLAIN QUERY PLAN` on the recursive statement and asserts no `SCAN f` in the detail column. This keeps the plan from regressing.
- **Risk / follow-ups:** `WITH ... INSERT` (CTE prefix on INSERT) is standard SQLite syntax. A large rescan still deletes in one writer transaction. To measure, add a bench section (do not run it now) that inserts about 200k folders in a deep tree and times `delete_stale_subtree`. The bench currently has no folder data.

### T8.2 Statement cache for hot read-side queries (closes DB-11, PIPE-13)
- **Verification:** DB-11 Confirmed and PIPE-13 Confirmed, same issue.
  - `queries.rs:159-172` `find_folder` uses `query_row`. In rusqlite 0.32.1 `query_row` calls `self.prepare`, so it does not use the cache.
  - `queries.rs:233-250` `find_by_path` uses `format!` plus `query_row`.
  - `queries.rs:252-267` `find_move_candidate` uses `format!` plus `prepare`.
  - These are called per directory and per file from `pass_enumerate.rs:189,217,241`.
  - Also: `STATEMENT_CACHE_DEFAULT_CAPACITY = 16` (rusqlite-0.32.1 `src/lib.rs:152`), and `writer.rs` already holds about 15 distinct `prepare_cached` statements, so its cache is at the LRU edge.
- **Fix:**
  1. Turn `FILE_COLS` into a macro, so the SQL can be a `concat!` constant (`format!` cannot be const).
  2. Use `prepare_cached` everywhere on the hot paths: `find_folder`, `find_by_path`, `find_move_candidate`, `rows_with_prefix`, `prefix_partners*`, and lookup's queries.
  3. Raise the cache capacity on both connections to 64.
- **Rejected alternatives:** `LazyLock<String>` works but is less direct than `concat!`. Holding `Statement`s in a struct on `Ctx` gives the same win with lifetime plumbing.
- **Boilerplate:**
```rust
// crates/core/src/db/queries.rs
macro_rules! file_cols {
    () => {
        "id,volume_id,folder_id,name,file_id,nlink,size,mtime_ns,ctime_ns,\
         attrs,prefix_hash,full_hash,prefix_bytes,state,err,scanned_at,last_seen_run_id"
    };
}
const FILE_COLS: &str = file_cols!(); // keep for the remaining format! users

pub fn find_folder(conn: &Connection, volume_id: i64, parent_id: i64, name: &RawPath)
    -> Result<Option<i64>>
{
    Ok(conn
        .prepare_cached("SELECT id FROM folders WHERE volume_id=?1 AND parent_id=?2 AND name=?3")?
        .query_row(params![volume_id, parent_id, name.as_bytes()], |r| r.get(0))
        .optional()?)
}

pub fn find_by_path(conn: &Connection, volume_id: i64, folder_id: i64, name: &RawPath)
    -> Result<Option<FileEntry>>
{
    const SQL: &str = concat!("SELECT ", file_cols!(),
        " FROM files WHERE volume_id=?1 AND folder_id=?2 AND name=?3");
    Ok(conn
        .prepare_cached(SQL)?
        .query_row(params![volume_id, folder_id, name.as_bytes()], row_to_file)
        .optional()?)
}

pub fn find_move_candidate(conn: &Connection, volume_id: i64, file_id: &[u8], run_id: i64)
    -> Result<Vec<FileEntry>>
{
    const SQL: &str = concat!("SELECT ", file_cols!(),
        " FROM files WHERE volume_id=?1 AND file_id=?2 AND last_seen_run_id<>?3");
    let mut stmt = conn.prepare_cached(SQL)?;
    let rows = stmt
        .query_map(params![volume_id, file_id, run_id], row_to_file)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

// crates/core/src/db/mod.rs
/// Enough for the writer's statements plus the read side's per-file queries.
pub const STATEMENT_CACHE_CAPACITY: usize = 64;
// in Db::open after apply_pragmas, and in Db::open_readonly after busy_timeout:
conn.set_prepared_statement_cache_capacity(STATEMENT_CACHE_CAPACITY);
```
- **Tests to add:** the existing phase1 rescan and move tests cover behaviour. Add a unit test that calls `find_by_path` twice on the same connection. For a hard check, call `conn.prepare_cached` in a loop and assert no new allocations; that is optional.
- **Risk / follow-ups:** none on correctness. To measure (do not run): a 1M-file cold `scan` of a generated tree, comparing the enumerate phase wall time before and after. The `scale` test (`DEDUPE_SCALE_N=1000000 ... --test scale -- --ignored`) reports end-to-end time. Expect a single-digit to low-double-digit percent gain on enumeration (an estimate, not measured).

### T8.3 Make the lookup query use idx_files_size; covering state index for the size pass (closes DB-10)
- **Verification:** DB-10 Partially confirmed.
  - Lookup (`lookup.rs:205-209`): `WHERE size=?1 AND state NOT IN (-1,-2)` has no `volume_id`. The partial index's WHERE term does appear verbatim, so the index is eligible, but its leading column `volume_id` is unconstrained. Without `sqlite_stat1` there is no skip-scan, so the plan is `SCAN files`. Confirmed.
  - Pass side (`queries.rs:303-315`): `state=0` does not imply `state NOT IN (-1,-2)` for SQLite's partial-index check (each term of the index WHERE must appear in the query WHERE). So `idx_files_size` is ineligible. Confirmed. However the query is not a full scan: `idx_files_state(volume_id, state)` serves `volume_id=? AND state=0`. The remaining costs are a table lookup per row for `last_seen_run_id`, `size` and `file_id`, plus a temp B-tree for `GROUP BY size`.
  - `idx_files_size` currently has no user at all (grep: nothing else filters on `size`).
- **Fix:**
  - (a) No schema change. Bind the leading column with an IN-subquery over the tiny `volumes` table. SQLite seeks the index once per volume.
  - (b) At the next schema bump (v3 under the existing "delete and rescan" policy; batch it with any other topic's schema change, do not bump for this alone), widen `idx_files_state` into a covering index. `size_candidates` then becomes an index-only scan whose GROUP BY is satisfied by index order. `rows_in_state` also benefits, because its three equality columns are all in the index.
```sql
-- (a) crates/core/src/lookup.rs size_candidates
SELECT id,volume_id,folder_id,name,size,mtime_ns,state,prefix_hash,full_hash
FROM files
WHERE volume_id IN (SELECT id FROM volumes) AND size=?1 AND state NOT IN (-1,-2);
-- expected plan: SEARCH files USING INDEX idx_files_size (volume_id=? AND size=?)
--                LIST SUBQUERY / SCAN volumes

-- (b) schema v3 (replaces idx_files_state; idx_files_size kept for lookup)
DROP INDEX IF EXISTS idx_files_state;  -- n/a in a fresh v3 schema; shown for clarity
CREATE INDEX idx_files_state ON files(volume_id, state, last_seen_run_id, size, file_id);
-- size_candidates plan: SEARCH files USING COVERING INDEX idx_files_state
--   (volume_id=? AND state=? AND last_seen_run_id=?)  -- no "USE TEMP B-TREE FOR GROUP BY"
```
```rust
// crates/core/src/lookup.rs
fn size_candidates(conn: &Connection, size: i64) -> Result<Vec<Candidate>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id,volume_id,folder_id,name,size,mtime_ns,state,prefix_hash,full_hash \
         FROM files WHERE volume_id IN (SELECT id FROM volumes) \
         AND size=?1 AND state NOT IN (-1,-2)",
    )?;
    // ... unchanged row mapping
}
```
- **Rejected alternatives:**
  - Changing the index to `files(size) WHERE state NOT IN (-1,-2)`: equally good for lookup but needs a schema bump; (a) gets the same plan today.
  - Adding a redundant `AND state NOT IN (-1,-2)` to the pass query: it makes the partial index eligible, but the plan would walk every live row of the volume in size order with table lookups. That is worse than `idx_files_state`.
- **How to measure (do not run):**
  - `bench` prints `lookup proxy (db-only) p50/p99` and `lookup real`. Change the proxy SQL in `bench/src/main.rs:136` to the rewritten WHERE clause, so it measures what lookup runs.
  - `size_candidates: ...` in the same output measures (b).
  - On a real DB: `sqlite3 .dedupe.db "EXPLAIN QUERY PLAN <query>"` and check for `SEARCH ... idx_files_size` instead of `SCAN files`.
- **Tests to add:** an EXPLAIN QUERY PLAN assertion test for both queries, which fails if the detail contains `SCAN files`. A lookup test with two indexed volumes: candidates from both volumes are still returned.
- **Risk / follow-ups:** (b) adds about 8-16 bytes per index entry (file_id blob), and state UPDATEs already rewrite this index entry. Consider `PRAGMA optimize` when the writer shuts down, so the planner has stats on large DBs. It is cheap, but optional.

### T8.4 Prefix classification by sort with an exact up-front budget (closes DB-12, PIPE-10)
- **Verification:**
  - DB-12 Confirmed: `pass_prefix.rs:48-51` builds the index from all existing prefixes, :94-96 inserts every `large` row, and the only `over_budget()` check is at :105, after everything is in memory. `index/memory.rs:12-14` `ENTRY_OVERHEAD = 48` ignores hashbrown's control bytes, its load factor (7/8) and growth headroom, and spilled SmallVec heap.
  - PIPE-10 Confirmed:
    - (a) `memory.rs:59-61` collects all partners into a new Vec on every insert, which the caller ignores, so n equal prefixes cost O(n^2) copies.
    - (b) Same as DB-12.
    - (c) `outcomes` (:57, :72) stores an outcome for every hashed row, with an `ErrRow` clone at :70, although only hardlink representatives are read (:74-90). `large.clone()` at :107 copies the vector.
- **Fix:** replace the hash-map index in `pass_prefix` with a sort.
  - `large` (`Vec<(i64, Hash)>`) must exist anyway to build the batches. Sorting it in place by hash costs no extra memory.
  - The existing committed prefixes of this run (state 2 or -4, from a resumed pass) are loaded into a `Vec<Hash>` and sorted. That is exactly 16 bytes each, and it is the only extra allocation.
  - A row is partnered when its hash run in `large` has length > 1, or when its hash is found in `existing` (binary search).
  - The budget check is exact and happens first: `COUNT(*)` the existing rows, and if `count * 16 > mem_budget`, go straight to the DB path without loading anything. The load also stops mid-way if the count was stale. This is the incremental fallback, at the only allocation that grows.
  - `outcomes` keeps only representatives, errors are moved rather than cloned, and the fallback sends `large` by move after extracting a sorted `Vec<i64>` of ids (8 bytes per row, not 24).
- **Rejected alternatives:**
  - Keeping `MemoryIndex` with a `try_add` that projects the next hashbrown resize: it works, but it still uses about 3x the bytes of the sort plus hashing work, and it needs heuristic accounting.
  - Estimating from `rows.len()` only: it is not exact, and it ignores the existing prefixes.
- **Boilerplate:**
```rust
// crates/core/src/pipeline/pass_prefix.rs
const HASH_BYTES: usize = std::mem::size_of::<Hash>(); // 16

/// Ids in `large` whose prefix has a partner: another large row or a committed
/// prefix of this run. Sorts `large` by hash in place. O(n log n), no extra
/// memory beyond `existing`.
fn split_partnered(large: &mut [(i64, Hash)], existing: &[Hash]) -> (Vec<(i64, Hash)>, Vec<(i64, Hash)>) {
    large.sort_unstable_by(|a, b| a.1.cmp(&b.1)); // needs Hash: Ord (derive it; it is [u8; 16])
    let (mut partnered, mut unique) = (Vec::new(), Vec::new());
    for run in large.chunk_by(|a, b| a.1 == b.1) { // slice::chunk_by is stable since 1.77
        let shared = run.len() > 1 || existing.binary_search(&run[0].1).is_ok();
        if shared { partnered.extend_from_slice(run) } else { unique.extend_from_slice(run) }
    }
    (partnered, unique)
}

/// Committed prefixes of this run, sorted, or `None` when they do not fit `budget`.
fn load_existing(ctx: &Ctx, volume_id: i64, budget: usize) -> Result<Option<Vec<Hash>>> {
    let n = queries::count_rows_with_prefix(ctx.read, volume_id, ctx.run_id)?; // new COUNT(*) twin of rows_with_prefix
    if n.saturating_mul(HASH_BYTES) > budget {
        return Ok(None);
    }
    let mut v = Vec::with_capacity(n);
    for (_, h) in queries::rows_with_prefix(ctx.read, volume_id, ctx.run_id)? {
        if (v.len() + 1) * HASH_BYTES > budget {
            return Ok(None); // rows appeared since the count: fall back mid-build
        }
        v.push(h);
    }
    v.sort_unstable();
    Ok(Some(v))
}

// in run():
let reps: HashSet<usize> = rep_of.values().copied().collect();
// in the hashing loop:
//   match outcome { Small/Large => push, Err(e) => { if reps.contains(&i) { errors.push(e.clone()) } else { errors.push(e); continue } } }
//   if reps.contains(&i) { outcomes.insert(i, outcome); }
// after the sibling loop:
let budget_left = cfg.mem_budget.saturating_sub(large.len() * std::mem::size_of::<(i64, Hash)>());
match load_existing(ctx, volume_id, budget_left)? {
    Some(existing) => {
        let (partnered, unique) = split_partnered(&mut large, &existing);
        drop(existing);
        // send small_batch, PrefixHashed(partnered), UniquePrefix(unique), errors, PhaseComplete
    }
    None => {
        let mut large_ids: Vec<i64> = large.iter().map(|(id, _)| *id).collect();
        large_ids.sort_unstable();
        ctx.writer.send(small_batch)?;
        ctx.writer.send(Batch::PrefixHashed(large))?; // moved, no clone
        // errors, PhaseComplete, flush, then:
        let singles = SqliteIndex::new(ctx.read, volume_id, ctx.run_id).prefix_singletons()?;
        let uniques: Vec<i64> = singles.into_iter()
            .filter(|id| large_ids.binary_search(id).is_ok()).collect();
        ctx.writer.send(Batch::UniquePrefixIds(uniques))?;
    }
}

// crates/core/src/db/writer.rs : new variant, mirrors UniqueSize
Batch::UniquePrefixIds(ids) => {
    let mut stmt = tx.prepare_cached("UPDATE files SET state=-4 WHERE id=?1 AND state=2")?;
    for id in ids { stmt.execute([id])?; }
}
```
  `MemoryIndex` then has no pipeline user. Either delete it together with the `Index` trait (preferred if no plan task still references `prefix_map`/`full_map` in 4.6; check `.overall_plan.md`), or at minimum make its insert O(1):
```rust
// crates/core/src/index/memory.rs
pub fn add(&mut self, key: Hash, id: i64) { self.ids += 1; self.map.entry(key).or_default().push(id); }
fn approx_bytes(&self) -> usize {
    let buckets = (self.map.capacity() * 8 / 7).next_power_of_two();
    buckets * (std::mem::size_of::<(Hash, SmallVec<[i64; 2]>)>() + 1) + self.spilled_ids * ID_BYTES
}
```
- **Tests to add:**
  - Unit tests for `split_partnered`: all-equal (200k ids, checks the quadratic is gone in time), all-distinct, a partner only in `existing`, and a mix.
  - `load_existing` returns `None` with `budget=0` while existing rows are present.
  - A phase1-style test with `mem_budget` = 1 byte: the DB fallback gives the same final states as the memory path on the same fixture. Equivalence is the key property.
- **Risk / follow-ups:**
  - Batch order changes (sorted by hash instead of row order). The writer UPDATEs by id, so state is unaffected. Any test that compares batch order must sort first.
  - Pre-existing inconsistency, not introduced here: the memory path counts committed -4 rows as partners (`rows_with_prefix` uses `state IN (2,-4)`), while `singleton_prefixes` (`queries.rs:368-378`) groups over `state=2` only. The two paths can classify a resumed run differently. Align the fallback with `state IN (2,-4)` in the GROUP BY and filter to state 2 in the outer query. Flag this for the state-machine topic, because a -4 row that gains a partner cannot move back under forward-only.
  - Whole-pass memory is still dominated by `rows: Vec<FileEntry>` (PIPE-04, another topic).
  - To measure (do not run): peak RSS via `/usr/bin/time -v dedupe scan <tree of 200k copies of one >20 KiB file>` before and after. A bench section could time `split_partnered` on 1M synthetic entries with one 200k group.

### T8.5 Prefix pass on the shared worker pool, with reusable buffers sized to the file (closes PIPE-12)
- **Verification:** PIPE-12 Confirmed.
  - `pass_prefix.rs:60-73` hashes sequentially and ignores `queue_depth`.
  - `hash.rs:143` `let mut buf = vec![0u8; chunk]` runs per call even for a 1 KiB file. `HDD_BUF_SIZE` = 1 MiB (`storage_profile.rs:16`), so every small file on HDD costs a 1 MiB zeroed allocation (mmap/munmap above glibc's threshold).
  - `pass_verify.rs:306-307` allocates two `chunk` buffers per pair, with the same issue.
  - `read_span` (`hash.rs`) allocates 16 KiB plus 4 KiB per large file.
- **Fix:**
  1. Move `Job`, `WorkerResult`, `hash_parallel`, `run_worker` and `hash_job` out of `pass_full.rs` into `pipeline/pool.rs` (crate-private functions, no new abstraction). Add `JobKind::{Full, Prefix}`, so the same workers hash small files whole and large files by prefix.
  2. Each worker owns one `Vec<u8>`, grown to `min(chunk, size)` and never shrunk. The helpers fill it via `read_full` into slices, so the bytes hashed are identical (BLAKE3 streaming over first then last equals hashing their concatenation).
  3. `pass_prefix` mirrors `pass_full`: plan on the main thread (reuse check, `rep_of` skip, `PathCache` paths), hash on workers, judge in row order with `meta_matches`.
  4. Process rows in windows of `POOL_WINDOW` (64k) so `Job` paths do not exist for all rows at once. This also bounds memory in `pass_full`. Hardlink representatives always come before their siblings (`hardlink_reps` picks the first index), so sibling resolution after all windows still works from the reps-only `outcomes` map.
  5. HDD keeps depth 1 (sequential path, extent order preserved).
- **Rejected alternatives:** rayon (a new dependency; the scoped-thread pool already exists and is tested in `core/tests/pool.rs`). A thread-local buffer (hidden state; an explicit `&mut Vec<u8>` is clearer).
- **Boilerplate:**
```rust
// crates/core/src/hash.rs
/// `buf` grows to `min(chunk, size)` and is reused across calls.
pub fn hash_full_target_buf(
    reader: &dyn ContentReader,
    target: ReadTarget<'_>,
    size: u64,
    chunk: usize,
    buf: &mut Vec<u8>,
) -> Result<Hash> {
    let chunk = chunk.max(1);
    let want_cap = usize::try_from(size).map_or(chunk, |s| s.min(chunk)).max(1);
    if buf.len() < want_cap {
        buf.resize(want_cap, 0);
    }
    let mut handle = reader.open_target(target)?;
    let mut h = blake3::Hasher::new();
    let mut offset = 0u64;
    while offset < size {
        let want = usize::try_from(size - offset).map_or(want_cap, |w| w.min(want_cap));
        let n = handle.read_at(offset, &mut buf[..want])?;
        if n == 0 { break; }
        h.update(&buf[..n]);
        offset += n as u64;
    }
    Ok(finish(h))
}

pub fn hash_full_target(reader: &dyn ContentReader, target: ReadTarget<'_>, size: u64, chunk: usize)
    -> Result<Hash>
{
    hash_full_target_buf(reader, target, size, chunk, &mut Vec::new()) // also fixes 1 MiB-per-1 KiB
}

/// Same bytes as `hash_prefix_target`, read through `buf` (no per-call Vecs).
pub fn hash_prefix_target_buf(reader: &dyn ContentReader, target: ReadTarget<'_>, size: u64,
    cfg: &Config, buf: &mut Vec<u8>) -> Result<(Hash, i64)> { /* read first into buf, h.update, read last, h.update */ todo!() }

// crates/core/src/pipeline/pool.rs
pub(crate) const POOL_WINDOW: usize = 65_536;
#[derive(Clone, Copy)]
pub(crate) enum JobKind { Full, Prefix }
pub(crate) struct Job { pub idx: usize, pub path: PathBuf, pub size: u64, pub file_id: Option<Vec<u8>>, pub kind: JobKind }
pub(crate) struct WorkerResult { pub hash: Result<Hash>, pub meta: Result<FileMeta> }

pub(crate) fn hash_parallel(ctx: &Ctx, volume_id: i64, jobs: Vec<Job>, rows_len: usize, depth: usize)
    -> Result<Vec<Option<WorkerResult>>> { /* moved from pass_full; captures `cfg: &Config` (Sync) */ todo!() }

fn run_worker(reader: &dyn ContentReader, cfg: &Config, cancel: &Token, jobs: &[Job],
    next: &AtomicUsize, abort: &AtomicBool, chunk: usize, tx: &mpsc::Sender<(usize, WorkerResult)>) {
    let mut buf = Vec::new(); // one per worker, reused for every job
    // ... loop as today, calling hash_job(reader, cfg, job, chunk, &mut buf)
}

fn hash_job(reader: &dyn ContentReader, cfg: &Config, job: &Job, chunk: usize, buf: &mut Vec<u8>) -> WorkerResult {
    let target = ReadTarget { path: &job.path, file_id: job.file_id.as_deref(), access: match job.kind {
        JobKind::Full => AccessPattern::Sequential, JobKind::Prefix => AccessPattern::Random } };
    let hash = match job.kind {
        JobKind::Full => hash_full_target_buf(reader, target, job.size, chunk, buf),
        JobKind::Prefix => hash_prefix_target_buf(reader, target, job.size, cfg, buf).map(|(h, _)| h),
    };
    WorkerResult { hash, meta: reader.metadata(&job.path) }
}
```
  In `pass_prefix::run`, `kind = if size <= cfg.prefix_total() { JobKind::Full } else { JobKind::Prefix }`. The judge maps `Full` to `HashOutcome::Small(h, h)` and `Prefix` to `Large(h)`. `pass_verify::verify_pair` gets the same `min(chunk, size)` sizing for `buf_a`/`buf_b`.
- **Tests to add:**
  - Extend `core/tests/pool.rs`: the prefix pass gives identical DB state at depth 1 and depth 4 (small, large, hardlinked and changed-during-read files).
  - `hash_prefix_target_buf == hash_prefix_target` and `hash_full_target_buf == hash_full_target` over a range of sizes, reusing one buffer across calls, including shrinking sizes. This catches stale-byte bugs.
- **Risk / follow-ups:**
  - The progress count must stay exactly `total` (planned rows advance at plan time, jobs when their result returns), as in `pass_full`.
  - Cancellation semantics are the same as `pass_full`.
  - To measure (do not run): `scale` test wall time at `DEDUPE_SCALE_N=1000000` on SSD with `--queue-depth 1` versus the default. On HDD, compare `scan --profile hdd` time and allocator page faults (`/usr/bin/time -v`, "Minor page faults").

### T8.6 Windows walker: one open per file, cached find data as fallback (closes GEN-06)
- **Verification:** GEN-06 Confirmed. `walk.rs:44-47` calls `path.symlink_metadata()`, which on Windows is `CreateFileW` plus `GetFileInformationByHandle` (std `lstat`). `meta.rs:15-26` then calls `fill_file_identity`, which opens again (`lib.rs:122-143`). walkdir 2.5.0 `dent.rs:130-137`: on Windows with `follow_links(false)`, `DirEntry::metadata()` returns the cached `fs::Metadata` from the directory read, with no syscall.
- **Fix:** for regular files on Windows, build `FileMeta` from the single identity handle. `BY_HANDLE_FILE_INFORMATION` carries attributes, size, creation and last-write FILETIMEs, the index and the link count, which is everything `FileMeta` needs. If that open fails (access denied, path too long), fall back to `entry.metadata()` (cached, free) with `file_id=None, nlink=1`, as today. Non-regular entries use `entry.metadata()` directly.

  Why the handle values are preferred over the find data: NTFS updates the size and last-write time in the directory entry lazily for files that are open for writing. `reader.metadata()` (std `lstat`, also handle-based) is what `meta_matches` compares against later, so handle-based values avoid spurious ChangedDuringRead and rehash.

  Unix is unchanged: walkdir's Unix `metadata()` is an `lstat`, the same cost as today.
- **Rejected alternatives:**
  - Find data plus the identity open (the review's suggestion): still two sources and stale-size risk, for the same open count.
  - Enumerating with `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)` (zero per-file opens): it gives the file id but not the link count, and it replaces walkdir. That is a larger change; noted as a follow-up.
- **Boilerplate:**
```rust
// crates/backend-generic/src/lib.rs  (win_sys)
/// Fields of one `GetFileInformationByHandle` call on an attributes-only handle.
pub struct WinFileInfo { pub attrs: u32, pub size: u64, pub created: u64, pub written: u64, pub index: u64, pub links: u32 }

pub fn file_info(path: &Path) -> Option<WinFileInfo> {
    // same open as file_identity (access 0, SHARE_ALL, BACKUP_SEMANTICS | OPEN_REPARSE_POINT)
    // then: WinFileInfo { attrs: info.dwFileAttributes,
    //   size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
    //   created: ft_u64(info.ftCreationTime), written: ft_u64(info.ftLastWriteTime),
    //   index: ..., links: info.nNumberOfLinks }
    todo!()
}

// crates/backend-generic/src/meta.rs
#[cfg(windows)]
const UNIX_EPOCH_AS_FILETIME: u64 = 116_444_736_000_000_000;

/// FILETIME (100 ns since 1601) to Unix ns; `None` before 1970, matching `to_ns`.
#[cfg(windows)]
fn filetime_ns(ft: u64) -> Option<i64> {
    ft.checked_sub(UNIX_EPOCH_AS_FILETIME)
        .and_then(|d| i64::try_from(d).ok()?.checked_mul(100))
}

/// Shared by `file_meta_from` and `file_meta_from_info` so both map attributes identically.
#[cfg(windows)]
fn win_attr_bits(fa: u32) -> i64 { /* body moved out of file_meta_from's cfg(windows) block */ todo!() }

#[cfg(windows)]
pub fn file_meta_from_info(i: &crate::win_sys::WinFileInfo) -> FileMeta {
    let mut attrs = win_attr_bits(i.attrs);
    if i.links > 1 { attrs |= attrs::HARDLINK; }
    FileMeta {
        file_id: Some(i.index.to_le_bytes().to_vec()),
        nlink: i64::from(i.links),
        size: i64::try_from(i.size).unwrap_or(i64::MAX),
        mtime_ns: filetime_ns(i.written).unwrap_or(0),
        ctime_ns: filetime_ns(i.created),
        attrs,
        kind: EntryKind::RegularFile,
    }
}

// crates/backend-generic/src/walk.rs, replacing the symlink_metadata match
let meta = match file_meta_for(&entry, ft) { Some(m) => m, None => continue };
visit(WalkItem::File { path: raw, meta })?;

#[cfg(windows)]
fn file_meta_for(entry: &walkdir::DirEntry, ft: std::fs::FileType) -> Option<FileMeta> {
    if ft.is_file() {
        if let Some(info) = crate::win_sys::file_info(entry.path()) {
            return Some(crate::meta::file_meta_from_info(&info));
        }
    }
    entry.metadata().ok().map(|md| file_meta_from(&md, ft)) // cached find data, no syscall
}

#[cfg(not(windows))]
fn file_meta_for(entry: &walkdir::DirEntry, ft: std::fs::FileType) -> Option<FileMeta> {
    entry.path().symlink_metadata().ok().map(|md| file_meta_from(&md, ft))
}
```
- **Tests to add:**
  - A pure unit test for `filetime_ns`: the epoch maps to 0, epoch+1 to 100, and pre-epoch to `None`.
  - A Windows-only test: for a temp file, `file_meta_from_info` equals `reader.metadata()` on size, mtime and ctime. This proves no rehash after the upgrade.
  - The existing `mft_vs_walk` self-check stays valid.
- **Risk / follow-ups:**
  - `filetime_ns` must match std's `SystemTime` conversion exactly, or every unchanged file looks modified on the first rescan after upgrade. The test above pins this; std's Windows `SystemTime` is FILETIME-based, so the same intervals give the same ns.
  - `fill_file_identity` becomes unused on the walker path; keep or remove it.
  - Run the Windows cross-check.
  - To measure (manual, Windows): `dedupe scan --no-mft --no-db <1M-file tree>` wall time, and the count of `IRP_MJ_CREATE` per file in Process Monitor. Expect 2 to 1.

### T8.7 Per-directory volume-root memo shared by extents and open-by-id (closes GEN-10, WIO-04)
- **Verification:**
  - GEN-10 Confirmed: `extents.rs:176-194` calls `volume_root(path)` (`GetVolumePathNameW`) before the cluster-size cache lookup, for every file, and takes a global `Mutex` twice.
  - WIO-04 Confirmed: `backend-windows/src/handle.rs:206-213` calls `volume_root(path)` on every `open_target` that carries an id, then serializes all hash workers on `self.hints.lock()`.
- **Fix:** add one memo in `backend-generic::win_sys`, keyed by the file's parent directory. Every entry directly inside one directory is on that directory's volume (a mount point is itself a directory, so it is a different parent). The memo uses `RwLock`, so warm hits take a shared lock only, and it is cleared when it reaches 65,536 entries to bound memory. Rows are processed roughly folder by folder, so the hit rate is high. Both callers use it. `HandleReader::hints` becomes an `RwLock` with a read-locked fast path and a double-checked write on a miss.

  The second `CreateFileW` per file in the extent lookup (separate from the hash open) is inherent to ordering before reading. Reading extents from the `$MFT` data runs during enumeration would remove it; that is a follow-up.
- **Rejected alternatives:**
  - Keying by volume serial from `GetFileInformationByHandle`: it works for extents (a handle exists) but not for open-by-id, which exists to avoid the path open.
  - Longest-known-root prefix: it is wrong for nested mount points.
  - Passing the volume root through `ReadTarget`: it changes a core trait for a Windows-only concern.
- **Boilerplate:**
```rust
// crates/backend-generic/src/lib.rs (win_sys)
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, PoisonError, RwLock};

const DIR_MEMO_CAP: usize = 65_536;

/// `volume_root(path)`, memoized per parent directory. `None` is memoized too.
pub fn volume_root_cached(path: &Path) -> Option<Arc<str>> {
    static BY_DIR: OnceLock<RwLock<HashMap<PathBuf, Option<Arc<str>>>>> = OnceLock::new();
    let Some(dir) = path.parent() else { return volume_root(path).map(Arc::from) };
    let memo = BY_DIR.get_or_init(Default::default);
    if let Some(hit) = memo.read().unwrap_or_else(PoisonError::into_inner).get(dir) {
        return hit.clone();
    }
    let root: Option<Arc<str>> = volume_root(path).map(Arc::from);
    let mut w = memo.write().unwrap_or_else(PoisonError::into_inner);
    if w.len() >= DIR_MEMO_CAP {
        w.clear();
    }
    w.insert(dir.to_path_buf(), root.clone());
    root
}

// crates/backend-generic/src/extents.rs  bytes_per_cluster
static CACHE: OnceLock<RwLock<HashMap<Arc<str>, u64>>> = OnceLock::new();
let root = volume_root_cached(path)?;
// read-lock lookup; on miss cluster_size(&root) then write-lock insert

// crates/backend-windows/src/handle.rs
hints: RwLock<HashMap<Arc<str>, Option<Arc<VolumeHint>>>>,

fn hint_for(&self, path: &Path) -> Option<Arc<VolumeHint>> {
    let root = volume_root_cached(path)?;
    if let Some(c) = self.hints.read().unwrap_or_else(PoisonError::into_inner).get(&root) {
        return c.clone();
    }
    let mut hints = self.hints.write().unwrap_or_else(PoisonError::into_inner);
    if let Some(c) = hints.get(&root) {
        return c.clone(); // another worker opened it meanwhile
    }
    let opened = open_handle(&to_wide_nul(&*root), GENERIC_READ, SHARE_ALL, FILE_FLAG_BACKUP_SEMANTICS)
        .ok().map(|h| Arc::new(VolumeHint(h)));
    hints.insert(root, opened.clone());
    opened
}
```
- **Tests to add:**
  - A Windows-only unit test: `volume_root_cached` equals `volume_root` for files in two directories, and the second call for the same directory does not call `volume_root` (count it via a `#[cfg(test)]` counter).
  - The existing `handle_reader.rs` tests cover open-by-id behaviour.
- **Risk / follow-ups:**
  - A volume mounted or unmounted during a scan could be served a stale root. Opening by id then fails and falls back to the path, so correctness holds.
  - Run the Windows cross-check.
  - To measure (manual, Windows): HDD-profile `scan` wall time on a many-small-files tree, and the `GetVolumePathNameW` call count via ETW or API Monitor.

### T8.8 Linux probe resolves anonymous devices through mountinfo (closes GEN-08)
- **Verification:** GEN-08 Confirmed. `probe.rs:53-68` maps `st_dev` to `/sys/dev/block/<maj>:<min>` only. btrfs (each subvolume), ZFS, bcachefs and overlayfs report an anonymous major 0 device with no sysfs block node, so both reads fail and the probe returns `None`, which resolves to `StorageKind::Unknown`.
- **Fix:** when the direct lookup yields `None`:
  1. Canonicalize the path and pick the `/proc/self/mountinfo` entry with the longest mount point that is a component-wise prefix (`Path::starts_with`). Longest prefix, not a `maj:min` match, because nested btrfs subvolumes have their own anonymous dev and no mountinfo line.
  2. Resolve the backing devices by fstype:
     - `btrfs`: the source device's `st_rdev`. For multi-device btrfs, also every entry of the `/sys/fs/btrfs/<fsid>/devices/` directory that contains the source device (read `/sys/class/block/<name>/dev` for `maj:min`).
     - `bcachefs`: the source is a `:`-separated device list.
     - `zfs`: the source is `pool/dataset`. Run `zpool list -vHPL <pool>` under the GEN-22 timeout helper and take the tokens starting with `/dev/`.
     - `overlay`: recurse once on `upperdir=` (or the first `lowerdir=` entry) from the super options.
     - Anything else (fuse, nfs, tmpfs): `None`.
  3. Combine the results: any device rotational gives `Some(true)` (HDD policy is the safe one for mixed arrays), all non-rotational gives `Some(false)`, otherwise `None`.

  dm-crypt and LVM sources resolve through `/sys/dev/block/253:N`, which has its own `queue/rotational`.
- **Rejected alternatives:** the `BTRFS_IOC_FS_INFO` ioctl (unsafe plus a struct layout, for what sysfs already offers). Calling `lsblk` (a subprocess even for the common case).
- **Boilerplate:**
```rust
// crates/backend-generic/src/probe.rs  (mod sysfs; pure parts compile under cfg(test) on all targets)
#[derive(Debug, PartialEq)]
pub(super) struct MountEntry { pub mount_point: PathBuf, pub fstype: String, pub source: String, pub super_opts: String }

/// One `/proc/self/mountinfo` line. Fields: id parent maj:min root mount_point opts [optional...] - fstype source super_opts
pub(super) fn parse_mountinfo_line(line: &str) -> Option<MountEntry> {
    let (left, right) = line.split_once(" - ")?;
    let mount_point = left.split(' ').nth(4)?;
    let mut r = right.split(' ');
    Some(MountEntry {
        mount_point: PathBuf::from(unescape_octal(mount_point)),
        fstype: r.next()?.to_owned(),
        source: unescape_octal(r.next()?),
        super_opts: r.next().unwrap_or("").to_owned(),
    })
}

/// mountinfo escapes space, tab, newline and backslash as `\ooo`.
pub(super) fn unescape_octal(s: &str) -> String { todo!() }

/// Longest component-wise mount point containing `path` (already canonical).
pub(super) fn mount_for<'a>(entries: &'a [MountEntry], path: &Path) -> Option<&'a MountEntry> {
    entries.iter().filter(|e| path.starts_with(&e.mount_point))
        .max_by_key(|e| e.mount_point.components().count())
}

pub(super) fn combine(flags: impl IntoIterator<Item = Option<bool>>) -> Option<bool> {
    let (mut any, mut all_known) = (false, true);
    let mut n = 0;
    for f in flags { n += 1; match f { Some(true) => any = true, Some(false) => {}, None => all_known = false } }
    if any { Some(true) } else if all_known && n > 0 { Some(false) } else { None }
}

#[cfg(target_os = "linux")]
pub(super) fn rotational(path: &Path) -> Option<bool> {
    rotational_inner(path, 0)
}

#[cfg(target_os = "linux")]
fn rotational_inner(path: &Path, depth: u8) -> Option<bool> {
    let st = rustix::fs::stat(path).ok()?;
    let (major, minor) = (rustix::fs::major(st.st_dev), rustix::fs::minor(st.st_dev));
    if major != 0 {
        return rotational_of_dev(major, minor); // today's body (queue + parent queue)
    }
    if depth > 1 { return None; }
    let canon = std::fs::canonicalize(path).ok()?;
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let entries: Vec<MountEntry> = info.lines().filter_map(parse_mountinfo_line).collect();
    let m = mount_for(&entries, &canon)?;
    match m.fstype.as_str() {
        "btrfs" => combine(btrfs_devices(&m.source)?.into_iter().map(|(a, b)| rotational_of_dev(a, b))),
        "bcachefs" => combine(m.source.split(':').map(|d| dev_of_node(Path::new(d)).and_then(|(a, b)| rotational_of_dev(a, b)))),
        "zfs" => combine(zpool_devices(m.source.split('/').next()?)?.iter()
            .map(|d| dev_of_node(d).and_then(|(a, b)| rotational_of_dev(a, b)))),
        "overlay" => overlay_dir(&m.super_opts).and_then(|d| rotational_inner(&d, depth + 1)),
        _ => None,
    }
}
// dev_of_node: stat(node).st_rdev -> (major, minor). btrfs_devices: the source rdev plus siblings
// from /sys/fs/btrfs/*/devices/*. zpool_devices: GEN-22 helper on ["zpool","list","-vHPL",pool].
```
- **Tests to add:**
  - `parse_mountinfo_line` on real-format samples: a btrfs subvolume line with `subvol=/@home`, a ZFS dataset, overlay with `upperdir=`, and a mount point with `\040`.
  - `mount_for`: `/home` must not match `/homework` (component-wise check).
  - `combine` truth table. All pure, so they run on every OS like the existing sysfs tests.
- **Risk / follow-ups:**
  - Not confirmed: the exact `zpool list -vHPL` output format (vdev lines indented with a tab, full `/dev/...` paths with `-P`) and whether `-L` is accepted by every OpenZFS version. Treat any parse failure as `None`.
  - Not confirmed: whether bcachefs prints the colon-joined source in mountinfo on every kernel version.
  - Containers often hide `/sys/fs/btrfs`; the result is then `None`, as today.
  - To measure: on Fedora (btrfs), `dedupe scan` must no longer print the unknown-storage note, and the resolved queue depth (visible in debug logs) must match the SSD default.

### T8.9 Batched progress advances, no Sync bound, poison-tolerant (closes PIPE-20)
- **Verification:** PIPE-20 Confirmed. `progress.rs:18` requires `Sink: Send + Sync`, but `SharedSink = Arc<Mutex<dyn Sink>>` only needs `Send` (`Mutex<T: Send>` is `Sync`). `emit_advance` (:47-51) locks once per row from `pass_prefix.rs:64`, `pass_full.rs:129,207,261` and verify. A poisoned lock silently drops all later progress.
- **Fix:** drop `Sync`. Recover a poisoned guard with `PoisonError::into_inner` (the sink state is only counters). Add a tiny accumulator that flushes every `PROGRESS_CHUNK` (moved from `pass_enumerate.rs:20` into `progress.rs`) and on `flush`/`Drop`. Passes call `adv.flush()` before `emit_finished`, so the sum still equals `total`. Lock acquisitions drop by a factor of 1024.
- **Rejected alternatives:** an `AtomicU64` counter polled by the UI (it changes the trait shape and needs a ticker thread). Rate limiting by time (`Instant::now()` per row costs about as much as the lock).
- **Boilerplate:**
```rust
// crates/core/src/progress.rs
pub trait Sink: Send { /* unchanged methods */ }
pub const PROGRESS_CHUNK: u64 = 1_024;

fn lock(s: &SharedSink) -> std::sync::MutexGuard<'_, dyn Sink> {
    s.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
pub fn emit_advance(s: &SharedSink, run_id: i64, volume_id: i64, n: u64) { lock(s).advance(run_id, volume_id, n) }
// emit_started / emit_finished likewise

/// Coalesces per-row advances into one sink call per `PROGRESS_CHUNK`.
pub struct Advancer<'a> { sink: &'a SharedSink, run_id: i64, volume_id: i64, pending: u64 }
impl<'a> Advancer<'a> {
    pub fn new(sink: &'a SharedSink, run_id: i64, volume_id: i64) -> Self { Self { sink, run_id, volume_id, pending: 0 } }
    pub fn add(&mut self, n: u64) {
        self.pending += n;
        if self.pending >= PROGRESS_CHUNK { self.flush(); }
    }
    pub fn flush(&mut self) {
        if self.pending > 0 { emit_advance(self.sink, self.run_id, self.volume_id, std::mem::take(&mut self.pending)); }
    }
}
impl Drop for Advancer<'_> { fn drop(&mut self) { self.flush(); } }
```
- **Tests to add:** `core/tests/progress.rs` already sums advances to `total`. Add a case with more than `PROGRESS_CHUNK` rows and one with fewer (flush on finish). Add a test that poisons the sink mutex (a panicking `advance` inside `catch_unwind`) and checks that later events still arrive.
- **Risk / follow-ups:** the bar updates in steps of 1024 rows. With indicatif's draw rate limit this is invisible at scale. For very slow large files (full pass on HDD), use a smaller chunk or flush when a job finishes if the UI feels stuck.

### T8.10 diskutil with a deadline, std only (closes GEN-22)
- **Verification:** GEN-22 Confirmed (theoretical hang). `probe.rs:110-115` calls `Command::output()` with no timeout.
- **Fix:** a shared `cfg(unix)` helper that spawns the command, drains stdout on a thread (so a full pipe cannot deadlock), polls `try_wait` until a 5 s deadline, and kills and reaps the child on timeout. On timeout the reader thread is detached rather than joined, in case a grandchild holds the pipe. GEN-08's `zpool` call reuses it.
- **Boilerplate:**
```rust
// crates/backend-generic/src/probe.rs
#[cfg(unix)]
pub(crate) fn output_with_timeout(mut cmd: std::process::Command, timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || { let mut b = Vec::new(); out.read_to_end(&mut b).map(|_| b) });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return reader.join().ok()?.ok(),
            Ok(Some(_)) => return None,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => { let _ = child.kill(); let _ = child.wait(); return None; } // reader thread detached
        }
    }
}
// macos::solid_state: let mut cmd = Command::new(DISKUTIL); cmd.args(["info", "-plist", &device]);
// let xml = output_with_timeout(cmd, PROBE_TIMEOUT)?;  with const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
```
- **Tests to add:** a unix test using `sh -c 'sleep 10'` with a 100 ms timeout: returns `None` within about 1 s. `sh -c 'echo hi'` returns `b"hi\n"`.
- **Risk / follow-ups:** none.

### T8.11 Ignore FIEMAP extents without a real physical location (closes GEN-15)
- **Verification:** GEN-15 Confirmed. `extents.rs:76-79` returns `fe_physical` regardless of `fe_flags`.
- **Fix:** return `None` when the first extent has `UNKNOWN` (0x2), `DELALLOC` (0x4), `DATA_INLINE` (0x200) or `DATA_TAIL` (0x400) set; values are from `<linux/fiemap.h>`. Do not pass `FIEMAP_FLAG_SYNC`: it forces writeback per file, which is far costlier than a slightly worse order. On btrfs `fe_physical` is a logical address in chunk space. It is still monotone enough to order by, so keep it.
```rust
const FIEMAP_EXTENT_UNKNOWN: u32 = 0x0000_0002;
const FIEMAP_EXTENT_DELALLOC: u32 = 0x0000_0004;
const FIEMAP_EXTENT_DATA_INLINE: u32 = 0x0000_0200;
const FIEMAP_EXTENT_DATA_TAIL: u32 = 0x0000_0400;
const NO_LOCATION: u32 = FIEMAP_EXTENT_UNKNOWN | FIEMAP_EXTENT_DELALLOC | FIEMAP_EXTENT_DATA_INLINE | FIEMAP_EXTENT_DATA_TAIL;

let ext = req.fm_extents[0];
if rc != 0 || req.fm_mapped_extents == 0 || ext.fe_flags & NO_LOCATION != 0 {
    return Ok(None);
}
Ok(Some(ext.fe_physical))
```
- **Tests to add:** split out a pure `physical_if_located(&FiemapExtent) -> Option<u64>` and unit-test it with each flag.

---

## T9. Build config, error types, dead code, docs and test gaps

**Findings covered:** DB-13, DB-18, CORE-16, CORE-17, CORE-14, PIPE-18, PIPE-19, PIPE-21, PIPE-16, CORE-08, CORE-10, CORE-11, CORE-12, CORE-18, CLI-14, CLI-15, CLI-16, CLI-17, CLI-18, CLI-19, CLI-25, GEN-20, GEN-21, WIO-14

**Root causes:**
- Row decoding is written as ad-hoc closures that default on bad data instead of a typed `FromSql` decode, and `CoreError` has no "corrupt DB" or "io at path" variant -> DB-18, CORE-16.
- A pluggability seam (`Hasher`, free-string `hash_algo`, `read_range`) was declared in Phase 0 and never wired, while the real hashing hard-codes BLAKE3 -> CORE-14, PIPE-18, part of CORE-18.
- The naive pipeline was frozen at Phase 0 while the persistent passes gained eligibility, re-stat and small-file rules -> PIPE-16.
- Workspace config predates the cargo-deny v2 schema and the 2026-10-18 repo move; docs were not swept after the v2 schema, `--format auto`, DB discovery, `--verify` and the walker file-id changes -> CLI-14, CLI-15, CLI-16, CLI-17, CLI-19, CORE-18, PIPE-21, DB-13.
- Tests prove convergence or "something happened" rather than the specific mechanism -> CORE-08, CORE-10, CORE-11, CORE-12, CLI-18, GEN-21, WIO-14.

**Recommended order of work:**
1. T9.1 (DB-13) so the core suite can go green again before anything else.
2. T9.2 error enum and typed decode (DB-18, CORE-16).
3. T9.3 delete the dead hash seam (CORE-14, PIPE-18), then T9.4 `Config::validate` and lookup meta sizes (CORE-17).
4. T9.5 naive alignment (PIPE-16), T9.6 scheduler key contract (PIPE-19).
5. T9.7-T9.9 build config (CLI-14, CLI-15, CLI-16, CLI-17).
6. T9.10 docs sweep (CLI-19, CORE-18, PIPE-21) in one commit.
7. T9.11 idiom cleanups (GEN-20, CLI-25).
8. T9.12 test skeletons (CORE-08, CORE-10, CORE-11, CORE-12, GEN-21, WIO-14, CLI-18): suggested only, written when the user asks.

---

### T9.1 Assert the current schema version (closes DB-13)
- **Verification:** DB-13 Confirmed. `crates/core/src/db/mod.rs:86` asserts `== 1`, `crates/core/src/db/schema.rs:7` has `EXPECTED_VERSION = 2`. The test expectation is wrong, not the implementation.
- **Fix:** compare against the constant so a future bump does not break it again.
```rust
// crates/core/src/db/mod.rs (test file_db_enables_wal)
// before
assert_eq!(schema::current_version(&db.conn).unwrap(), 1);
// after
assert_eq!(
    schema::current_version(&db.conn).unwrap(),
    schema::EXPECTED_VERSION
);
```
- **Rejected alternatives:** hard-coding `2` (breaks on the next bump).
- **Tests to add:** none; this is the test.
- **Risk / follow-ups:** suggests the core suite has not run green since v2. Run the full `cargo test --workspace` once (via a subagent, when the user asks) after this lands.

---

### T9.2 Typed column decode and two new error variants (closes DB-18, CORE-16)
- **Verification:**
  - DB-18 Confirmed: `db/queries.rs:35` `State::from_i64(..).unwrap_or(State::Error)`; `:32-33` wrong-length prefix/full blob silently becomes `None`; `:343-347` `rows_with_prefix` drops bad blobs via `filter_map(transpose)`; `:411` (`prefix_groups`), `:625` (`list_groups`), `:703` (`volume_group_members`) use `unwrap_or(Hash([0; 16]))`; `:803` `get_phase` maps an unknown phase to `Phase::None`. `lookup.rs:220-222` repeats the state and hash coercion.
  - CORE-16 Confirmed: `error.rs:11-12` `Io(#[from] io::Error)` has no path; `lookup.rs:69` `reader.metadata(path)?` and `pipeline/scan.rs:46` `canonicalize(root)?` lose the path; `exclude.rs:28,33` use `Other` for user input; `report.rs:128` uses `Other` for a negative count that can only come from a corrupt DB.
- **Fix:**
  1. Implement `rusqlite::types::FromSql` for `Hash` and `State` (both local types, so the orphan rule allows it). Every `r.get::<_, Hash>()` / `r.get::<_, Option<Hash>>()` then fails with `FromSqlConversionFailure` / `InvalidBlobSize` on a bad row, which flows into `CoreError::Sqlite` with column index and type. One impl replaces seven ad-hoc closures.
  2. Add `CoreError::Corrupt(String)` for logical corruption that is not a SQL decode (negative counts, unknown `phase_reached`), with the "delete it and scan again" hint that `SchemaVersion` already uses.
  3. Add `CoreError::IoAt { path, source }` plus a small helper, and use it only where the path is not otherwise recorded (query file in lookup, scan roots, DB open). Per-file pass errors already store `errors.path`, so the backend reader does not need to wrap (that would duplicate the path in `errors.message`).
  4. `exclude.rs` uses `InvalidArg`.
  Fail-fast on corruption is correct for a dedupe report: a zero-hash merge produces false duplicate groups, which is worse than an error. No code matches on `CoreError::Io` (checked with grep), so adding variants breaks nothing.
- **Rejected alternatives:** skip-and-warn on bad rows (keeps the report running but hides corruption from JSON consumers; only reconsider if a user hits it); replacing `Io(#[from])` entirely with a path-carrying variant (every `?` on io in core would need a `map_err`, high churn for no gain).
- **Boilerplate:**
```rust
// crates/core/src/error.rs (additions)
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    // ... existing variants unchanged ...
    #[error("io: {path}: {source}")]
    IoAt {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database is corrupt: {0} (delete it and scan again)")]
    Corrupt(String),
}

impl CoreError {
    /// `.map_err(CoreError::io_at(path))` for io calls whose path is not recorded elsewhere.
    pub fn io_at(path: &std::path::Path) -> impl FnOnce(std::io::Error) -> CoreError + '_ {
        move |source| CoreError::IoAt { path: path.to_path_buf(), source }
    }

    /// Attach a path to a pathless `Io` error coming back through a trait (e.g. `ContentReader`).
    pub fn at_path(self, path: &std::path::Path) -> CoreError {
        match self {
            CoreError::Io(source) => CoreError::IoAt { path: path.to_path_buf(), source },
            other => other,
        }
    }
}
```
```rust
// crates/core/src/db/decode.rs (new, `mod decode;` in db/mod.rs) - or at the bottom of model.rs
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ValueRef};

use crate::model::{Hash, State};

impl FromSql for Hash {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let bytes = value.as_blob()?;
        Hash::from_slice(bytes).ok_or(FromSqlError::InvalidBlobSize {
            expected_size: Hash::LEN,
            blob_size: bytes.len(),
        })
    }
}

impl FromSql for State {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let n = value.as_i64()?;
        State::from_i64(n).ok_or(FromSqlError::OutOfRange(n))
    }
}
```
```rust
// crates/core/src/db/queries.rs (shape of each call site)
// row_to_file
prefix_hash: row.get::<_, Option<Hash>>(10)?,
full_hash: row.get::<_, Option<Hash>>(11)?,
state: row.get::<_, State>(13)?,

// rows_with_prefix: no more filter_map(transpose)
.query_map(params![volume_id, run_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Hash>(1)?)))?
.collect::<std::result::Result<Vec<_>, _>>()?;

// prefix_groups / list_groups / volume_group_members
full_hash: r.get::<_, Hash>(0)?,

// get_phase
match v {
    None => Ok(Phase::None),
    Some(n) => Phase::from_i64(n)
        .ok_or_else(|| CoreError::Corrupt(format!("run_volumes.phase_reached = {n}"))),
}
```
```rust
// crates/core/src/lookup.rs:69 and :220-222
let meta = reader.metadata(path).map_err(|e| e.at_path(path))?;
// size_candidates closure
state: r.get::<_, State>(6)?,
prefix_hash: r.get::<_, Option<Hash>>(7)?,
full_hash: r.get::<_, Option<Hash>>(8)?,

// crates/core/src/pipeline/scan.rs:46
let canon = std::fs::canonicalize(root).map_err(CoreError::io_at(root))?;

// crates/core/src/report.rs:128
let to_count = |n: i64| {
    usize::try_from(n).map_err(|_| CoreError::Corrupt(format!("negative file count {n}")))
};

// crates/core/src/exclude.rs:28,33
.map_err(|e| CoreError::InvalidArg(format!("bad --exclude pattern {pattern:?}: {e}")))?;
.map_err(|e| CoreError::InvalidArg(format!("build --exclude patterns: {e}")))?;
```
- **Tests to add:** unit test in `db/decode.rs`: insert a 15-byte `full_hash` and `state = 99` into an in-memory v2 DB, assert `list_groups` / `rows_in_state` return `CoreError::Sqlite(FromSqlConversionFailure | InvalidColumnType ..)` rather than a row; `lookup` on a missing path, assert the `Display` contains the path; `ExcludeSet::new(&["[".into()])` returns `InvalidArg`.
- **Risk / follow-ups:** `FromSqlError::InvalidBlobSize` and `OutOfRange` exist in rusqlite 0.32 to my knowledge; confirm with `cargo doc -p rusqlite` (not checked online). Remaining `CoreError::Other` users (`scheduler.rs:115` test, `traits.rs:217` test, `pass_full.rs:322`, `mft_walk.rs` tests) are tests or internal invariants; leave them.

---

### T9.3 Delete the `Hasher` seam, make `hash_algo` a constant (closes CORE-14, PIPE-18)
- **Verification:** CORE-14 Confirmed and PIPE-18 Confirmed (one item). `traits.rs:144-155` defines `Hasher`; `pipeline/mod.rs:36` carries `Services.hasher`; no pass reads it; `naive.rs:201` `let _ = hasher;`. All hashing goes through `hash::hash_bytes`/`hash_prefix_target`/`hash_full_target` on `blake3::Hasher`. `config.rs:35` `hash_algo: String` is only written to and compared with `meta` (`resume.rs:32,48,62`). Dead helpers `hash.rs:181` `prefix_bytes_matches`, `:191` `config_fingerprint`, `:200` `_assert_error_type` have no callers. `naive.rs:68` `let _ = codec; // kept for signature symmetry` is stale: `codec.raw_to_path` is used at `naive.rs:142,169`.
- **Recommendation: delete, do not wire up.** Reasons: (1) the trait shape (`hash_bytes(&[u8])`) cannot express streaming or batched hashing, and the plan already approved a different trait for Phase 5 (`BatchHasher`, `.overall_plan.md` P5 skeleton), so wiring `Hasher` now would be replaced later; (2) routing every 64 KiB chunk through `&dyn Hasher` would need a stateful streaming API that does not exist; (3) a free-string algorithm label with a fixed algorithm is a correctness hazard (it can label BLAKE3 output as something else). Keep `meta.hash_algo` in the DB (the guard is still useful across releases), but source it from `hash::HASH_ALGO`.
- **Rejected alternatives:** route hashing through `svc.hasher` with a new streaming trait (speculative abstraction, conflicts with the approved `BatchHasher` design); turn `hash_algo` into a one-variant enum (adds a type with nothing to choose between; reintroduce when a second algorithm is real).
- **Boilerplate:**
```rust
// crates/core/src/traits.rs: delete `pub trait Hasher { .. }` (lines 143-155)
// crates/core/src/hash.rs: delete `Blake3Hasher` + impl, `prefix_bytes_matches`,
//   `config_fingerprint`, `_assert_error_type`; drop `Hasher` and `CoreError` from imports if unused.

// crates/core/src/pipeline/mod.rs
pub struct Services<'a> {
    pub enumr: &'a dyn Enumerator,
    pub reader: &'a dyn ContentReader,
    pub codec: &'a dyn PathCodec,
    pub volume: &'a dyn VolumeIdentity,
    pub cancel: &'a Token,
    pub sink: &'a crate::progress::SharedSink,
}

// crates/core/src/pipeline/naive.rs
pub fn scan_naive(
    roots: &[PathBuf],
    enumr: &dyn Enumerator,
    reader: &dyn ContentReader,
    codec: &dyn PathCodec,
    cfg: &Config,
    cancel: &Token,
) -> Result<(Report, ScanStats)> { /* remove both `let _ =` lines */ }

// crates/core/src/config.rs: remove `pub hash_algo: String` and its default.

// crates/core/src/pipeline/resume.rs
use crate::hash::HASH_ALGO;
let mismatch = algo != HASH_ALGO
    || fb != Some(cfg.prefix_first_bytes)
    || lb != Some(cfg.prefix_last_bytes);
// ... format!("db={algo}:{}/{} current={HASH_ALGO}:{}/{}", ..)
queries::meta_set(conn, "hash_algo", HASH_ALGO)?;

// crates/cli/src/scan.rs: drop `use dedupe_core::hash::Blake3Hasher`, `let hasher`, `hasher:` field,
// and the `svc.hasher` argument to scan_naive.
```
- **Tests to add:** none new. Test code that builds `Services { hasher: &hasher, .. }` must drop the field: `crates/core/tests/{pool,progress,phase1,verify,scale}.rs`. This is a mechanical API update, not a change of expectations; list the files to the user before editing per the project rules.
- **Risk / follow-ups:** public API break inside the workspace only. The `.overall_plan.md` line 607/637 lists `Hasher` among Phase 0 traits; update the plan text in the same change.

---

### T9.4 `Config::validate` and lookup uses stored prefix sizes (closes CORE-17)
- **Verification:** CORE-17 Confirmed. `config.rs:66` `prefix_first_bytes + prefix_last_bytes` unchecked; nothing rejects zero sizes; `storage_profile.rs:51-59` `Profile` fields are `pub`, `hash.rs:139` `chunk.max(1)` silently turns `buf_size = 0` into 1-byte reads; `cli/src/lookup.rs:18` uses `Config::default()` and `core/src/lookup.rs:91-92` never compares with `meta.prefix_*`. Latent today (no prefix flags).
- **Fix:** one `validate()` returning `InvalidArg`, called at the top of `run_scan` and `lookup`. In `lookup`, read the prefix sizes from `meta` when present and hash with those, so a DB built with other sizes still matches. Reuse the parsing already in `resume::check_config_guard` by extracting it.
- **Rejected alternatives:** private fields with a builder (heavy churn across CLI and tests for a latent issue); refusing lookup on mismatch (worse UX than adopting the DB's sizes, which is always correct for a read query).
- **Boilerplate:**
```rust
// crates/core/src/config.rs
use crate::error::{CoreError, Result};

impl Config {
    /// `F + L`. Callers run `validate` first, so this cannot overflow.
    pub fn prefix_total(&self) -> u64 {
        self.prefix_first_bytes.saturating_add(self.prefix_last_bytes)
    }

    pub fn validate(&self) -> Result<()> {
        if self.prefix_first_bytes == 0 || self.prefix_last_bytes == 0 {
            return Err(CoreError::InvalidArg("prefix sizes must be at least 1 byte".into()));
        }
        self.prefix_first_bytes
            .checked_add(self.prefix_last_bytes)
            .ok_or_else(|| CoreError::InvalidArg("prefix sizes overflow".into()))?;
        if self.profile.queue_depth == 0 || self.profile.buf_size == 0 {
            return Err(CoreError::InvalidArg("queue depth and buffer size must be >= 1".into()));
        }
        Ok(())
    }
}

// crates/core/src/pipeline/resume.rs (extracted from check_config_guard)
/// `(prefix_first_bytes, prefix_last_bytes)` stored in `meta`, if the DB has them.
pub fn stored_prefix_sizes(conn: &Connection) -> Result<Option<(u64, u64)>> {
    let fb = queries::meta_get(conn, "prefix_first_bytes")?.and_then(|s| s.parse().ok());
    let lb = queries::meta_get(conn, "prefix_last_bytes")?.and_then(|s| s.parse().ok());
    Ok(fb.zip(lb))
}

// crates/core/src/lookup.rs (top of `lookup`)
cfg.validate()?;
let effective;
let cfg = match crate::pipeline::resume::stored_prefix_sizes(conn)? {
    Some((f, l)) if (f, l) != (cfg.prefix_first_bytes, cfg.prefix_last_bytes) => {
        effective = Config { prefix_first_bytes: f, prefix_last_bytes: l, ..cfg.clone() };
        &effective
    }
    _ => cfg,
};
// pipeline/scan.rs run_scan: `cfg.validate()?;` before Db::open.
```
- **Tests to add:** `config_validate_rejects_zero_prefix`, `config_validate_rejects_overflow`; phase1 `lookup_uses_db_prefix_sizes` (scan with F=8,L=4, look up with default config, expect a match).
- **Risk / follow-ups:** `Profile` built by `storage_profile::resolve` already passes; only hand-built profiles in tests could trip `validate`.

---

### T9.5 Align the naive reference with the persistent pipeline (closes PIPE-16)
- **Verification:** PIPE-16 Confirmed. The review's line numbers do not match the current file (naive.rs is 203 lines); the behaviours are present at: (a) no re-stat after any read, `naive.rs:136-176`; (b) roots are not canonicalized or deduplicated, `naive.rs:72-73`, and `inode_key` falls back to the record index, `naive.rs:37-47`; persistent canonicalizes and dedups roots (`scan.rs:45-55`), and nested roots collapse by idempotent upsert; (c) `meta.size == 0` skipped but `size < 0` counted as error, `naive.rs:90-102`, while persistent skips `size <= 0` (`pass_enumerate.rs:292`); (d) small files hashed twice and counted twice in `bytes_hashed`, `naive.rs:136-176`. Additional divergence found: naive never applies `cfg.exclude` itself (persistent does at `pass_enumerate.rs:87,100`), which is why CLAUDE.md says `--no-db` on the `$MFT` path does not filter.
- **Exact list to align (naive must match persistent on each):**
  1. Roots: canonicalize each root, drop exact duplicates, and drop any root that is a descendant of another root (path-prefix by components). Report paths are then absolute in both pipelines.
  2. Eligibility: a single shared `skip_reason(meta, include_placeholders)` (move the private fn from `pass_enumerate.rs:288` to `pipeline/mod.rs` as `pub(crate)`), so non-regular, `size <= 0`, and placeholders count as skipped identically.
  3. Exclude: call `cfg.exclude.is_excluded(&native)` on every item, mirroring `pass_enumerate.rs:100` (the walker already prunes; this covers the `$MFT` enumerator).
  4. Dedupe by path within a run: keep a `HashSet<RawPath>` of seen file paths so overlapping walks cannot add one file twice even with `file_id = None`.
  5. Re-stat: after the prefix read and after the full read, call `reader.metadata(native)` and compare with the enumerated stat using the same rule as `pass_prefix::meta_matches` (size, mtime, ctime when both present); a mismatch counts as `files_errored` and drops the file, like persistent state -2 `ChangedDuringRead`.
  6. Small files: when `bytes <= cfg.prefix_total()`, reuse the prefix hash as the full hash (persistent `HashOutcome::Small`), so they are read once and `bytes_hashed` counts them once.
  7. Remove the stale `let _ = codec;` and (after T9.3) the `hasher` parameter.
- **Rejected alternatives:** making `--no-db` call `run_scan` with an in-memory SQLite DB (would remove the independent reference the plan relies on); leaving the divergence documented (tests comparing the two pipelines would keep hiding or inventing differences).
- **Boilerplate:**
```rust
// crates/core/src/pipeline/mod.rs (shared rules, moved from pass_enumerate.rs / pass_prefix.rs)
/// `Some(())` when the file is skipped (state -1). Shared by both pipelines.
pub(crate) fn skip_reason(meta: &FileMeta, include_placeholders: bool) -> Option<()> { /* moved body */ }

/// Same stat rule as `pass_prefix::meta_matches`, on two `FileMeta` values.
pub(crate) fn same_stat(before: &FileMeta, after: &FileMeta) -> bool {
    let ctime_ok = match (before.ctime_ns, after.ctime_ns) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    };
    before.size == after.size && before.mtime_ns == after.mtime_ns && ctime_ok
}

/// Canonical, deduplicated roots with nested roots removed.
pub(crate) fn normalize_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    for r in roots {
        let c = std::fs::canonicalize(r).map_err(CoreError::io_at(r))?;
        out.push(c);
    }
    out.sort();
    out.dedup();
    // Sorted order puts an ancestor before its descendants.
    let mut kept: Vec<PathBuf> = Vec::new();
    for r in out {
        if !kept.iter().any(|k| r.starts_with(k)) {
            kept.push(r);
        }
    }
    Ok(kept)
}

// crates/core/src/pipeline/naive.rs
struct Rec {
    path: RawPath,
    volume_id: i64,
    size: i64,
    bytes: u64,
    file_id_key: Option<Vec<u8>>,
    meta: FileMeta, // enumerated stat, for the re-stat check
}

// in the walk callback
let native = codec.raw_to_path(&path);
if cfg.exclude.is_excluded(&native) || !seen.insert(path.clone()) {
    return Ok(());
}
if skip_reason(&meta, cfg.include_placeholders).is_some() {
    stats.files_skipped += 1;
    return Ok(());
}

// prefix pass
match hash_prefix(reader, &native, r.bytes, cfg) {
    Ok((h, _)) if restat_ok(reader, &native, &r.meta) => { /* insert, bytes_hashed += r.bytes */ }
    Ok(_) | Err(_) => stats.files_errored += 1,
}

// full pass
let h = if r.bytes <= cfg.prefix_total() {
    prefix_hashes[&i] // small file: prefix == full, no second read
} else {
    match hash_full(reader, &native, r.bytes) {
        Ok(h) if restat_ok(reader, &native, &r.meta) => { stats.bytes_hashed += r.bytes; h }
        _ => { stats.files_errored += 1; continue; }
    }
};

fn restat_ok(reader: &dyn ContentReader, native: &Path, before: &FileMeta) -> bool {
    reader.metadata(native).is_ok_and(|after| same_stat(before, &after))
}
```
- **Tests to add:** `naive_and_persistent_reports_match_on_fixture` (same tree, compare normalized JSON); `naive_nested_roots_do_not_self_duplicate` (roots `x` and `x/sub`, fake enumerator with `file_id: None`); `naive_counts_negative_size_as_skipped`; `naive_small_file_read_once` (counting reader, assert opens == 1 per small candidate); `naive_change_during_read_is_error` (reuse `TamperReader` from phase1.rs).
- **Risk / follow-ups:** report paths in `--no-db` output become canonical (absolute), which changes output for users who pass relative roots; persistent already does this. Note it in the README `scan` section.

---

### T9.6 Scheduler key: per-file errors must not abort the pass (closes PIPE-19)
- **Verification:** PIPE-19 Confirmed. `pass_prefix.rs:349-354` and `pass_full.rs:64-69` propagate `first_extent_offset` errors; `scheduler.rs:58-61` collects with `?`; the soft-fail policy is only in `backend-generic/src/reader.rs:72-84`; the trait doc at `traits.rs:124-128` does not state it; `pipeline/mod.rs:80` uses `expect("just inserted")`.
- **Fix:** document the contract on the trait and also enforce it in the caller (treat `Err` as `None`, with a debug log), keeping `PathCache` errors fatal because they are DB errors, not per-file. Rewrite `PathCache::dir` with the entry API.
- **Rejected alternatives:** documentation only (a future backend can still abort HDD scans); making `first_extent_offset` return `Option<u64>` (cleaner, but changes a trait signature implemented in two backends for a Low item).
- **Boilerplate:**
```rust
// crates/core/src/traits.rs
/// Physical offset of the first extent, used only to order reads on rotational
/// media. `None` when unsupported or unknown. Must not fail for per-file reasons
/// (vanished, access denied): callers treat `Err` as `None`.
fn first_extent_offset(&self, _path: &Path) -> Result<Option<u64>> { Ok(None) }

// crates/core/src/pipeline/pass_prefix.rs (same shape in pass_full.rs)
scheduler.order(&mut rows, |r| {
    let p = cache.file_path(r.folder_id, &r.name)?; // DB error: fatal
    let native = ctx.svc.codec.raw_to_path(&p);
    Ok(ctx.svc.reader.first_extent_offset(&native).unwrap_or_else(|e| {
        tracing::debug!(path = %native.display(), error = %e, "extent lookup failed");
        None
    }))
})?;

// crates/core/src/pipeline/mod.rs
pub fn dir(&mut self, folder_id: i64) -> Result<&RawPath> {
    let conn = self.conn;
    match self.dirs.entry(folder_id) {
        std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
        std::collections::hash_map::Entry::Vacant(e) => {
            Ok(e.insert(queries::reconstruct_path(conn, folder_id)?))
        }
    }
}
```
- **Tests to add:** `hdd_order_survives_extent_error` (fake reader whose `first_extent_offset` returns `Err` for one path, `Profile::for_kind(Hdd)`, assert the scan completes and the file is still hashed).

---

### T9.7 Corrected `deny.toml` and `publish = false` (closes CLI-14, CLI-15)
- **Verification:**
  - CLI-14 Confirmed. `deny.toml:8` `vulnerability` and `deny.toml:14` `unlicensed` are listed as removed keys in the current cargo-deny docs (advisories cfg: "`vulnerability` - Removed, all vulnerability advisories now emit errors"; licenses cfg: "`unlicensed`: Removed"). `unmaintained = "warn"` is no longer valid: current values are `all` (default), `workspace`, `transitive`, `none`. `wildcards = "deny"` with no `allow-wildcard-paths` and no `publish = false` in any of the 7 manifests flags every internal `path` dependency.
  - CLI-15 Confirmed. No `[sources]` table; both `unknown-registry` and `unknown-git` default to `warn` per the sources cfg docs.
- **Fix:** v2 schema keys, explicit `[sources]` denial, `allow-wildcard-paths = true`, and `publish = false` inherited from `[workspace.package]`. MPL-2.0: no crate in the current `Cargo.lock` (181 packages) is one I recognize as MPL-licensed, so remove it from `allow`; if `cargo deny check licenses` then fails, re-add it with a comment naming the crate.
- **Rejected alternatives:** `wildcards = "allow"` (drops the real policy for crates.io deps); adding `version = "0.1.0"` to every path dep (noise, and still wrong for unpublished crates).
- **Boilerplate:**
```toml
# deny.toml
# cargo-deny configuration (v2 config schema). `cargo deny check` is the manual
# dependency-policy gate (see CLAUDE.md). Check the installed version with
# `cargo deny --version`; the keys below follow the current docs.
#
# Advisories: every vulnerability, notice and unsound advisory is an error;
#   unmaintained is an error for direct dependencies of workspace crates only.
# Licenses: deny everything not explicitly allowed.
# Bans: warn on duplicate versions; no wildcard versions except internal path deps.
# Sources: crates.io only; no git dependencies, no other registries.

[graph]
targets = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "x86_64-pc-windows-gnu",
]
all-features = false

[advisories]
yanked = "warn"
unmaintained = "workspace"
ignore = []

[licenses]
confidence-threshold = 0.93
allow = [
    "MIT",
    "Apache-2.0",
    "Apache-2.0 WITH LLVM-exception",
    "BSD-2-Clause",
    "BSD-3-Clause",
    "ISC",
    "Unicode-3.0",
    "Unicode-DFS-2016",
    "Zlib",
    "CC0-1.0",
    "Unlicense",
]
# Workspace crates are publish = false, so their own license field is not checked.
[licenses.private]
ignore = true

[bans]
multiple-versions = "warn"
wildcards = "deny"
# Path deps between publish = false workspace crates are not wildcard violations.
allow-wildcard-paths = true
deny = []

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-registry = ["https://github.com/rust-lang/crates.io-index"]
allow-git = []
```
```toml
# Cargo.toml
[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.83"
license = "MIT OR Apache-2.0"
repository = "https://example.invalid/dedupe"
publish = false

# each of crates/{core,backend-generic,backend-windows,cli,tui}/Cargo.toml,
# bench/Cargo.toml, xtask/Cargo.toml, under [package]:
publish.workspace = true
```
- **Tests to add:** none; verification is `cargo deny check` run by the user (it is a manual gate per CLAUDE.md).
- **Risk / follow-ups:** (1) Not confirmed online: the docs page for `allow-wildcard-paths` says "private" crates without defining it; my understanding is that cargo-deny treats `publish = false` as private, which is why the manifests need it. Confirm with a `cargo deny check bans` run. (2) `unmaintained = "workspace"` is a judgment call; `"all"` is stricter and may fail on transitive crates (for example `number_prefix` via `indicatif`, if an advisory exists). (3) Explicit `targets` is optional; `targets = []` (all targets) also works but evaluates wasm-only deps. (4) If CC3 later publishes `dedupe` to crates.io, flip `publish` for that crate only and give its path deps versions.

---

### T9.8 Workspace `unsafe_code` lint (closes CLI-16)
- **Verification:** CLI-16 Confirmed. `Cargo.toml:20-22` has only `[workspace.lints.clippy] all, perf`; no `[workspace.lints.rust]`. `crates/core/src/lib.rs:1` forbids and `crates/backend-windows/src/lib.rs:11` denies in source; `cli`, `bench`, `xtask`, `tui` have nothing. `backend-generic` has `unsafe` in `lib.rs` (win_sys, lines 64-161), `probe.rs`, `extents.rs`, `volume.rs` with no lint attribute. `clippy::perf` is a subset of `clippy::all`.
- **Fix:** workspace `unsafe_code = "deny"`, not `forbid`. A per-crate `[lints.rust]` override cannot be combined with `[lints] workspace = true` (Cargo rejects the manifest; a crate must either inherit all workspace lints or declare all its own). So the "override" for the backends is done in source, which `deny` allows and a command-line `forbid` would not: `backend-windows` already uses local `#[expect(unsafe_code)]`; `backend-generic` gets `#[expect(unsafe_code, reason = "...")]` on each cfg module that calls the OS. `core` keeps its stronger `#![forbid(unsafe_code)]` source attribute. Drop `perf`.
- **Rejected alternatives:** `forbid` at workspace level (backends could not opt in at all); backends opting out of `workspace = true` and copying the clippy lints (two lint lists to keep in sync).
- **Boilerplate:**
```toml
# Cargo.toml
[workspace.lints.rust]
# `deny`, not `forbid`: the two backend crates opt in per module with
# `#[expect(unsafe_code, reason = "...")]`, which `forbid` would not allow.
# `core` keeps `#![forbid(unsafe_code)]` in source.
unsafe_code = "deny"

[workspace.lints.clippy]
all = "deny"
```
```rust
// crates/backend-generic/src/lib.rs (on the windows FFI module)
#[cfg(windows)]
#[expect(unsafe_code, reason = "Win32 handle and file-identity FFI")]
mod win_sys { /* ... */ }

// crates/backend-generic/src/volume.rs (macOS getattrlist / Windows volume FFI modules)
#[cfg(target_os = "macos")]
#[expect(unsafe_code, reason = "getattrlist and statfs FFI")]
mod imp { /* ... */ }

// same pattern on the cfg modules in probe.rs and extents.rs
```
- **Tests to add:** none; `cargo clippy --workspace --all-targets --all-features -- -D warnings` plus the two Windows cross-check commands prove it.
- **Risk / follow-ups:** `#[expect]` on a module whose cfg is inactive produces nothing, which is fine; on an active module with no unsafe it raises `unfulfilled_lint_expectations`, which is the point. MSRV 1.83 supports `#[expect]` (stable since 1.81).

---

### T9.9 Drop `env-filter`, use a fixed max level (closes CLI-17)
- **Verification:** CLI-17 Confirmed. `Cargo.toml:33` enables `env-filter`; `cli/src/main.rs:420-434` builds the filter from a constant string, so `RUST_LOG` has no effect. `matchers` and a second `regex-automata`/`regex-syntax` version are in `Cargo.lock`, consistent with the env-filter dependency chain.
- **Fix:** `with_max_level(tracing::Level)` and remove the feature. The CLI already exposes `--log-level`; nothing uses per-target filters.
- **Rejected alternatives:** keep the feature and honour `RUST_LOG` via `EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(level))` (valid, and the right choice if CC2 per-phase spans want per-module filtering; revisit then).
- **Boilerplate:**
```toml
# Cargo.toml
tracing-subscriber = { version = "0.3", default-features = false, features = ["fmt", "ansi", "std"] }
```
```rust
// crates/cli/src/main.rs
fn init_tracing(level: LogLevel) {
    let level = match level {
        LogLevel::Error => tracing::Level::ERROR,
        LogLevel::Warn => tracing::Level::WARN,
        LogLevel::Info => tracing::Level::INFO,
        LogLevel::Debug => tracing::Level::DEBUG,
        LogLevel::Trace => tracing::Level::TRACE,
    };
    let _ = tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .try_init();
}
```
- **Tests to add:** none.
- **Risk / follow-ups:** confirm the exact feature names (`fmt`, `ansi`, `std`) against tracing-subscriber 0.3 when editing; I did not check them online. If `default-features = false` is undesirable, simply removing `features = ["env-filter"]` is enough.

---

### T9.10 Docs sweep: exact replacement sentences (closes CLI-19, CORE-18, PIPE-21)
- **Verification:**
  - CLI-19 Confirmed for every bullet: `README.md:59` vs `cli/src/main.rs:39-66,171` (default `auto`); `README.md:90,156,170` and `CLAUDE.md:33,106` vs `cli/src/discover.rs:55-73`; `README.md:207` vs `README.md:61` and `main.rs:145`; `README.md:198` vs `backend-generic/src/meta.rs` `fill_file_identity`; `README.md:212`, `CLAUDE.md:9,116,132` vs `git rev-parse --show-toplevel` = workspace root and `.overall_plan.md` present there; `xtask/src/main.rs:5`; `cli/src/scan.rs:69-70`.
  - CORE-18 Confirmed for `traits.rs:93-99` (`read_range` single `read_at`, and unused: grep finds no caller) and `traits.rs:189-193` (StorageProbe doc vs `storage_profile.rs:123-126`). Partially confirmed for the CLAUDE.md sentence: `core/tests/verify.rs:345-485` drives `run_scan` with `VerifyMode::Byte` in five tests, so only a CLI-level test is missing.
  - PIPE-21 Confirmed: no doc mentions the 128-bit truncation as a trust boundary.
- **Fix:** replace the sentences below; delete `read_range` (unused; any future caller should use `hash::read_full`).
- **Exact text:**

README.md:59 (scan options table):
> `| --format <auto\|human\|json> | Output format. Default: auto, which prints human text on a terminal and JSON when stdout is piped or redirected (so dedupe scan ~/x > out.txt writes JSON). --json is shorthand for --format json. |`

README.md:90 (report), replace the `--db` row and add rows:
> `| [DIR] | Folder that holds the database. Default: the current folder. deDupe uses the one deDupe database (*.db) in that folder and stops with an error if there are none or several. A file path is used as the database itself. |`
> `| --db <PATH> | Read this database. Needed only when the folder holds several databases. |`
> `| -o, --output <FILE> | Write the report to this file as UTF-8 instead of stdout. With --format auto the file gets JSON. |`

README.md report and lookup `--format` rows:
> `| --format <auto\|human\|json> | Output format. Default: auto (human on a terminal, JSON when piped). |`

README.md:156 (lookup):
> `| --dir <DIR> | Folder that holds the database. Default: the current folder; the single deDupe database in it is used. |`
> `| --db <PATH> | Query this database. Needed only when the folder holds several databases. |`

README.md:170 (db):
> Each subcommand finds the database the same way as `report`: the single deDupe database in the current folder, or the folder given with `--dir <DIR>`, or the file given with `--db <PATH>`. `drop` refuses to run without `--yes`.

README.md scan options table, add rows:
> `| --exclude <GLOB> | Skip paths matching this glob and everything under a matching folder. Repeatable. Matched against the full path with / separators, case-sensitive, for example --exclude '**/node_modules'. |`
> `| --one-file-system | Stay on the starting drive (the default). Conflicts with --cross-mounts. |`
> `| --no-mft-cache | Windows only: do not read or write the MFT cache for this scan. Ignored elsewhere. |`
> `| --rebuild-mft-cache | Windows only: ignore any MFT cache, read the whole volume, and write a fresh cache. Ignored elsewhere. |`

README.md:198 (Windows section):
> **Hard-link detection on the standard walker needs one extra open per file.** The walker asks Windows for each file's id and link count; when that open fails (access denied, path too long) the file has no id and is treated as unique. Whether these ids equal the fast path's ids is not yet verified, so a rescan in the other mode may hash files again.

README.md:207 (Notes and limits):
> **Verification.** By default, files are grouped when their size and 128-bit BLAKE3 hash match. Accidental collisions are not a practical concern, but someone who can craft files could make two different files match. Before acting on results from content you do not trust, scan with `--verify byte`, which compares the bytes of every match.

README.md:212:
> See `CLAUDE.md` for the build, lint, and test commands and the code layout. The full engineering plan and status are in `.overall_plan.md`.

CLAUDE.md:9:
> The engineering plan is `.overall_plan.md` in this directory (the git repo root). Its "Status index" and "Task ledger" are the source of truth for what is done and pending. Do not restate status in this file.

CLAUDE.md:33:
> `cargo run -p dedupe -- report                 # reads the single deDupe *.db in <cwd>`

CLAUDE.md:106:
> - Default DB location differs by command: `scan` uses `<first root>/.dedupe.db`, while `report`, `lookup`, and `db` use the single deDupe-schema `*.db` in `<cwd>` (or `--dir`/`report [DIR]`) and error if there are none or several (`cli/src/discover.rs`).

CLAUDE.md:116:
> - Several source comments point to `.plan.md`. That file is now `.overall_plan.md` at the repo root, and the section numbers in those comments may not match the current plan.
(Note: a grep of `crates/`, `bench/`, `xtask/` still finds 7 `.plan.md` references, so keep the bullet until those comments are updated, then delete it.)

CLAUDE.md:122:
> - `scan --verify <off|byte>` sets `Config.verify` (plan P2.7). `pass_verify` moves state 3 -> 4 and a mismatch to -2. Core tests drive byte verify through `run_scan` (`core/tests/verify.rs`); there is no CLI-level test of `--verify byte` and no test that reaches `VerifyMismatch`.

CLAUDE.md:132:
> - Plan status and task IDs live in `.overall_plan.md`. Read that file before starting work on any task ID.

xtask/src/main.rs:5:
```rust
Some("schema") => eprintln!(
    "schema bump not implemented (schema is v{}; bumps have no migrations)",
    dedupe_core::db::schema::EXPECTED_VERSION
),
```
(needs `dedupe-core = { path = "../crates/core" }` in xtask; if that dependency is unwanted, hard-code `v2`.)

crates/cli/src/scan.rs:69-70:
```rust
// A failed probe is dropped on purpose: `None` means unknown, and core maps unknown
// storage to SSD buffers with the queue depth capped at UNKNOWN_QUEUE_DEPTH, so a
// probe error must not abort the scan.
```

crates/core/src/traits.rs:189-193:
```rust
/// Detects rotational media. Policy lives in `core::storage_profile`.
pub trait StorageProbe: Send + Sync {
    /// `None` when unknown. `storage_profile::resolve` maps unknown to
    /// `StorageKind::Unknown`: SSD buffers, queue depth capped at `UNKNOWN_QUEUE_DEPTH`.
    fn is_rotational(&self, path: &Path) -> Result<Option<bool>>;
}
```

crates/core/src/traits.rs:93-99: delete `read_range`.

crates/core/src/hash.rs:10-11:
```rust
/// `meta.hash_algo` value. BLAKE3 truncated to 128 bits: accidental collisions are
/// negligible (birthday bound about 2^64), but a crafted colliding pair is within reach
/// of a determined attacker. Grouping with `VerifyMode::Off` trusts the hash; any future
/// destructive action must require `VerifyMode::Byte`.
pub const HASH_ALGO: &str = "blake3-128";
```
- **Tests to add:** none (docs). CLI-level `--verify byte` test is suggested in T9.12.
- **Risk / follow-ups:** after T9.5, add one line to the README `scan` section that `--no-db` reports canonical (absolute) paths.

---

### T9.11 Idiom cleanups (closes GEN-20, CLI-25)
- **Verification:**
  - GEN-20 Confirmed: `volume.rs:227-228` `char::from_digit(..).unwrap()`; `volume.rs:185` `let _ = &st;`; `volume.rs:215` `buf.len() < 36` is always false on a 1024-byte array; `meta.rs:31` `#[allow(unused_mut, unused_assignments)]`; `volume.rs:115` `st.f_type as i64` sign-extends a negative 32-bit `f_type` so `BTRFS` (0x9123683E) never matches on 32-bit Linux; vfat/exfat/ntfs map to `Other`.
  - CLI-25 Confirmed: `bench/src/main.rs:163` `set_len` (not sparse on NTFS/exFAT); `:244` `partial_cmp().unwrap()`.
- **Fix:**
```rust
// crates/backend-generic/src/volume.rs (linux)
let fs_type = fs_type_from_magic(st.f_type as u32); // truncate, never sign-extend

fn fs_type_from_magic(magic: u32) -> dedupe_core::model::FsType {
    use dedupe_core::model::FsType;
    match magic {
        0xEF53 => FsType::Ext4,
        0x5846_5342 => FsType::Xfs,
        0x9123_683E => FsType::Btrfs,
        0x4D44 => FsType::Fat32,                    // MSDOS_SUPER_MAGIC (vfat)
        0x2011_BAB0 => FsType::Exfat,
        0x5346_544E | 0x7366_746E => FsType::Ntfs,  // ntfs (fuse/legacy), ntfs3
        _ => FsType::Other,
    }
}

// macOS: format_uuid without unwrap
fn hex(s: &[u8]) -> String {
    s.iter().map(|b| format!("{b:02x}")).collect()
}
// delete `let _ = &st;` and the `|| buf.len() < 36` clause.
```
```rust
// crates/backend-generic/src/meta.rs: per-OS helpers instead of mutated locals
#[cfg(unix)]
fn identity_bits(md: &Metadata) -> (Option<Vec<u8>>, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    let hard = if md.nlink() > 1 { attrs::HARDLINK } else { 0 };
    (Some(md.ino().to_le_bytes().to_vec()), md.nlink() as i64, hard)
}
#[cfg(not(unix))]
fn identity_bits(_md: &Metadata) -> (Option<Vec<u8>>, i64, i64) { (None, 1, 0) }

#[cfg(target_os = "macos")]
fn platform_attr_bits(md: &Metadata) -> i64 { /* SF_DATALESS -> PLACEHOLDER */ }
#[cfg(windows)]
fn platform_attr_bits(md: &Metadata) -> i64 { /* hidden/offline/recall bits */ }
#[cfg(not(any(target_os = "macos", windows)))]
fn platform_attr_bits(_md: &Metadata) -> i64 { 0 }

pub fn file_meta_from(md: &Metadata, ft: FileType) -> FileMeta {
    let (file_id, nlink, hard) = identity_bits(md);
    let mut attr_bits = hard | platform_attr_bits(md);
    // ... kind, symlink bit, FileMeta { .. } as today; no #[allow] needed
}
```
```rust
// bench/src/main.rs:244
sorted.sort_by(f64::total_cmp);
```
  and change the bench doc comment at `bench/src/main.rs:9` to: `lookup file 10 GiB (sparse on ext4, XFS and APFS; on NTFS and exFAT it reserves real disk space, so pass --lookup-file-bytes on Windows)`.
- **Rejected alternatives:** marking the bench file sparse with `FSCTL_SET_SPARSE` (adds Windows FFI to a bench binary for a deferred, user-run tool).
- **Tests to add:** `fs_type_from_magic_maps_known_magics` (pure fn, Linux-only cfg test, including `0x9123_683E` passed as `u32`); `format_uuid_formats_known_bytes` (macOS cfg test).
- **Risk / follow-ups:** confirm the ntfs/ntfs3 magic values against `linux/magic.h` when editing (not checked online). `FsType::Fat32` is used for vfat generally; acceptable because fs_type is informational.

---

### T9.12 Test-gap skeletons (closes CORE-08, CORE-10, CORE-11, CORE-12, GEN-21, WIO-14, CLI-18)
Per the project rules these are suggestions only; write them when the user asks.

- **Verification:**
  - CORE-08 Confirmed: `exclude.rs` has no test module; no core or CLI test passes `--exclude`.
  - CORE-10 Confirmed: `core/tests/phase1.rs:469-472` rewrites `a.txt` with the same length and only asserts `opens() > 0`; no explicit mtime bump (contrast `verify.rs:385`).
  - CORE-11 Confirmed: phase1.rs has no self-lookup, insert-on-indexed, insert-non-matching, or report-after-partial-rescan assertion; no core test uses a `C:\` mount.
  - CORE-12 Confirmed: `verify.rs:394-405` documents `VerifyMismatch` as unreachable in that scenario; `phase1.rs:349-367` checks only report equality.
  - GEN-21 Confirmed: `backend-generic` tests cover only `fs_type_from_wide` (volume) and reader open counts.
  - WIO-14 Confirmed: `backend-windows/tests/handle_reader.rs:160-180` passes a valid path with the id; `:136-158` uses a small cached file.
  - CLI-18 Confirmed: `cli/tests/kill_fuzz.rs:2` says "normalized" but compares raw strings; `:37-38` stderr null; `:46` ignores report status; `:94` discards the resume result; no `#[cfg(unix)]`; ignore reason still says "per-push CI" although there is no CI.

```rust
// crates/core/src/exclude.rs (CORE-08)
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn set(p: &[&str]) -> ExcludeSet {
        ExcludeSet::new(&p.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test] fn normalize_keeps_unix_paths() { todo!("/a/b -> /a/b") }
    #[test] fn normalize_rewrites_backslashes_and_drive() { todo!(r"C:\a\b -> C:/a/b") }
    #[test] fn normalize_strips_verbatim_drive_prefix() { todo!(r"\\?\C:\a -> C:/a") }
    #[test] fn normalize_keeps_verbatim_unc_prefix() { todo!(r"\\?\UNC\srv\share\a -> //?/UNC/srv/share/a") }
    #[test] fn star_does_not_cross_separator() { todo!("`/x/*.tmp` matches /x/a.tmp, not /x/d/a.tmp") }
    #[test] fn double_star_matches_any_depth() { todo!("`**/node_modules` matches /a/b/node_modules") }
    #[test] fn trailing_slash_pattern_semantics() { todo!("pin current behaviour of `**/target/`") }
    #[test] fn matching_is_case_sensitive() { todo!("`**/Target` does not match /a/target") }
    #[test] fn empty_set_matches_nothing() { todo!() }
    #[test] fn bad_pattern_is_invalid_arg() { todo!("`[` -> CoreError::InvalidArg (after T9.2)") }
}

// crates/core/tests/phase1.rs (CORE-08 integration)
#[test] fn run_scan_with_exclude_prunes_directory_and_drops_rows() { todo!() }
```
```rust
// crates/core/tests/phase1.rs (CORE-10: modify the existing test)
// after write(&e.root, "a.txt", b"HELLO WORLD\n"):
//   fs::OpenOptions::new().write(true).open(e.root.join("a.txt"))?
//       .set_modified(SystemTime::now() + Duration::from_secs(3600))?;
// then also assert: stored full_hash of a.txt changed, and a.txt is no longer in its old group.
```
```rust
// crates/core/tests/phase1.rs (CORE-11)
#[test] fn lookup_of_indexed_path_does_not_report_itself() { todo!("CORE-03") }
#[test] fn lookup_insert_on_indexed_file_keeps_single_row() { todo!("CORE-02") }
#[test] fn lookup_insert_non_matching_file_persists_row() { todo!("CORE-06") }
#[test] fn partial_root_rescan_still_reports_cross_root_pair() { todo!("CORE-01: extend rescan_root_scoping_*") }
#[test] fn windows_style_mount_round_trips_scan_report_lookup() {
    todo!("fake codec + MountVol with `C:\\` mount; reconstruct_file_path, lookup persist, relative_components")
}
```
```rust
// crates/core/tests/verify.rs (CORE-12)
/// Serves `from`'s bytes when `to` is read while armed, so prefix and full hashes match
/// but the real bytes differ.
struct AliasReader { inner: FsReader, from: PathBuf, to: PathBuf, armed: AtomicBool }
impl ContentReader for AliasReader { /* read_at/metadata delegate; redirect path when armed */ }

#[test]
fn verify_should_record_mismatch_when_hashes_lie() -> TestResult {
    // 1. same-size files a, b with different bytes.
    // 2. scan_run(.., &alias_armed, false, Some(<stop after FullDone>)) with byte_cfg.
    // 3. disarm; scan_run(.., resume = true, None).
    // 4. assert b state == -2 and error_kinds == [ErrorKind::VerifyMismatch.as_i64()] (5).
    todo!()
}

// crates/core/tests/phase1.rs (CORE-12: extend resume test)
#[test]
fn resume_after_full_done_reads_no_content() {
    todo!("stop after FullDone, resume with a fresh FsReader, assert reader.opens() == 0")
}
```
```rust
// crates/backend-generic/tests/walk.rs (GEN-21, Unix; tempdir-based)
#[test] fn walker_prunes_excluded_dir_below_root() { todo!() }
#[test] fn walker_does_not_follow_symlinked_dir() { todo!() }
#[test] fn walker_yields_dir_before_its_children() { todo!() }
#[test] fn file_meta_from_maps_kind_and_hardlink_bit() { todo!() }
#[test] fn mount_root_accepts_relative_path() { todo!("needs mount_root pub(crate) + unit test in volume.rs") }
// crates/backend-generic/src/volume.rs (macOS): extract
//   fn parse_vol_uuid_reply(buf: &[u8]) -> Option<String>
// and unit-test it with a fixture reply buffer (attribute_set_t + uuid_t).
#[cfg(target_os = "macos")] #[test] fn parse_vol_uuid_reply_reads_fixture() { todo!() }
```
```rust
// crates/backend-windows/tests/handle_reader.rs (WIO-14)
#[test]
fn handle_reader_open_should_succeed_by_id_when_path_is_gone() {
    // capture frn_of(path), rename the file, pass the OLD path with the id;
    // only OpenFileById can succeed. Assert bytes match the renamed file.
    todo!()
}
#[test]
fn overlapped_file_should_not_hang_when_dropped_with_uncached_reads_in_flight() {
    // large file (e.g. 256 MiB) opened with FILE_FLAG_NO_BUFFERING for the reads,
    // drop after the first read on another thread; 30 s timeout.
    todo!()
}
#[test] fn overlapped_read_should_return_short_count_when_file_shrinks() { todo!() }
#[test] fn overlapped_read_should_treat_sync_handle_eof_as_zero() { todo!("ERROR_HANDLE_EOF path") }
#[test] #[ignore = "elevated shell on an NTFS volume"]
fn raw_volume_reads_boot_sector_and_journal() { todo!() }
```
```rust
// crates/cli/tests/kill_fuzz.rs (CLI-18)
#![cfg(unix)]
fn scan(db: &Path, root: &Path, extra: &[&str]) -> std::process::Output { /* stderr: Stdio::piped() */ }
fn report(db: &Path) -> serde_json::Value {
    // assert status.success() with stderr in the message; parse JSON, then normalize
    // (sort groups and members) before comparing.
    todo!()
}
#[test]
#[ignore = "manual: 20-iteration kill -9 fuzz, slow"]
fn kill9_then_resume_converges() {
    // per iteration: record `child.try_wait()` before kill (finished_early);
    // resume must succeed, or fail with "nothing to resume" only when finished_early;
    // on mismatch print resume stderr and report stderr.
    todo!()
}
// plus: a CLI-level byte-verify smoke test
#[test] fn scan_verify_byte_reports_same_groups_as_off() { todo!() }
```
- **Risk / follow-ups:** the CORE-12 alias test depends on `stop_after` (test-hooks); confirm the verify.rs test binary already enables it. CORE-11 tests are expected to fail until CORE-01/02/03/06 are fixed in their own topic.


---

# Part 3. Second review notes

Second review of this file by a Rust and cross-platform reviewer. Claims in DB-01, DB-02, DB-03, PIPE-03, GEN-02, GEN-03, GEN-04 and CLI-01 were re-checked against the code and hold. The items below are corrections to the file and gaps it does not cover. IDs start with REV.

### REV-01. A WAL database on a network filesystem is accepted silently

- Topic: T3 / T4 (extends T3.6 and T4.6)
- Severity: High
- Confidence: High
- Location: db/mod.rs `apply_pragmas`, cli/src/scan.rs default DB path
- Problem: T3.6 says WAL on a network filesystem is "already unsupported (`WalUnavailable`)". It is not: `WalUnavailable` is returned only when SQLite itself refuses WAL, and SQLite accepts WAL on SMB, NFS and FUSE (the `-shm` file is memory-mapped, so locking is unreliable there). The default DB path is `<first root>/.dedupe.db`, and T4.6 makes UNC and mapped roots scannable, so a WAL database lands on the share by default.
- Fix: `backend-generic::fs_locality` classifies the folder that holds the DB (Linux statfs magic, macOS `MNT_LOCAL`, Windows `GetDriveTypeW`). The scan refuses a default DB on a remote filesystem, prints a note for an explicit `--db` there, and prints a note for WSL 9p. Magic numbers come from memory and need a check against `linux/magic.h`.

### REV-02. Volume identity must be computed on the canonical path

- Topic: T4 / T5 (extends T4.6 and T5.1)
- Problem: T4.6 puts the spelling of the root into the volume uuid (`serial:{serial}:{root_lower}`), and T5.1 calls `identify` before canonicalizing. `Z:\` and `\\nas\share\` become two volumes, and `lookup Z:\x` misses an index built from `\\nas\share`.
- Fix: canonicalize first, then identify, in `lookup` as in `scan`. Two spellings of one share that canonicalize differently (an IP address versus a host name, a DFS path) still give two volumes; document it.

### REV-03. `subst` drives get a second identity

- Topic: T4
- Problem: a `subst X: C:\data` drive has no volume GUID, reports the serial of the volume behind it, and has the root key `x:\`. It is a second volume row for files that exist under the real drive too.
- Action: add to "Behaviour to verify on real hardware". Consider resolving through `QueryDosDeviceW`.

### REV-04. MFT-01 is a P1 on mechanism only

- Topic: T6
- Problem: the cost is real (record 0 is re-read per record), but no one has measured it, and benchmarks are deferred by project rule.
- Action: ship only T6.5 stage A (pinned record-0 chunk, 4 MiB stream). Gate stage B (own `$MFT` reader) on a measurement on a 1M+ record volume.

### REV-05. T2.3 rehash cost is wider than stated

- Topic: T2
- Problem: besides `chmod` and `chown`, extended-attribute writes bump ctime on Linux and macOS. Finder writes `com.apple.lastuseddate#PS` when a file is opened, and backup and antivirus tools set xattrs, so those files rehash on the next scan.
- Action: documented in the README. The fix stays.

### REV-06. Part 1 line numbers drift

- Example: GEN-02 cites `volume.rs:408-434`, but the file has 362 lines and the code is at 195-220. Use the Part 2 citations, which were re-checked.

### REV-07. The build matrix is narrower than CLAUDE.md implies

- Topic: T9
- Problem: only `x86_64-pc-windows-gnu` is cross-checked, and the setup text assumes a macOS host. Nothing compiles the macOS code from Linux, the MSVC target (bundled SQLite builds with a different C compiler), or aarch64 (Apple Silicon, Windows on ARM).
- Action: CLAUDE.md now states which targets are compile-checked from this host. `cargo-xwin` is one way to add an MSVC check from Linux.

### REV-08. Copy-on-write clones count as wasted bytes

- Topic: T9 (docs)
- Severity: Low
- Problem: APFS `clonefile`, btrfs and XFS reflinks, and ReFS block clones share extents, so `wasted_logical` overstates reclaimable space. The README now says so.
