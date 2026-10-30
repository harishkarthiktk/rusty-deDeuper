"""Execute a reconcile plan. Dry-run by default: nothing on disk changes unless --execute is given.

Usage: python execute_plan.py <plan.json> [--execute] [--quarantine <win_dir>] [--log <file.jsonl>]
  plan.json        output of reconcile_plan.py (JSON list of cluster records)
  --execute        perform the renames, deletes and folder removals
  --quarantine D   move deleted copies under the Windows folder D (path kept below D) instead of deleting them
  --log F          JSON lines log of every action (default: <plan>.exec.jsonl, written only with --execute)
Order per cluster: renames and moves, then deletes, then folder removals (deepest first).
Safety: a cluster is checked first. Any missing source, existing target, changed size or missing
matching copy skips the whole cluster. Folders are removed with os.rmdir, so a non-empty folder is never removed.
Only folders with status "removable" are touched. Paths: X:\\a\\b maps to $DEDUPE_MOUNT_ROOT/x/a/b on WSL (default /mnt).
"""
import json
import os
import re
import sys
import time


def to_local(p):
    if os.name == 'nt':
        return p
    m = re.match(r'^([A-Za-z]):\\(.*)$', p)
    if not m:
        raise ValueError(f'not a Windows drive path: {p}')
    # WSL automount root is configurable in /etc/wsl.conf, so it is not assumed.
    root = os.environ.get('DEDUPE_MOUNT_ROOT', '/mnt').rstrip('/')
    return f'{root}/{m.group(1).lower()}/' + m.group(2).replace('\\', '/')


def check_cluster(c):
    """Return a list of problems. Empty list means the cluster can run."""
    problems = []
    moves = c['renames'] + c['moves']
    sources = {m['from'].lower() for m in moves}
    targets = {m['to'].lower() for m in moves}
    for m in moves:
        if not os.path.isfile(to_local(m['from'])):
            problems.append(f"missing source {m['from']}")
        if os.path.exists(to_local(m['to'])):
            problems.append(f"target already exists {m['to']}")
    for d in c['deletes']:
        if not os.path.isfile(to_local(d['path'])):
            problems.append(f"missing file {d['path']}")
            continue
        if os.path.getsize(to_local(d['path'])) != d['size']:
            problems.append(f"size changed since the plan {d['path']}")
        if d['path'].lower() in sources:
            problems.append(f"file is both moved and deleted {d['path']}")
        if d['matches'].lower() not in targets and not os.path.isfile(to_local(d['matches'])):
            problems.append(f"matching copy missing {d['matches']}")
    return problems


def run_cluster(c, quarantine, record):
    cid = c['cluster_id']
    for m in c['renames'] + c['moves']:
        os.rename(to_local(m['from']), to_local(m['to']))
        record(cid, 'rename', m['from'], m['to'])
    for d in c['deletes']:
        src = to_local(d['path'])
        if quarantine:
            dst = quarantine + d['path'][2:]  # drop the drive letter and colon, keep the folder tree
            os.makedirs(os.path.dirname(to_local(dst)), exist_ok=True)
            os.rename(src, to_local(dst))
            record(cid, 'quarantine', d['path'], dst)
        else:
            os.remove(src)
            record(cid, 'delete', d['path'])
    for fr in c['folder_removals']:
        if fr['status'] != 'removable':
            record(cid, 'keep-folder', fr['folder'], detail=fr['status'])
            continue
        for sub in fr['dirs_to_remove']:  # already deepest first
            os.rmdir(to_local(fr['folder'] + '\\' + sub))
            record(cid, 'rmdir', fr['folder'] + '\\' + sub)
        os.rmdir(to_local(fr['folder']))
        record(cid, 'rmdir', fr['folder'])


def main(argv):
    execute = '--execute' in argv
    args = list(argv)
    quarantine = None
    if '--quarantine' in args:
        i = args.index('--quarantine')
        quarantine = args[i + 1].rstrip('\\')
        del args[i:i + 2]
    log_path = None
    if '--log' in args:
        i = args.index('--log')
        log_path = args[i + 1]
        del args[i:i + 2]
    args = [a for a in args if a != '--execute']
    if len(args) != 1:
        sys.exit(__doc__)
    plan_path = args[0]
    log = None
    if execute:
        log = open(log_path or plan_path + '.exec.jsonl', 'a', encoding='utf-8')

    def record(cid, action, path, target='', result='ok', detail=''):
        if log:
            log.write(json.dumps({'time': time.strftime('%Y-%m-%d %H:%M:%S'), 'cluster': cid, 'action': action,
                                  'path': path, 'target': target, 'result': result, 'detail': detail},
                                 ensure_ascii=False) + '\n')
            log.flush()
        tag = result.upper() if execute else 'PLAN'
        print(f'[{cid}] {tag:9} {action:11} {path}' + (f' -> {target}' if target else '') +
              (f'  ({detail})' if detail else ''))

    with open(plan_path, encoding='utf-8') as f:
        plan = json.load(f)
    for c in plan:
        cid = c['cluster_id']
        if not c['common_folder']:
            continue
        problems = check_cluster(c)
        if problems:
            for p in problems:
                record(cid, 'precheck', p, result='skipped')
            print(f'[{cid}] cluster skipped: {len(problems)} precheck problem(s)')
            continue
        if not execute:
            run_cluster_plan(c, quarantine, record)
            continue
        try:
            run_cluster(c, quarantine, record)
        except OSError as e:
            record(cid, 'error', str(e), result='failed')
            print(f'[{cid}] stopped at the first failure; the log shows what already ran')
    if log:
        log.close()


def run_cluster_plan(c, quarantine, record):
    """Dry-run output: list the actions without touching disk."""
    cid = c['cluster_id']
    for m in c['renames'] + c['moves']:
        record(cid, 'rename', m['from'], m['to'])
    for d in c['deletes']:
        if quarantine:
            record(cid, 'quarantine', d['path'], quarantine + d['path'][2:])
        else:
            record(cid, 'delete', d['path'])
    for fr in c['folder_removals']:
        if fr['status'] != 'removable':
            record(cid, 'keep-folder', fr['folder'], detail=fr['status'])
            continue
        for sub in fr['dirs_to_remove']:
            record(cid, 'rmdir', fr['folder'] + '\\' + sub)
        record(cid, 'rmdir', fr['folder'])


if __name__ == '__main__':
    main(sys.argv[1:])
