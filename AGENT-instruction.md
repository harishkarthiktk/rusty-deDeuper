# AGENT-instruction: duplicate-file analysis workflow

For a future agent (human or AI) repeating the workflow on a new `dedupe.json`. Scripts live in `agent/` (see `agent/README.md`). Read this file top to bottom before starting.

## 1. Purpose

Turn a `dedupe.json` report (output of the deDupe CLI) into a reviewed, per-group decision list: which copy of each duplicate file to keep, and which to delete or hardlink.

The workflow does analysis and intelligent detection only:

1. Remove noise (venvs, node_modules, build output, git internals) from the report.
2. Rank what is left by reclaimable space.
3. Inspect folders read-only to judge which copy is the "home" of a file.
4. Write decisions to a file.
5. A user-approved executor (`agent/execute_plan.py`, section 9) carries out the plan; the agent never runs it with `--execute`.

**Nothing is deleted, moved or copied by the analysis scripts. Nothing changes on disk unless the user runs `execute_plan.py` with `--execute`.**

## 2. Input format

`dedupe.json` is the CLI's `--json` report.

Top-level keys:

| Key | Meaning |
|---|---|
| files_enumerated | files seen by the scan |
| files_errored | files that could not be read |
| files_skipped | files skipped (excluded, special) |
| group_count | number of duplicate groups |
| groups | array of groups |
| wasted_logical_total | sum of `wasted_logical` over all groups, bytes |

Group keys:

| Key | Meaning |
|---|---|
| distinct_inodes | distinct file identities (inode / NTFS file id) among members |
| full_hash | BLAKE3 truncated to 128 bits, 32 hex chars |
| member_count | number of paths in the group |
| members | array of `{path, volume_id}` |
| size | file size in bytes (same for all members) |
| wasted_logical | `(distinct_inodes - 1) * size`, bytes |

Facts to rely on:

- Equal `full_hash` and equal `size` means equal content. Trust the hash over file names.
- `wasted_logical = (distinct_inodes - 1) * size`. If two members are already hardlinks they share an inode and add no waste.
- Encoding: **UTF-16 LE with BOM, CRLF newlines**, pretty-printed (usually 2-space indent). Plain `open().read()` as UTF-8 fails. The scripts detect the BOM; for ad hoc reads use `open(p,'rb').read().decode('utf-16')`.
- Paths are Windows paths (`X:\A\B\file.ext`).

## 3. Phase 0: inspect input

Read-only checks before anything else. Report these to the user.

```
python - <<'EOF'
import json, sys
sys.path.insert(0, 'agent')
from jsonio import load_json
d, text, enc = load_json('PATH/TO/dedupe.json')
g = d['groups']
print('encoding', enc, 'CRLF', '\r\n' in text)
print({k: d[k] for k in d if k != 'groups'})
print('groups', len(g), 'sum wasted', sum(x['wasted_logical'] for x in g))
print('groups sharing inodes', sum(1 for x in g if x['distinct_inodes'] < x['member_count']))
print('groups wasted==0', sum(1 for x in g if x['wasted_logical'] == 0))
EOF
```

Check:

| Check | Why |
|---|---|
| Encoding and newline | the split script must write the same format back |
| `group_count == len(groups)` | detects truncated or hand-edited input |
| Sum of `wasted_logical` equals `wasted_logical_total` | sanity check on the report |
| Groups with `distinct_inodes < member_count` | members already hardlinked; merging those saves no space for the shared part |
| Volumes in `volume_id` | all members on one volume allows hardlinks; cross-volume does not |

## 4. Phase 1: classify and split

Goal: remove build and tooling artifacts so the real user data stands out.

### Dry run first

```
python agent/report.py PATH/TO/dedupe.json
```

Read the "leftover segments" list. If a common artifact folder name appears there that no rule covers (for example a new toolchain), tell the user and extend `agent/rules.py` before splitting. Do not change existing rules silently.

### Rules (implemented in `agent/rules.py`)

A member is an artifact if its path matches a rule. Rules are checked in this order; the first match gives the label. Matching works on the directory segments of the path and the lowercase file name.

| Label | Plain-language rule |
|---|---|
| python-venv | any folder is `site-packages` or `dist-packages`; or a folder name starts with `.venv` or `venv`; or a folder is called `env` or ends with `env` AND the path also has a `Lib`, `Scripts`, `Include` or `share` folder; or the file sits in a `Scripts` folder and its name starts with `python`, `pip`, `activate` or `deactivate` |
| build-toolchain | any folder name starts with `cmake-` (CMake install trees) |
| node-modules | any folder is `node_modules` |
| rust-target | any folder is `.fingerprint`; or a `target` folder together with a `debug`, `release` or `doc` folder |
| git-internals | any folder is `.git` |
| python-cache | any folder is `__pycache__` |
| compiled-artifact | file extension is one of `.pyc .pyo .o .obj .pdb .rlib .rmeta .timestamp .a .pyd` |

### Group rule

A group goes to the ignore file **only if every member** matches a rule. A mixed group (some members are artifacts, some are real files) stays in the kept file, because a real copy exists. The `reason` of an ignored group is the sorted, comma-joined set of labels of its members (a "combined" group has more than one label, for example `node-modules,python-venv`).

### Run the split

```
python agent/split.py PATH/TO/dedupe.json OUT_DIR
```

Use an `OUT_DIR` different from the input folder.

Outputs:

| File | Content |
|---|---|
| OUT_DIR/dedupe.json | kept groups; totals recomputed; same encoding as input |
| OUT_DIR/dedupe_ignore.json | ignored groups, each with `reason`; extra top-level `reason_counts` |

Built-in assertions (the script aborts if any fails): the layout is reproducible byte for byte before writing; kept plus ignored equals the original groups (lossless, ignoring the added `reason` field); kept wasted plus ignored wasted equals the original `wasted_logical_total`.

Record the printed counts in `OUT/run_record.md`.

## 5. Phase 2: rank and tabulate

```
python agent/top_locations.py OUT_DIR/dedupe.json 10 OUT_DIR
```

Choose N with the user. Output:

- `OUT_DIR/top_10_locations.md`: table with rank, location number (`1.1`, `1.2`, ... = group rank . member index), folder (without drive letter), file, size in decimal MB.
- `OUT_DIR/top_10_locations.json`: one row per location with `group`, `loc`, `path`, `folder`, `name`, `size`, `full_hash`.

Groups are ranked by `wasted_logical`, largest first. Show the markdown table to the user. Reclaimable space of a group is `(copies kept out of members - ...)`: with one copy kept it is `(member_count - 1) * size`.

## 6. Phase 3: folder exploration (read-only)

Goal: for each location in the top N, learn what the folder looks like, so Phase 5 can pick the home copy.

### Permission first

Ask the user once before reading outside the working directory. The **parent agent asks once**, naming the drives and what will be read (directory listings and small text files). Subagents do the listing and cannot ask the user, so they start only after the user said yes.

### Path translation

Inputs are Windows paths. On native Windows use them as is. On WSL, `X:\a` maps to `$DEDUPE_MOUNT_ROOT/x/a` (default `/mnt`; drive letter lowercase, backslash to slash). The PowerShell fallback below is WSL-only, for a missing or slow mount:

```
powershell.exe -NoProfile -Command "Get-ChildItem -LiteralPath 'X:\A\B' | Select-Object Name,Length,LastWriteTime"
```

Use `-LiteralPath` so brackets and special characters in names are not treated as wildcards. Escape single quotes in names by doubling them.

### What may be read

| Allowed | Not allowed |
|---|---|
| Directory listings: names, sizes, modified times | Opening, previewing, hashing or copying media files |
| Small text sidecars under 64 KB: `.txt .nfo .json .md .srt .csv .url .html .log` | Any other file type |
| Parent folders of the folder (listing only) | Writing, relocating, renaming, deleting anything |

### What to collect per folder

- Entry names with sizes and mtimes (counts of files and subfolders).
- Whether the folder holds a complete numbered set (Img1..ImgN, no gaps) or only a partial set.
- Names that hint at processing (`_stab`, `upscaled`, `trimmed`, `edited`, `shrunk`).
- Sidecar text that names a source or an owner.
- The names of the parent and grandparent folders (they often say whether the place is organized or scratch).

Subagent reports contain metadata only (names, sizes, mtimes), never content. Say so in the final report. Inspect parent folders too; skipping them weakens decisions.

## 7. Phase 4: sibling cross-check

A **unique** file is one that appears in no group in either JSON file. Deleting a folder that holds unique files destroys the only copy.

Build the folder list (JSON array of Windows paths) from the folders you plan to remove entirely, then:

```
python agent/sibling_check.py OUT_DIR/dedupe.json OUT_DIR/dedupe_ignore.json folders.json listing.txt
```

`listing.txt` is the full file listing of those folders (one path per line, from the Phase 3 listing). Without it the script only counts group members and says unique files cannot be confirmed.

Rules:

- Never delete a whole folder unless every file in it is a confirmed duplicate elsewhere (same `full_hash` in the JSON, with a surviving copy) or the user has explicitly accepted losing the unique files.
- A file counted in a group is only safe if at least one other member of that group survives.
- Folder comparison is case-insensitive (the script does this).
- The script checks direct children only. Subfolders must be listed separately.
- A folder with 5 of 6 files in groups is a trap: the 6th is unique. If one file in a scratch folder is in no group, that folder must be kept (or the file preserved) even though its other files were duplicates.

## 8. Phase 5: decide keeps

Apply these principles in priority order. Earlier ones win ties.

1. **Keep the copy in a folder that holds a complete set or the organized home of the file.** A numbered series in one folder beats the same files scattered.
2. **Penalize scratch markers.** `New Folder`, names that repeat their parent (`X\X`, a typical extraction sign), `Temp`, extraction-looking folders, and partial sets (missing numbered items).
3. **A derivative marks the processing folder.** Names like `_stab`, `upscaled`, `trimmed`, `edited` mean the processing happened in that folder. Keep that folder's copy.
4. **Name and content can disagree. Trust the hash.** A file named `upscaled` that is byte-identical to another copy (same `full_hash`) is not an upscaled version.
5. **Use hardlinks when two paths each have a legitimate reason to exist.** On NTFS a hardlink frees the same space as deleting but keeps both paths.
   - Check first: both paths on the same NTFS volume (`fsutil fsinfo volumeinfo X:` or `Get-Volume`). exFAT, FAT32 and network shares do not support hardlinks.
   - Warn the user: an edit to one copy changes all of them (they are one file).
   - A hardlinked group shows `distinct_inodes < member_count` in a later scan.
6. **Modified-time lineage is inference only.** A 2025 mtime on one copy may mean it is the original, but it is unverified; mtimes change on copy and extraction. State this when used.
7. **Always report confidence** with the reason:

| Confidence | When |
|---|---|
| high | one clear scratch copy (partial set, extraction folder, shrunk copy) versus one clear home; parents inspected |
| medium | both candidates plausible, one slightly better organized; or only mtime or size evidence |
| low | candidates look equal, or parent folders not inspected, or content disagrees with names |

Present decisions per group to the user with: rank, hash, keep path, delete or hardlink paths, action, reason, confidence. Let the user override any of them.

### Decisions file

Write `OUT_DIR/decisions.json` (UTF-8, plain JSON array). One object per group:

```json
[
  {
    "group_rank": 1,
    "full_hash": "0123456789abcdef0123456789abcdef",
    "size": 1775347442,
    "keep": "X:\\ROOT\\FOLDER_A\\SUB\\file.ext",
    "delete": [],
    "hardlink": ["X:\\ROOT\\FOLDER_B\\SUB\\file.ext"],
    "action": "hardlink",
    "reason": "folder A is the organized home; the copy in folder B is a second legitimate location",
    "confidence": "medium"
  }
]
```

| Field | Notes |
|---|---|
| group_rank | rank in the top-N table |
| full_hash | 32 hex chars; links the entry to the group in the JSON |
| size | bytes; used for re-checks |
| keep | one full Windows path; must be a member of the group |
| delete | paths to delete (may be empty) |
| hardlink | paths to replace by a hardlink to `keep` (may be empty) |
| action | `delete`, `hardlink` or `skip` |
| reason | one sentence, includes the evidence |
| confidence | `high`, `medium` or `low` |

Only entries the user approved go in the file the deletion script reads. Keep undecided groups out, or mark `"action": "skip"`.

## 8b. Phase 5b: Reconciliation (folder-level consolidation)

Goal: where whole folders hold duplicates of each other, consolidate into one common folder, then delete the duplicate items in the other folders. Single-file keeps (Phase 5) are the special case of a cluster with one group.

1. **Cluster.** Folders that are members of the same duplicate group(s) form a cluster (folders connected by shared groups). Run on the kept file; with `-` as the inventory it only writes the cluster list, which is the folder list for step 2:
   `python agent/reconcile_plan.py OUT/dedupe.json OUT/dedupe_ignore.json - OUT/clusters.json`
2. **Inventory.** Full recursive listing per folder (name, kind, size, mtime), saved as `OUT/folder_inventory.json`. Made by a read-only listing step, which needs the user's approval, same as Phase 3 (section 6). Schema:
   `{"folders": [{"index", "windows_path", "status": "ok"|"missing", "entries": [{"rel_path", "kind": "file"|"dir", "size", "mtime"}]}]}`
3. **Classify** every file in the cluster:

| Class | Test | Handling |
|---|---|---|
| duplicate | its full hash is in a group (either JSON) | pick one canonical copy per group |
| unique | in no group in either JSON | preserved: moved, never deleted |
| name conflict | same name as an existing target, different content | rename, never overwrite |
| unknown | same name and same size, not both in a group | needs hashing, which reads media: ask the user first |

   Different sizes prove different content. Equal size never proves sameness.
3b. **Parent scope check.** A cluster folder with a generic or segregation-style name (`vid`, `vids`, `video`, `videos`, `upscale`, `upscaled`, `upload`, `uploads`, `pic`, `pics`, `image`, `images`, `clip`, `clips`, `bg`, `solo`, `out`, `output`, `new`, `copy`, `backup`, `part N`, or similar) may be one subfolder of a container that belongs to the same set. Check its parent with a **one-level listing** (read-only, same approval as section 6). The parent is a container of this set only if both hold:
   - (a) it holds no files and no folders unrelated to this set;
   - (b) its children are segregation subfolders of this set (for example a media folder and a picture folder under one collection folder).

   If the parent holds unrelated siblings it is not a container: do not include it. Examples: a collection folder holding only a picture folder and a video folder, the video folder holding only two subfolders, is a container, include. A parent holding 40 unrelated folders is not a container, exclude.
   - Record the decision and its evidence in `decisions.json`, one object per checked folder: `{"type": "parent_scope_check", "cluster_id", "folder", "parent", "container": true|false, "evidence": "what the one-level listing showed"}`.
   - If the parent IS a container: add it to the cluster with the `overrides.json` key `"add_folders"`, `{"add_folders": {"<cluster_id>": ["X:\\PARENT"]}}`, and inventory the parent recursively (one listing, same schema as step 2). The parent's whole tree then belongs to the cluster.
4. **Common folder.** The folder with the complete set or the organized home, by the Phase 5 principles. The script only suggests one (most files held, fewest scratch markers); override with an `overrides.json` key per cluster id: `{"<cluster_id>": "X:\\path"}` (the same file may also hold `add_folders`, step 3b). The common folder can be a container: a copy anywhere under its tree, subfolders included, counts as the kept copy. Create a new folder only if no existing folder fits, and only with user approval.
5. **Moves.** Every unique file goes to the common folder **root** (plus its path relative to its own source folder), never into an existing subfolder of the common tree. If a file belongs in a subfolder, move it by hand, or set the common folder to that subfolder. On a name conflict, rename it with a source suffix: `<stem>__<source-folder-slug>.<ext>`, slug = last two folder names joined by `-`, spaces as `_`, e.g. `item5__FOLDER-SUB.ext`. Never overwrite.
6. **Deletes and held items.** Every duplicate whose full hash matches a copy under the common folder tree (or the canonical copy moved there) is deleted. Record the matching kept path in `matches`. Two cases are HELD (listed in `unresolved`, not moved, not deleted):
   - a duplicate whose hash has a copy outside the cluster and no copy under the common tree;
   - duplicates inside the common tree: one entry per hash, with the paths. The user decides whether to dedupe inside the common folder; report the count.
7. **Folder removal.** Only if the folder is empty after the moves and deletes. Folders under the common tree are never removed; they may be left empty, so report them. `desktop.ini` and `Thumbs.db` are never dropped silently: they are listed in `system_files` and the status is `needs_user_ok`. Statuses: `removable`, `needs_user_ok`, `blocked` (with `blockers`).
8. **Output.** Run:
   `python agent/reconcile_plan.py OUT/dedupe.json OUT/dedupe_ignore.json OUT/folder_inventory.json OUT/reconcile_plan.json [overrides.json] [--dry-run]`
   Clusters with no inventoried folder are skipped and counted in the output ("N clusters skipped: no folder in the inventory"). One entry per cluster: `{"cluster_id", "folders", "common_folder", "moves": [{"from","to"}], "renames": [{"from","to","reason"}], "deletes": [{"path","matches","size"}], "folder_removals": [...], "conflicts": [...], "unresolved": [...]}`. Anything in `unresolved` needs a human or more data before any action.
9. **Gates.** Planning is read-only (the script reads JSON and writes only the plan file). Moves, renames, folder creation, deletes and folder removal happen only in the user-run script (section 9), which defaults to dry-run and needs `--execute`. Before executing it re-checks each path's existence and size (section 9).

**`--dry-run`** (validate before trusting the plan; the `out.json` argument is still required but no file is written):

- Prints every planned action (MOVE, RENAME, DELETE with its matching copy, RMDIR with its status, HELD for unresolved items) grouped by cluster, then a SUMMARY (deletes, moves, held, bytes freed).
- Validates the plan and prints each PROBLEM: two actions writing to one target, a path deleted twice, a file both moved and deleted, a matching copy that is neither in the common folder nor a planned target, a folder with status `blocked`. Conflicts are not problems in themselves: they are resolved by renames.
- Exit code 1 if any problem was found, 0 otherwise. Fix the cause (overrides, listing, user decision) and rerun until it reports 0 problems.
- Prints every removal, including subfolders (`RMDIR ... (subfolder, deepest first)`), then the folder with its status.
- Folders inside an inventoried folder (for example a picture subfolder) are covered by the recursive listing and are not flagged.
- `desktop.ini` and `thumbs.db` are never planned as deletes: their folder gets status `needs_user_ok` and is kept.
- It needs a real inventory (not `-`).

Gotchas for this phase:

- Sizes only prove difference, never sameness.
- Folder names can differ only in case (`folder` vs `Folder`); compare case-insensitively.
- Mtime lineage is inference only.
- A file can be duplicated outside the cluster, so the cluster plan does not cover it (for example a common-folder file with a copy elsewhere: the common-folder copy is kept and the outside copy is out of scope).
- `desktop.ini` and `thumbs.db` can be Windows folder settings even when they are hash-matched duplicates. The planner never plans them as deletes; their folder gets status `needs_user_ok` and is kept.
- Inspect parent folders before deleting a folder; a parent can hold other content.
- The script lists nested cluster folders deepest first and counts only files a folder owns; re-run after the first removals if folders are nested.

## 9. Phase 6: executor (agent/execute_plan.py)

Implemented. It executes a `reconcile_plan.json` (section 8b). It never runs by itself: the user runs it, or approves the exact command first.

```
python agent/execute_plan.py OUT/reconcile_plan.json                        # dry run (default)
python agent/execute_plan.py OUT/reconcile_plan.json --execute --quarantine X:\QUARANTINE --log OUT/exec.jsonl
```

| Option | Meaning |
|---|---|
| (none) | Dry run: prechecks run, every action is printed as `PLAN`, nothing on disk changes, no log is written |
| `--execute` | Perform the actions. Without it nothing is changed |
| `--quarantine DIR` | Instead of deleting, move each duplicate under `DIR`, keeping the folder tree below the drive letter (`X:\ROOT\A\f.ext` goes to `DIR\ROOT\A\f.ext`). Nothing is lost. `DIR` must be on the same volume (it uses a rename) |
| `--log FILE` | JSON lines log. Default `<plan>.exec.jsonl`. Written only with `--execute`, appended if it exists |

Paths follow the rule in section 6 (Path translation).

### Order per cluster

1. Renames and moves.
2. Deletes (or quarantine moves).
3. Folder removals, deepest first.

Clusters without a common folder are skipped.

### Prechecks (run for every cluster, also in the dry run)

| Check | Failure means |
|---|---|
| Source of every move or rename exists | missing source |
| Target of every move or rename does not exist | target exists (never overwrite) |
| Size of every delete equals the size in the plan | size changed since the plan |
| Matching copy of every delete exists (on disk, or is a planned target) | matching copy missing |
| No file is both moved and deleted | conflicting actions |

**A failed precheck skips the whole cluster** (each problem is printed and logged with result `skipped`). Other clusters still run.

### Folder removal

- Only folders with status `removable` are touched. Subfolders listed in `dirs_to_remove` go first (deepest first), then the folder.
- Removal uses `os.rmdir` only, so a folder that still holds anything is never removed (the call fails instead).
- `needs_user_ok` and `blocked` folders are kept and reported as `keep-folder` with the status.

### Failure behaviour

On an OS error the cluster stops at that action and logs `failed`; the log shows what already ran. There is **no automatic rollback**. Recover by hand from the log (a quarantine run makes this safe: the files are still under `DIR`).

### Log fields

One JSON object per line: `time`, `cluster`, `action` (`rename`, `delete`, `quarantine`, `rmdir`, `keep-folder`, `precheck`, `error`), `path`, `target`, `result` (`ok`, `skipped`, `failed`), `detail`.

### Limits

- It checks **size**, not content hash. The hash match was established at scan time by `dedupe.json`. If files may have changed since the scan, rescan first.
- It executes reconcile plans only. Hardlink decisions from `decisions.json` (section 8) are not executed; the user does those by hand.

### Verification afterwards (read-only)

- Every `matches` path of the plan still exists.
- Every deleted or quarantined path is gone from its old location (and present under `DIR` if quarantined).
- Every move and rename target exists.
- Every `removable` folder is gone; every `keep-folder` folder is still there.
- Rescan (section 11) and compare group counts.

### Approval

The agent never runs `--execute`. Any run that touches the drive, the dry run included (it stats files), needs the user to approve the exact command line first. Show the command, the plan summary (deletes, moves, bytes) and whether `--quarantine` is used.

## 10. Gotchas

- Mixed-case folder names exist (`folder` vs `Folder`). Windows is case-insensitive: compare folder paths case-insensitively when grouping or checking.
- Subagent reports were size and mtime only, not content. Do not claim a content comparison beyond the hash.
- Parent folders of a folder are easy to skip; inspect them, it changes confidence.
- Parents of cluster folders are not inventoried and are never removed by the scripts.
- `split.py` writes new files only; the original is never modified. Keep the original JSON.
- If `split.py` fails with "layout not reproducible", the input was not written by the usual serializer (different indent or newlines, or a trailing field). Inspect with Phase 0 before forcing anything.
- `dedupe.json` is stale after any deletion or move. Refresh it with a rescan before planning again.
- The common-folder choice in `reconcile_plan.py` (most files held, fewest scratch markers) can differ from a manual choice. Use `overrides.json` to force it.
- A cluster folder with a generic name may be one part of a bigger container; do the parent scope check (section 8b, step 3b) before planning.
- Renamed files follow `stem__source-folder.ext`.
- Hash equality is for the full file. File names such as `upscaled` are not evidence of content.

## 11. How to repeat with a new JSON

Set `IN` to the new dedupe.json and `OUT` to a fresh folder (not the input's folder). The scripts resolve their imports from their own folder, so run them from any directory; every path is passed as an argument (the `agent/` prefix below assumes the repo root). Order:

| Step | Command | Output |
|---|---|---|
| 0 | Phase 0 snippet (section 3), `python agent/report.py IN` | printed checks, dry run |
| 1 | `python agent/split.py IN OUT` | `OUT/dedupe.json`, `OUT/dedupe_ignore.json` |
| 2 | `python agent/top_locations.py OUT/dedupe.json 10 OUT` | `OUT/top_10_locations.md`, `OUT/top_10_locations.json` |
| 3 | `python agent/reconcile_plan.py OUT/dedupe.json OUT/dedupe_ignore.json - OUT/clusters.json`, then ask the user and run the read-only recursive listing (sections 6 and 8b) | `OUT/clusters.json`, `OUT/folder_inventory.json` |
| 4 | `python agent/sibling_check.py OUT/dedupe.json OUT/dedupe_ignore.json folders.json listing.txt` | per-folder counts and UNIQUE files |
| 5 | `python agent/reconcile_plan.py OUT/dedupe.json OUT/dedupe_ignore.json OUT/folder_inventory.json OUT/reconcile_plan.json --dry-run` | printed actions; **must report 0 problems** (exit 0) |
| 6 | Same command without `--dry-run` | `OUT/reconcile_plan.json` |
| 7 | `python agent/execute_plan.py OUT/reconcile_plan.json` (dry run) | printed `PLAN` lines, precheck results |
| 8 | User approves the exact `--execute` command (section 9) | approval |
| 9 | `python agent/execute_plan.py OUT/reconcile_plan.json --execute --quarantine DIR --log OUT/exec.jsonl` (user-approved) | `OUT/exec.jsonl` |
| 10 | Read-only verification (section 9) | checks |
| 11 | Rescan with the CLI, then write a run summary to `OUT/run_record.md` | new `dedupe.json` |

Apply Phase 5 (section 8) while preparing step 3 to 5: the principles decide the common folder, and `overrides.json` enforces a manual choice.

Order matters: split before ranking (so artifacts do not crowd the top N), cross-check before planning, a clean `--dry-run` before the plan file is trusted, user approval before any run touches the drive.

Checklist before handing over:

- [ ] `OUT/dedupe.json` plus `OUT/dedupe_ignore.json` counts match the original (script assertions passed).
- [ ] User confirmed shell reads outside the project.
- [ ] Every folder slated for full removal passed the sibling check, with a listing.
- [ ] Parent scope check done for every cluster folder with a generic name; decision and evidence recorded.
- [ ] `reconcile_plan.py --dry-run` reported 0 problems.
- [ ] Every decision has a confidence and reason.
- [ ] No file was opened other than listings and small text sidecars.
- [ ] No deletion, move or copy was performed by the agent.
