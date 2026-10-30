"""Plan a folder-level reconciliation. Read-only: reads JSON, writes only <out.json>.

Usage: python reconcile_plan.py <kept.json> <ignore.json> <folder_inventory.json> <out.json> [overrides.json] [--dry-run]
  --dry-run       print every planned action and validate the plan; writes no <out.json>; exit 1 on problems
  inventory "-"   cluster step only: out.json lists clusters and their folders (input for the listing step)
  overrides.json  optional {"<cluster_id>": "X:\\common\\folder"} to force the common folder
folder_inventory.json: {"folders": [{"index", "windows_path", "status": "ok"|"missing",
  "entries": [{"rel_path", "kind": "file"|"dir", "size", "mtime"}]}]}
Paths keep the report's style (Windows or POSIX, see jsonio.py). Inventory and override paths in the
other style are translated through the WSL mount root.
Plan per cluster: {cluster_id, folders, common_folder, override_ignored, moves, renames, deletes,
  folder_removals, conflicts, unresolved}. Nothing is executed here.
"""
import collections
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonio import key, load_json, pathmod, report_style, to_style  # noqa: E402

SYSTEM_FILES = {'desktop.ini', 'thumbs.db'}
STYLE, PM, SEP = 'win', pathmod('win'), '\\'  # set from the report in main()


def norm(p):
    return key(p, STYLE)


def parent(p):
    return PM.dirname(p)


def in_tree(p, root):
    """p is root or below it (both normed)."""
    return p == root or p.startswith(root if root.endswith(SEP) else root + SEP)


def lossy(p):
    """Name was not valid UTF-8 or could not be listed exactly: never act on it."""
    return '\ufffd' in p or any('\udc80' <= ch <= '\udcff' for ch in p)


def scratch(p):
    segs = [s.lower() for s in p.split(SEP)]
    pen = sum(any(k in s for k in ('new folder', 'temp', 'extract')) for s in segs)
    return pen + any(a == b for a, b in zip(segs, segs[1:]))  # X\X repeat


def clusters_from(kept):
    """Union folders that share a duplicate group. Returns (clusters, display names)."""
    up, disp = {}, {}

    def find(x):
        while up[x] != x:
            up[x] = up[up[x]]
            x = up[x]
        return x

    for g in kept['groups']:
        fs = []
        for m in g['members']:
            f = parent(m['path'])
            disp.setdefault(norm(f), f)
            up.setdefault(norm(f), norm(f))
            fs.append(norm(f))
        for f in fs[1:]:
            up[find(f)] = find(fs[0])
    comp = collections.defaultdict(set)
    for f in up:
        comp[find(f)].add(f)
    out = sorted((sorted(c) for c in comp.values() if len(c) > 1), key=lambda c: c[0])
    return out, disp


def differs(a, b):
    """a, b = (size, hash or None). True = proven different, None = unknown."""
    if a[0] != b[0]:
        return True  # sizes prove difference only
    if a[1] and b[1]:
        return a[1] != b[1]
    return True if (a[1] or b[1]) else None  # one in a group, one in none: different


def plan_cluster(cid, folders, disp, inv, info, override, byhash):
    unresolved, conflicts, moves, renames, deletes = [], [], [], [], []
    ok = []
    inv_ok = [f for f in folders if (inv.get(f) or {}).get('status') == 'ok']
    for f in folders:
        rec = inv.get(f)
        if rec is None or rec.get('status') != 'ok':
            if any(f != o and in_tree(f, o) for o in inv_ok):
                continue  # inside an inventoried folder (for example Pic), already in its recursive listing
            unresolved.append({'folder': disp[f], 'reason': 'not in inventory' if rec is None else 'folder missing'})
        else:
            ok.append(f)
    # Files and dirs, owned by the deepest cluster folder that contains them.
    files, dirs = {}, collections.defaultdict(list)
    for f in ok:
        for e in inv[f]['entries']:
            rel = e['rel_path'].replace('/', '\\') if STYLE == 'win' else e['rel_path']
            full = disp[f] + SEP + rel
            owner = max((o for o in ok if norm(full) != o and in_tree(norm(full), o)), key=len)
            if owner != f:
                continue
            if e['kind'] == 'dir':
                dirs[f].append(rel)
            else:
                files[norm(full)] = {'owner': f, 'rel': rel, 'full': full, 'size': e['size'],
                                     'name': PM.basename(rel).lower()}
    held = collections.Counter(r['owner'] for r in files.values())
    order = sorted(ok, key=lambda f: (-held[f], scratch(disp[f]), f))
    if not order:
        return {'cluster_id': cid, 'folders': [disp[f] for f in folders], 'common_folder': None,
                'override_ignored': None, 'moves': [], 'renames': [], 'deletes': [], 'folder_removals': [],
                'conflicts': [], 'unresolved': unresolved}
    common = order[0]
    override_ignored = None
    if override:
        try:
            ov = norm(to_style(override, STYLE))
        except ValueError:
            ov = None
        if ov in ok:
            common = ov
        else:
            override_ignored = override
            unresolved.append({'folder': override, 'reason': 'override is not an ok folder of this cluster'})
    cdisp = disp[common]
    occupied = {p: (r['size'], info.get(p, (None,))[0]) for p, r in files.items() if in_tree(p, common)}
    planned = {}
    handled = set()

    def place(rec, h):
        """Return the final path for rec in the common folder, or None (recorded as unresolved)."""
        mine = (rec['size'], h)
        t = cdisp + SEP + rec['rel']
        taken = {**occupied, **planned}
        if norm(t) in taken:
            d = differs(mine, taken[norm(t)])
            if d is None:
                unresolved.append({'path': rec['full'], 'target': t,
                                   'reason': 'same name and size, not both in a group; hash needed (reads media, needs user approval)'})
                return None
            if not d:
                unresolved.append({'path': rec['full'], 'target': t, 'reason': 'same content already at target'})
                return None
            segs = [s for s in disp[rec['owner']].split(SEP) if s and not s.endswith(':') and s != '?']
            sfx = re.sub(r'[^\w.-]+', '_', '-'.join(segs[-2:]))
            stem, ext = PM.splitext(t)
            t2 = f'{stem}__{sfx}{ext}'
            if norm(t2) in taken:
                unresolved.append({'path': rec['full'], 'target': t2, 'reason': 'renamed target also taken'})
                return None
            conflicts.append({'path': rec['full'], 'target': t, 'reason': 'same name, different content'})
            renames.append({'from': rec['full'], 'to': t2, 'reason': 'name conflict with ' + t})
            planned[norm(t2)] = mine
            handled.add(norm(rec['full']))
            return t2
        planned[norm(t)] = mine
        moves.append({'from': rec['full'], 'to': t})
        handled.add(norm(rec['full']))
        return t

    dup_by_hash = collections.defaultdict(list)
    system = collections.defaultdict(list)
    for p in sorted(files):
        r = files[p]
        if r['name'] in SYSTEM_FILES:
            system[r['owner']].append(r['full'])
            continue
        if lossy(r['full']):
            unresolved.append({'path': r['full'], 'reason': 'name not valid UTF-8, handle by hand'})
            continue
        if p in info:
            h, size = info[p]
            if size != r['size']:
                unresolved.append({'path': r['full'], 'reason': 'size differs from scan, stale'})
                continue
            dup_by_hash[h].append(p)
        elif not in_tree(p, common):
            place(r, None)  # unique: move, never delete

    rank = {f: i for i, f in enumerate(order)}
    owned = set(files)
    for h, ps in dup_by_hash.items():
        # a copy anywhere under the common folder counts as kept (common may be a parent of segregation subfolders)
        in_common = [p for p in ps if in_tree(p, common)]
        others = [p for p in ps if p not in in_common]
        if len(in_common) > 1:
            unresolved.append({'hash': h, 'reason': 'duplicates inside the common folder, not touched',
                               'paths': [files[p]['full'] for p in in_common]})
        if in_common:
            keeper = files[in_common[0]]['full']
        elif others:
            if any(q not in owned for q in byhash.get(h, [])):
                unresolved.append({'hash': h, 'reason': 'copy outside the cluster and none in the common folder, not touched',
                                   'paths': [files[p]['full'] for p in others]})
                continue
            others.sort(key=lambda p: rank[files[p]['owner']])
            keeper = place(files[others.pop(0)], h)
            if keeper is None:
                continue
        for p in others:
            deletes.append({'path': files[p]['full'], 'matches': keeper, 'size': files[p]['size']})
            handled.add(p)

    removals, status_of = [], {}
    ok_set = set(ok)
    # folders inside the common tree are destination folders, never removed; deepest first
    for f in sorted((f for f in ok if not in_tree(f, common)), key=lambda f: -f.count(SEP)):
        left = [r['full'] for p, r in files.items() if r['owner'] == f and p not in handled
                and r['name'] not in SYSTEM_FILES]
        # a cluster folder inside f that stays (the common folder, or one not removable) keeps f
        inner = [disp[g] for g in ok if g != f and in_tree(g, f)
                 and (in_tree(g, common) or status_of.get(g) != 'removable')]
        status = 'blocked' if left or inner else ('needs_user_ok' if system[f] else 'removable')
        status_of[f] = status
        removals.append({'folder': disp[f], 'status': status, 'blockers': left + inner,
                         'system_files': system[f],
                         # cluster folders inside f are removed by their own entry
                         'dirs_to_remove': sorted((d for d in dirs[f] if norm(disp[f] + SEP + d) not in ok_set),
                                                  key=lambda d: -d.count(SEP))})
    return {'cluster_id': cid, 'folders': [disp[f] for f in folders], 'common_folder': cdisp,
            'override_ignored': override_ignored,
            'moves': moves, 'renames': renames, 'deletes': deletes, 'folder_removals': removals,
            'conflicts': conflicts, 'unresolved': unresolved}


def dry_run_report(res):
    """Print every planned action and validate the whole plan. Returns the problems found."""
    problems = []
    targets, sources, deleted = collections.Counter(), set(), collections.Counter()
    kept = {}
    freed = held = 0
    for c in res:
        cid, common = c['cluster_id'], c['common_folder']
        print(f'== cluster {cid}: common folder {common}')
        if c.get('override_ignored'):
            problems.append(f"cluster {cid}: override {c['override_ignored']} is not an ok folder of this cluster")
        for m in c['moves']:
            print(f"  [{cid}] MOVE   {m['from']} -> {m['to']}")
            targets[norm(m['to'])] += 1
            sources.add(norm(m['from']))
        for r in c['renames']:
            print(f"  [{cid}] RENAME {r['from']} -> {r['to']}")
            targets[norm(r['to'])] += 1
            sources.add(norm(r['from']))
        for d in c['deletes']:
            print(f"  [{cid}] DELETE {d['path']} (matches {d['matches']})")
            deleted[norm(d['path'])] += 1
            freed += d.get('size') or 0
            keep = norm(d['matches'])
            kept[keep] = d['matches']
            if not (in_tree(keep, norm(common)) or keep in targets):
                problems.append(f"{d['path']}: matching copy {d['matches']} is neither in the common folder nor a planned target")
        for fr in c['folder_removals']:
            for sub in fr['dirs_to_remove']:
                print(f"  [{cid}] RMDIR  {fr['folder']}{SEP}{sub} (subfolder, deepest first)")
            print(f"  [{cid}] RMDIR  {fr['folder']} (status {fr['status']})")
            if fr['status'] == 'blocked':
                problems.append(f"{fr['folder']}: still holds files or folders the plan keeps: {fr['blockers'][:3]}")
        for u in c['unresolved']:
            print(f"  [{cid}] HELD   {u}")
            held += 1
    for t, n in targets.items():
        if n > 1:
            problems.append(f'{n} actions write to {t}')
    for p, n in deleted.items():
        if n > 1:
            problems.append(f'{p} is deleted {n} times')
        if p in sources:
            problems.append(f'{p} is both moved and deleted')
    for p, shown in kept.items():
        if p in deleted or p in sources:
            problems.append(f'{shown} is a matching copy but is itself deleted or moved')
    moves = sum(len(c['moves']) + len(c['renames']) for c in res)
    print(f'SUMMARY: {sum(len(c["deletes"]) for c in res)} deletes, {moves} moves/renames, '
          f'{held} held for review, {freed} bytes freed ({freed / 1e9:.3f} GB)')
    print(f'VALIDATION: {len(problems)} problem(s)')
    for p in problems:
        print('  PROBLEM', p)
    return problems


def main(kept_p, ign_p, inv_p, out_p, ovr_p=None, dry_run=False):
    global STYLE, PM, SEP
    kept = load_json(kept_p)[0]
    ign = load_json(ign_p)[0]
    STYLE = report_style(kept, ign)
    PM = pathmod(STYLE)
    SEP = PM.sep
    print('path style', STYLE)
    cl, disp = clusters_from(kept)
    if inv_p == '-':
        if dry_run:
            sys.exit('--dry-run needs folder_inventory.json, not "-"')
        res = [{'cluster_id': i, 'folders': [disp[f] for f in c]} for i, c in enumerate(cl, 1)]
    else:
        info, byhash = {}, collections.defaultdict(list)
        for g in kept['groups'] + ign['groups']:
            for m in g['members']:
                info[norm(m['path'])] = (g['full_hash'], g['size'])
                byhash[g['full_hash']].append(norm(m['path']))
        inv = {norm(to_style(r['windows_path'], STYLE)): r for r in load_json(inv_p)[0]['folders']}
        ovr = load_json(ovr_p)[0] if ovr_p else {}
        # "add_folders": {"<cluster_id>": ["X:\\parent"]} adds a parent folder (its whole tree) to a cluster
        for i, c in enumerate(cl, 1):
            for f in ovr.get('add_folders', {}).get(str(i), []):
                f = to_style(f, STYLE)
                disp.setdefault(norm(f), f)
                if norm(f) not in c:
                    c.append(norm(f))
        covered = [(i, c) for i, c in enumerate(cl, 1) if any(inv.get(f, {}).get('status') == 'ok' for f in c)]
        print(len(cl) - len(covered), 'clusters skipped: no folder in the inventory')
        res = [plan_cluster(i, c, disp, inv, info, ovr.get(str(i)), byhash) for i, c in covered]
    if dry_run:
        sys.exit(1 if dry_run_report(res) else 0)
    # backslashreplace keeps lone surrogates (non-UTF-8 names) as \udcXX escapes that json reads back
    with open(out_p, 'w', encoding='utf-8', errors='backslashreplace') as f:
        json.dump(res, f, indent=1, ensure_ascii=False)
    print(len(res), 'clusters ->', out_p)


if __name__ == '__main__':
    args = sys.argv[1:]
    dry_run = '--dry-run' in args
    args = [a for a in args if a != '--dry-run']
    if len(args) not in (4, 5):
        sys.exit(__doc__)
    main(*args, dry_run=dry_run)
