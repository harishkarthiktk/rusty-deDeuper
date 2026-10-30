"""Shared input helpers: BOM-aware text and JSON loading."""
import json


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


def split_path(p):
    """Split a Windows (or forward-slash) path into segments."""
    return p.replace('/', '\\').split('\\')
