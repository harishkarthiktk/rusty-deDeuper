"""Dry-run summary: how many groups the artifact rules would ignore, and what is left.

Usage: python report.py <input.json>
Read-only. Prints counts by reason, wasted bytes, leftover path segments in kept
groups (use them to spot new artifact patterns) and a few mixed-group samples.
"""
import collections
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonio import load_json, split_path  # noqa: E402
from rules import reason  # noqa: E402


def main(src):
    d, _, _ = load_json(src)
    ign_groups = collections.Counter()
    ign_bytes = 0
    kept = []
    mixed = []
    kept_seg = collections.Counter()
    for g in d['groups']:
        rs = [reason(m['path']) for m in g['members']]
        if all(rs):
            ign_groups[','.join(sorted(set(rs)))] += 1
            ign_bytes += g['wasted_logical']
            continue
        kept.append(g)
        if any(rs):
            mixed.append(g)
        for m, r in zip(g['members'], rs):
            if not r:
                for s in set(split_path(m['path'])[:-1]):
                    kept_seg[s] += 1
    kept_bytes = sum(g['wasted_logical'] for g in kept)
    print('groups total', len(d['groups']), 'ignored', sum(ign_groups.values()),
          'kept', len(kept), 'mixed kept', len(mixed))
    print('wasted ignored', ign_bytes, 'wasted kept', kept_bytes, 'sum', ign_bytes + kept_bytes,
          'orig total', d['wasted_logical_total'])
    print('ignored by reason:')
    for k, v in ign_groups.most_common():
        print('  ', v, k)
    print('leftover segments in kept groups (non-artifact members):')
    for k, v in kept_seg.most_common(70):
        print('  ', v, k)
    random.seed(3)
    print('mixed samples:')
    for g in random.sample(mixed, min(8, len(mixed))):
        print('  --')
        for m in g['members']:
            print('    ', reason(m['path']), '|', m['path'])


if __name__ == '__main__':
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    main(sys.argv[1])
