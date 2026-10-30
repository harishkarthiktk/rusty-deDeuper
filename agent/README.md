# agent/ - duplicate analysis helpers

Analysis scripts for a `dedupe.json` produced by the dedupe CLI. All are read-only except `execute_plan.py`, which changes the drive only with `--execute` after the user approves the exact command. Full workflow: `../AGENT-instruction.md`.

By default, `dedupe scan` skips most of what `rules.py` classifies (venv `site-packages`, `node_modules`, Rust `target`, `.git`, compiled objects) at scan time (`--include-dev-artifacts` turns this off). `split.py` still catches the rest, such as `cmake-*` installs and venv `Scripts` launchers.

Files: `rules.py` (artifact classifier, imported by the others), `jsonio.py` (BOM-aware loading, imported by the others).

Scripts resolve their imports from their own folder, so run them from any directory; every path is passed as an argument. Use `python` (3.8+), standard library only. Replace `RUN` with your working folder for outputs. On WSL, Windows paths map to `$DEDUPE_MOUNT_ROOT/<drive>/...` (env var, default `/mnt`).

## 1. report.py - dry run

```
python report.py /path/to/dedupe.json
```

Prints group counts per artifact reason, wasted bytes (ignored vs kept), leftover path segments in kept groups and a few mixed-group samples. Writes nothing. Use it to check that the rules fit the new data before splitting.

## 2. split.py - split kept and ignored

```
python split.py /path/to/dedupe.json RUN
```

Writes `RUN/dedupe.json` (kept) and `RUN/dedupe_ignore.json` (artifact groups, each with `reason`, plus a `reason_counts` summary). Same encoding, indent and CRLF as the input. Asserts the split is lossless and wasted totals add up. Do not point `RUN` at the folder holding the input, or the input is overwritten.

## 3. top_locations.py - rank and tabulate

```
python top_locations.py RUN/dedupe.json 10 RUN
```

Writes `RUN/top_10_locations.md` (rank, location number, folder, file, size MB) and `RUN/top_10_locations.json`.

## 4. sibling_check.py - folder cross-check

```
python sibling_check.py RUN/dedupe.json RUN/dedupe_ignore.json folders.json
python sibling_check.py RUN/dedupe.json RUN/dedupe_ignore.json folders.json listing.txt
```

`folders.json` is a JSON array of Windows folder paths. Prints, per folder, how many files sit in any duplicate group (kept or ignore file). With the optional `listing.txt` (one full file path per line, or a JSON array) it also prints files in no group as `UNIQUE`. Without a listing, unique files cannot be confirmed.

## 5. reconcile_plan.py - folder-level plan

```
python reconcile_plan.py RUN/dedupe.json RUN/dedupe_ignore.json - RUN/clusters.json
python reconcile_plan.py RUN/dedupe.json RUN/dedupe_ignore.json RUN/folder_inventory.json RUN/reconcile_plan.json [overrides.json] [--dry-run]
```

First form lists clusters (folders linked by shared groups) for the listing step. Second form needs `folder_inventory.json` and writes a plan per cluster: moves, renames, deletes, folder removals, conflicts, unresolved. `overrides.json` holds `{"<cluster_id>": "X:\\common"}` to force a common folder and `{"add_folders": {"<cluster_id>": ["X:\\PARENT"]}}` to add a container parent (inventory it recursively first). A copy anywhere under the common tree counts as kept; unique files move to the common folder root; duplicates inside the common tree and duplicates with a copy outside the cluster are held, not touched; folders under the common tree are never removed. `--dry-run` prints every action grouped by cluster, including subfolder removals (deepest first), validates the plan (two actions on one target, a file both moved and deleted, a matching copy outside the common folder, a blocked folder), writes no file and exits 1 on problems. Clusters with no inventoried folder are skipped and counted. `desktop.ini` and `thumbs.db` are never planned as deletes. Read-only apart from the output file; nothing is executed. Schemas: `../AGENT-instruction.md` section 8b.

## 6. execute_plan.py - executor (user-approved only)

```
python execute_plan.py RUN/reconcile_plan.json
python execute_plan.py RUN/reconcile_plan.json --execute --quarantine X:\QUARANTINE --log RUN/exec.jsonl
```

First form is a dry run: prechecks and a printed list of actions, no changes. `--execute` performs renames and moves, then deletes, then folder removals (deepest first, `os.rmdir` only, only status `removable`). `--quarantine DIR` moves duplicates under `DIR` (same volume, folder tree kept) instead of deleting. `--log` defaults to `<plan>.exec.jsonl`, written only with `--execute`. A failed precheck (missing source, target exists, size changed, matching copy missing, file both moved and deleted) skips the whole cluster; an error during a run stops the cluster, with no rollback. It checks size, not hash. Details: `../AGENT-instruction.md` section 9.
