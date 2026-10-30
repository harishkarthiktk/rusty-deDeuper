"""Split a dedupe.json into kept groups and ignored (artifact) groups.

Usage: python split.py <input.json> <out_dir>
Writes <out_dir>/dedupe.json and <out_dir>/dedupe_ignore.json, same encoding,
indent and newlines as the input (UTF-16 LE with BOM when the input was UTF-16).
"""
import collections
import json
import os
import re
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from jsonio import load_json  # noqa: E402
from rules import reason  # noqa: E402


def main(src, out_dir):
    d, text, enc = load_json(src)

    # Detect the original layout so the output matches it.
    m = re.search(r'\r?\n( *)"', text)
    indent = len(m.group(1)) if m else 2
    nl = '\r\n' if '\r\n' in text else '\n'

    def dump(obj):
        return json.dumps(obj, indent=indent, ensure_ascii=False).replace('\n', nl)

    body = text.rstrip('\r\n')
    trailing = text[len(body):]
    assert dump(d) == body, 'layout not reproducible (indent/newline detection failed)'

    kept, ignored = [], []
    reasons = collections.Counter()
    for g in d['groups']:
        rs = [reason(m['path']) for m in g['members']]
        if all(rs):
            label = ','.join(sorted(set(rs)))
            ignored.append({**g, 'reason': label})
            reasons[label] += 1
        else:
            kept.append(g)

    # Lossless check: the two sets together are exactly the original groups.
    assert len(kept) + len(ignored) == len(d['groups'])
    orig = collections.Counter(json.dumps(g, sort_keys=True) for g in d['groups'])
    new = collections.Counter(
        json.dumps({k: v for k, v in g.items() if k != 'reason'}, sort_keys=True)
        for g in kept + ignored)
    assert orig == new, 'split is not lossless'

    base = {k: d[k] for k in ('files_enumerated', 'files_errored', 'files_skipped')}
    kept_doc = {**base, 'group_count': len(kept),
                'wasted_logical_total': sum(g['wasted_logical'] for g in kept),
                'groups': kept}
    ign_doc = {**base, 'group_count': len(ignored),
               'wasted_logical_total': sum(g['wasted_logical'] for g in ignored),
               'reason_counts': dict(reasons.most_common()),
               'groups': ignored}
    assert kept_doc['wasted_logical_total'] + ign_doc['wasted_logical_total'] == d['wasted_logical_total'], \
        'wasted total mismatch'

    def write(path, doc):
        s = dump(doc) + trailing
        if enc == 'utf-16':
            data = b'\xff\xfe' + s.encode('utf-16-le')
        elif enc == 'utf-8-sig':
            data = b'\xef\xbb\xbf' + s.encode('utf-8')
        else:
            data = s.encode('utf-8')
        with open(path, 'wb') as f:
            f.write(data)

    os.makedirs(out_dir, exist_ok=True)
    write(os.path.join(out_dir, 'dedupe.json'), kept_doc)
    write(os.path.join(out_dir, 'dedupe_ignore.json'), ign_doc)
    print('kept', len(kept), 'ignored', len(ignored), 'reasons', dict(reasons.most_common()))
    print('kept wasted', kept_doc['wasted_logical_total'], 'ignored wasted', ign_doc['wasted_logical_total'])


if __name__ == '__main__':
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(sys.argv[1], sys.argv[2])
