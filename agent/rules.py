# Member-level artifact rules. Each returns a reason label or None.
# Folder names are lowercased first, so matching is case-insensitive (Scripts, scripts, SCRIPTS).
import ntpath

RUST_PROFILES = {'debug', 'release', 'doc'}
ARTIFACT_EXT = {'.pyc', '.pyo', '.o', '.obj', '.pdb', '.rlib', '.rmeta', '.timestamp', '.a', '.pyd'}

def is_venv(dirs):
    if 'site-packages' in dirs or 'dist-packages' in dirs:
        return True
    named = any(s.startswith('.venv') or s.startswith('venv') for s in dirs)
    env_like = any(s == 'env' or s.endswith('env') for s in dirs)
    return named or (env_like and bool({'lib', 'scripts', 'include', 'share'} & dirs))

def is_venv_launcher(dirs, low):
    return 'scripts' in dirs and low.startswith(('python', 'pip', 'activate', 'deactivate'))

def is_cmake_install(dirs, low):
    return any(s.startswith('cmake-') for s in dirs)

def is_rust_target(dirs):
    if '.fingerprint' in dirs:
        return True
    return 'target' in dirs and bool(RUST_PROFILES & dirs)

DIR_RULES = [
    ('python-venv', lambda dirs, low: is_venv(dirs) or is_venv_launcher(dirs, low)),
    ('build-toolchain', lambda dirs, low: is_cmake_install(dirs, low)),
    ('node-modules', lambda dirs, low: 'node_modules' in dirs),
    ('rust-target', lambda dirs, low: is_rust_target(dirs)),
    ('git-internals', lambda dirs, low: '.git' in dirs),
    ('python-cache', lambda dirs, low: '__pycache__' in dirs),
]

def reason(path):
    parts = path.replace('/', '\\').split('\\')
    dirs = {s.lower() for s in parts[:-1]}
    low = parts[-1].lower()
    for label, rule in DIR_RULES:
        if rule(dirs, low):
            return label
    if ntpath.splitext(low)[1] in ARTIFACT_EXT:
        return 'compiled-artifact'
    return None
