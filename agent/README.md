# agent/ - duplicate analysis helpers

Analysis scripts for a `dedupe.json` produced by the dedupe CLI. All are read-only except `execute_plan.py`, which changes the drive only with `--execute` after the user approves the exact command. Full workflow: `../AGENT-instruction.md`.

By default, `dedupe scan` skips most of what `rules.py` classifies (venv `site-packages`, `node_modules`, Rust `target`, `.git`, compiled objects) at scan time (`--include-dev-artifacts` turns this off). `split.py` still catches the rest, such as `cmake-*` installs and venv `Scripts` launchers. Rules match folder names case-insensitively.

Files: `rules.py` (artifact classifier, imported by the others), `jsonio.py` (BOM-aware loading and path styles, imported by the others), `examples/` (reference `dedupe.json` files, Windows and POSIX, made-up data).

Scripts resolve their imports from their own folder, so run them from any directory; every path is passed as an argument. Use `python` (3.8+), standard library only. Replace `RUN` with your working folder for outputs.

Paths: a report holds Windows paths (`C:\...`, `\\?\C:\...`, UNC) from a Windows build, or POSIX paths (`/mnt/c/...`, `/home/...`) from a Linux or macOS build, never both. The scripts detect the style, keep it in their output, compare Windows paths case-insensitively and POSIX paths exactly. On WSL, Windows paths map to `$DEDUPE_MOUNT_ROOT/<drive>/...` (env var, default `/mnt`); overrides, inventory and quarantine paths may use either style. Files with non-UTF-8 names (`path_bytes`) are held, never acted on. Details: `../AGENT-instruction.md` section 2.

## 1. report.py - dry run

```
python report.py /path/to/dedupe.json
```

Prints group counts per artifact reason, wasted bytes (ignored vs kept), leftover path segments in kept groups and a few mixed-group samples. Writes nothing. Use it to check that the rules fit the new data before splitting.

## 2. split.py - split kept and ignored

```
python split.py /path/to/dedupe.json RUN
```

Writes `RUN/dedupe.json` (kept) and `RUN/dedupe_ignore.json` (artifact groups, each with `reason`, plus a `reason_counts` summary). Same encoding, indent and CRLF as the input. Checks (not `assert`, so `python -O` keeps them) that `group_count` matches, the split is lossless and wasted totals add up. Refuses a `RUN` that is the input's folder.

## 3. top_locations.py - rank and tabulate

```
python top_locations.py RUN/dedupe.json 10 RUN
```

Writes `RUN/top_10_locations.md` (rank, location number, folder, file, size MB) and `RUN/top_10_locations.json`.

## 4. reconcile_plan.py - folder-level plan

```
python reconcile_plan.py RUN/dedupe.json RUN/dedupe_ignore.json - RUN/clusters.json
python reconcile_plan.py RUN/dedupe.json RUN/dedupe_ignore.json RUN/folder_inventory.json RUN/reconcile_plan.json [overrides.json] [--dry-run]
```

First form lists clusters (folders linked by shared groups) for the listing step. Second form needs `folder_inventory.json` and writes a plan per cluster: moves, renames, deletes, folder removals, conflicts, unresolved. `overrides.json` holds `{"<cluster_id>": "X:\\common"}` to force a common folder and `{"add_folders": {"<cluster_id>": ["X:\\PARENT"]}}` to add a container parent (inventory it recursively first). A copy anywhere under the common tree counts as kept, and files under the common tree stay put; unique files from other folders move to the common folder root; duplicates inside the common tree and duplicates with a copy outside the cluster are held, not touched; folders under the common tree are never removed, and a folder that contains the common folder is blocked. `--dry-run` prints every action grouped by cluster, validates the plan (two actions on one target, a file both moved and deleted, a matching copy outside the common folder or itself deleted, an ignored override, a blocked folder), writes no file and exits 1 on problems. Clusters with no inventoried folder are skipped and counted. `desktop.ini` and `thumbs.db` are never planned as deletes. Read-only apart from the output file; nothing is executed. Schemas: `../AGENT-instruction.md` section 8.

## 5. execute_plan.py - executor (user-approved only)

```
python execute_plan.py RUN/reconcile_plan.json
python execute_plan.py RUN/reconcile_plan.json --execute --quarantine X:\QUARANTINE --log RUN/exec.jsonl
python execute_plan.py RUN/reconcile_plan.json --execute --delete --log RUN/exec.jsonl
```

First form is a dry run: prechecks and a printed list of actions, no changes. `--execute` needs `--quarantine DIR` (recommended) or `--delete`. It performs renames and moves (creates missing folders, never overwrites), then deletes, then folder removals (deepest first, `os.rmdir` only, only status `removable`). `--quarantine DIR` moves duplicates under `DIR` keeping the drive letter (`X:\A\f` to `DIR\X\A\f`, `/mnt/c/f` to `DIR/mnt/c/f`); `DIR` must exist on the same volume. Before each delete the file is compared byte for byte with its matching copy; a mismatch skips that file. `--log` defaults to `<plan>.exec.jsonl`, written only with `--execute`. A failed precheck (missing source, target exists, size changed, matching copy missing or resized, a path that another delete keeps as its copy, quarantine collision or other volume) skips the whole cluster; an error during a run stops the cluster, with no rollback. Exit code 1 when anything was skipped or failed. Details: `../AGENT-instruction.md` section 9.
