"""Shared input helpers: BOM-aware JSON loading and path styles.

A report holds one path style: 'win' (C:\\a, \\\\?\\C:\\a, \\\\server\\share) from a Windows build,
or 'posix' (/mnt/c/a, /home/a) from a Linux or macOS build. Windows paths compare
case-insensitively, POSIX paths exactly. On WSL, X:\\a maps to $DEDUPE_MOUNT_ROOT/x/a (default /mnt).
"""
import json
import ntpath
import os
import posixpath
import re


def read_text(path):
    """Return (text, encoding). Encoding is one of utf-16, utf-8-sig, utf-8."""
    raw = open(path, 'rb').read()
    if raw[:2] in (b'\xff\xfe', b'\xfe\xff'):
        return raw.decode('utf-16'), 'utf-16'
    if raw[:3] == b'\xef\xbb\xbf':
        return raw.decode('utf-8-sig'), 'utf-8-sig'
    return raw.decode('utf-8'), 'utf-8'


def load_json(path):
    """Return (obj, text, encoding)."""
    text, enc = read_text(path)
    return json.loads(text), text, enc


def path_style(p):
    if re.match(r'^([A-Za-z]:[\\/]|\\\\)', p):
        return 'win'
    if p.startswith('/'):
        return 'posix'
    raise ValueError(f'not an absolute path: {p}')


def report_style(*docs):
    """The single path style of all members in the given reports. Exits on a mix."""
    styles = {path_style(m['path']) for d in docs for g in d['groups'] for m in g['members']}
    if len(styles) > 1:
        raise SystemExit('reports mix Windows and POSIX paths; scan with one build per report')
    return styles.pop() if styles else 'win'


def pathmod(style):
    return ntpath if style == 'win' else posixpath


def key(p, style):
    """Comparison key: case-folded with backslashes for Windows, exact for POSIX."""
    return ntpath.normcase(p).rstrip('\\') if style == 'win' else p.rstrip('/') or '/'


def split_path(p):
    """Split a Windows or POSIX path into segments."""
    return p.replace('/', '\\').split('\\')


def mount_root():
    # WSL automount root is configurable in /etc/wsl.conf, so it is not assumed.
    return os.environ.get('DEDUPE_MOUNT_ROOT', '/mnt').rstrip('/')


def strip_verbatim(p):
    """\\\\?\\C:\\a -> C:\\a. UNC and Volume verbatim paths are left as they are."""
    return p[4:] if re.match(r'^\\\\\?\\[A-Za-z]:', p) else p


def to_style(p, style):
    """Translate a user-supplied path (override, inventory, quarantine) into the report style."""
    if path_style(p) == style:
        return p
    if style == 'posix':  # X:\a -> /mnt/x/a
        m = re.match(r'^([A-Za-z]):[\\/]?(.*)$', strip_verbatim(p))
        if not m:
            raise ValueError(f'no POSIX mapping for {p}')
        return f'{mount_root()}/{m.group(1).lower()}/' + m.group(2).replace('\\', '/')
    m = re.match(re.escape(mount_root()) + r'/([a-zA-Z])(/.*)?$', p)  # /mnt/x/a -> X:\a
    if not m:
        raise ValueError(f'no Windows mapping for {p}')
    return m.group(1).upper() + ':' + ((m.group(2) or '').replace('/', '\\') or '\\')


def to_local(p):
    """Path usable by this host's file APIs."""
    style = path_style(p)
    if os.name == 'nt':
        if style != 'win':
            raise ValueError(f'POSIX path on a Windows host: {p}')
        return p
    if style == 'posix':
        return p
    return to_style(p, 'posix')
