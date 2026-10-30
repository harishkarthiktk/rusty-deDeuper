"""Rank duplicate groups by wasted_logical and list every location of the top N.

Usage: python top_locations.py <input.json> <N> <out_dir>
Writes <out_dir>/top_<N>_locations.md and <out_dir>/top_<N>_locations.json.
Location numbers are <rank>.<member index>, e.g. 1.1 and 1.2 for rank 1.
Size MB is decimal (bytes / 1e6).
"""
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonio import load_json  # noqa: E402


def main(src, n, out_dir):
    d, _, _ = load_json(src)
    top = sorted(d['groups'], key=lambda g: g['wasted_logical'], reverse=True)[:n]  # stable on ties
    rows = []
    for rank, g in enumerate(top, 1):
        for i, m in enumerate(g['members'], 1):
            p = m['path'].replace('/', '\\')
            folder, name = p.rsplit('\\', 1)
            rows.append({'group': rank, 'loc': f'{rank}.{i}', 'path': p, 'folder': folder,
                         'name': name, 'size': g['size'], 'full_hash': g['full_hash']})
    os.makedirs(out_dir, exist_ok=True)
    with open(os.path.join(out_dir, f'top_{n}_locations.json'), 'w', encoding='utf-8') as f:
        json.dump(rows, f, indent=1, ensure_ascii=False)
    lines = ['| # | Loc | Folder (under drive root) | File | Size (MB) |', '|---|---|---|---|---|']
    for r in rows:
        under = r['folder'].split('\\', 1)[1] if '\\' in r['folder'] else r['folder']
        lines.append(f"| {r['group']} | {r['loc']} | {under} | {r['name']} | {r['size'] / 1e6:.1f} |")
    with open(os.path.join(out_dir, f'top_{n}_locations.md'), 'w', encoding='utf-8') as f:
        f.write('\n'.join(lines) + '\n')
    print(f'{len(top)} groups, {len(rows)} locations, wasted',
          sum(g['wasted_logical'] for g in top))


if __name__ == '__main__':
    if len(sys.argv) != 4:
        sys.exit(__doc__)
    main(sys.argv[1], int(sys.argv[2]), sys.argv[3])
