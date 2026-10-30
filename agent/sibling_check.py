"""Count how many files of given folders appear in any duplicate group.

Usage: python sibling_check.py <kept.json> <ignore.json> <folder_list.json> [listing]
folder_list.json: JSON array of Windows folder paths (e.g. "X:\\A\\B"). Compared
case-insensitively, direct children only. WSL mount-root paths (e.g. $DEDUPE_MOUNT_ROOT/x/A/B) are accepted.
listing (optional): a JSON array of file paths, or a text file with one path per
line (a folder listing). With it, files in the folder but in no group are printed
as UNIQUE. Without it, unique files cannot be confirmed.
"""
import collections
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonio import load_json, read_text  # noqa: E402


def norm(p):
    # WSL automount root is configurable in /etc/wsl.conf, so it is not assumed.
    root = os.environ.get('DEDUPE_MOUNT_ROOT', '/mnt').rstrip('/')
    m = re.match(re.escape(root) + r'/([a-zA-Z])(/.*)?$', p)
    if m:
        p = m.group(1).upper() + ':' + (m.group(2) or '')
    return p.replace('/', '\\').rstrip('\\').lower()


def main(kept, ignore, folders_file, listing=None):
    folders = load_json(folders_file)[0]
    in_group = collections.defaultdict(list)  # folder -> [(source, group index, name)]
    members = set()
    for label, path in (('kept', kept), ('ignore', ignore)):
        for i, g in enumerate(load_json(path)[0]['groups']):
            for m in g['members']:
                p = norm(m['path'])
                members.add(p)
                in_group[p.rsplit('\\', 1)[0]].append((label, i, p.rsplit('\\', 1)[1]))
    listed = collections.defaultdict(list)
    if listing:
        text, _ = read_text(listing)
        paths = json.loads(text) if text.lstrip().startswith('[') else text.splitlines()
        for p in paths:
            p = norm(p.strip())
            if p:
                listed[p.rsplit('\\', 1)[0]].append(p)
    for f in folders:
        k = norm(f)
        hits = in_group.get(k, [])
        by = collections.Counter(h[0] for h in hits)
        print(f'{f}: {len(hits)} files in groups (kept {by["kept"]}, ignore {by["ignore"]}), '
              f'{len({(h[0], h[1]) for h in hits})} distinct groups')
        if listing:
            uniq = sorted(p for p in listed.get(k, []) if p not in members)
            print(f'  listing has {len(listed.get(k, []))} files, {len(uniq)} UNIQUE (in no group)')
            for p in uniq:
                print('   UNIQUE', p)
        else:
            print('  unique files can only be confirmed from a folder listing (pass it as 4th arg)')


if __name__ == '__main__':
    if len(sys.argv) not in (4, 5):
        sys.exit(__doc__)
    main(*sys.argv[1:])
