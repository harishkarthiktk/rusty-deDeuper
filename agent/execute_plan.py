"""Execute a reconcile plan. Dry-run by default: nothing on disk changes unless --execute is given.

Usage: python execute_plan.py <plan.json> [--execute (--quarantine <dir> | --delete)] [--log <file.jsonl>]
  plan.json        output of reconcile_plan.py (JSON list of cluster records)
  --execute        perform the renames, moves, deletes and folder removals
  --quarantine D   move each duplicate under D instead of deleting it (D must be on the same volume).
                   X:\\ROOT\\f goes to D\\X\\ROOT\\f; /mnt/c/f goes to D/mnt/c/f
  --delete         delete duplicates for good (needed with --execute when --quarantine is not given)
  --log F          JSON lines log of every action (default: <plan>.exec.jsonl, written only with --execute)
Order per cluster: renames and moves, then deletes, then folder removals (deepest first).
Safety: a cluster is checked first. Any missing source, existing target, changed size, missing
matching copy, or a path that another delete keeps as its copy skips the whole cluster. Right before
each delete the file is compared byte for byte with its matching copy; a mismatch skips that file.
Folders are removed with os.rmdir, so a non-empty folder is never removed.
Paths: Windows paths map to $DEDUPE_MOUNT_ROOT/<drive>/... on WSL (default /mnt); POSIX paths are used as is.
"""
import argparse
import filecmp
import json
import os
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonio import key, path_style, strip_verbatim, to_local, to_style  # noqa: E402


def k(p):
    return key(p, path_style(p))


def quarantine_path(qdir, p):
    """Target under qdir that keeps the drive letter (or the full POSIX path), so volumes never collide."""
    style = path_style(p)
    q = to_style(qdir, style).rstrip('\\/')
    if style == 'posix':
        return q + p
    p = strip_verbatim(p)
    if len(p) < 3 or p[1] != ':':
        raise ValueError(f'quarantine supports drive paths only: {p}')
    return q + '\\' + p[0].upper() + p[2:]


def protected_paths(plan):
    """Keys of every matching copy in the whole plan: none of them may be deleted or moved."""
    return {k(d['matches']) for c in plan for d in c['deletes']}


def check_cluster(c, protected, quarantine):
    """Return a list of problems. Empty list means the cluster can run."""
    problems = []
    moves = c['renames'] + c['moves']
    sources = {k(m['from']) for m in moves}
    targets = set()
    for m in moves:
        if k(m['to']) in targets:
            problems.append(f"two actions write to {m['to']}")
        targets.add(k(m['to']))
        if k(m['from']) in protected:
            problems.append(f"moves a file another delete keeps as its copy {m['from']}")
        if not os.path.isfile(to_local(m['from'])):
            problems.append(f"missing source {m['from']}")
        if os.path.exists(to_local(m['to'])):
            problems.append(f"target already exists {m['to']}")
    seen = set()
    for d in c['deletes']:
        if k(d['path']) in seen:
            problems.append(f"deleted twice {d['path']}")
        seen.add(k(d['path']))
        src = to_local(d['path'])
        if not os.path.isfile(src):
            problems.append(f"missing file {d['path']}")
            continue
        if os.path.getsize(src) != d['size']:
            problems.append(f"size changed since the plan {d['path']}")
        if k(d['path']) in sources:
            problems.append(f"file is both moved and deleted {d['path']}")
        if k(d['path']) in protected:
            problems.append(f"deletes a file another delete keeps as its copy {d['path']}")
        match = to_local(d['matches'])
        if k(d['matches']) not in targets:
            if not os.path.isfile(match):
                problems.append(f"matching copy missing {d['matches']}")
            elif os.path.getsize(match) != d['size']:
                problems.append(f"matching copy has a different size {d['matches']}")
        if quarantine:
            dst = to_local(quarantine_path(quarantine, d['path']))
            if os.path.exists(dst):
                problems.append(f"quarantine target already exists {dst}")
            if os.stat(src).st_dev != os.stat(to_local(quarantine)).st_dev:
                problems.append(f"quarantine is on another volume than {d['path']}")
    return problems


def safe_move(src, dst):
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    # shortcut: exists-then-rename leaves a tiny window on POSIX, where rename overwrites; fine for one user
    if os.path.exists(dst):
        raise FileExistsError(dst)
    os.rename(src, dst)


def run_cluster(c, quarantine, record):
    cid = c['cluster_id']
    for m in c['renames'] + c['moves']:
        safe_move(to_local(m['from']), to_local(m['to']))
        record(cid, 'rename', m['from'], m['to'])
    for d in c['deletes']:
        src, match = to_local(d['path']), to_local(d['matches'])
        if not os.path.isfile(match) or not filecmp.cmp(src, match, shallow=False):
            record(cid, 'delete', d['path'], d['matches'], result='skipped',
                   detail='matching copy missing or content differs')
            continue
        if quarantine:
            dst = quarantine_path(quarantine, d['path'])
            safe_move(src, to_local(dst))
            record(cid, 'quarantine', d['path'], dst)
        else:
            os.remove(src)
            record(cid, 'delete', d['path'])
    for fr in c['folder_removals']:
        if fr['status'] != 'removable':
            record(cid, 'keep-folder', fr['folder'], detail=fr['status'])
            continue
        sep = '\\' if path_style(fr['folder']) == 'win' else '/'
        for sub in fr['dirs_to_remove']:  # already deepest first
            os.rmdir(to_local(fr['folder'] + sep + sub))
            record(cid, 'rmdir', fr['folder'] + sep + sub)
        os.rmdir(to_local(fr['folder']))
        record(cid, 'rmdir', fr['folder'])


def run_cluster_plan(c, quarantine, record):
    """Dry-run output: list the actions without touching disk."""
    cid = c['cluster_id']
    for m in c['renames'] + c['moves']:
        record(cid, 'rename', m['from'], m['to'])
    for d in c['deletes']:
        if quarantine:
            record(cid, 'quarantine', d['path'], quarantine_path(quarantine, d['path']))
        else:
            record(cid, 'delete', d['path'])
    for fr in c['folder_removals']:
        if fr['status'] != 'removable':
            record(cid, 'keep-folder', fr['folder'], detail=fr['status'])
            continue
        sep = '\\' if path_style(fr['folder']) == 'win' else '/'
        for sub in fr['dirs_to_remove']:
            record(cid, 'rmdir', fr['folder'] + sep + sub)
        record(cid, 'rmdir', fr['folder'])


def main(argv):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument('plan')
    ap.add_argument('--execute', action='store_true')
    ap.add_argument('--quarantine')
    ap.add_argument('--delete', action='store_true')
    ap.add_argument('--log')
    a = ap.parse_args(argv)
    if a.quarantine and a.delete:
        sys.exit('use --quarantine or --delete, not both')
    if a.execute and not (a.quarantine or a.delete):
        sys.exit('--execute needs --quarantine DIR (recommended) or --delete')
    if a.quarantine:
        try:
            if not os.path.isdir(to_local(a.quarantine)):
                sys.exit(f'quarantine folder does not exist: {a.quarantine}')
        except ValueError as e:
            sys.exit(f'bad quarantine folder: {e}')

    with open(a.plan, encoding='utf-8') as f:
        plan = json.load(f)
    log = open(a.log or a.plan + '.exec.jsonl', 'a', encoding='utf-8') if a.execute else None

    bad = []  # any skipped or failed row makes the exit code 1

    def record(cid, action, path, target='', result='ok', detail=''):
        if result != 'ok':
            bad.append(cid)
        if log:
            log.write(json.dumps({'time': time.strftime('%Y-%m-%d %H:%M:%S'), 'cluster': cid, 'action': action,
                                  'path': path, 'target': target, 'result': result, 'detail': detail},
                                 ensure_ascii=False) + '\n')
            log.flush()
        tag = result.upper() if a.execute or result != 'ok' else 'PLAN'
        print(f'[{cid}] {tag:9} {action:11} {path}' + (f' -> {target}' if target else '') +
              (f'  ({detail})' if detail else ''))

    if not a.execute:
        print('dry run: each cluster is checked against the disk as it is now; '
              'earlier clusters can change the result at execute time')
    protected = protected_paths(plan)
    for c in plan:
        cid = c['cluster_id']
        if not c['common_folder']:
            continue
        try:
            problems = check_cluster(c, protected, a.quarantine)
        except (OSError, ValueError) as e:
            problems = [f'precheck failed: {e}']
        if problems:
            for p in problems:
                record(cid, 'precheck', p, result='skipped')
            print(f'[{cid}] cluster skipped: {len(problems)} precheck problem(s)')
            continue
        if not a.execute:
            run_cluster_plan(c, a.quarantine, record)
            continue
        try:
            run_cluster(c, a.quarantine, record)
        except Exception as e:  # noqa: BLE001 - any failure must leave a 'failed' row in the log
            record(cid, 'error', '', result='failed', detail=str(e))
            print(f'[{cid}] stopped at the first failure; the log shows what already ran')
    if log:
        log.close()
    if bad:
        print(f'{len(bad)} action(s) skipped or failed in cluster(s) {sorted(set(bad))}; see the output and log')
        sys.exit(1)


if __name__ == '__main__':
    main(sys.argv[1:])
