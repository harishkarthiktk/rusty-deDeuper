//! Path masking for LLM-bound output, with a sidecar vault that reverses it.
//!
//! Each word of a path becomes `W<n>` (a per-export counter), media extensions become
//! `E<n>`, and a small allowlist stays readable. The token-to-bytes maps are written to
//! a separate SQLite file (the vault) before any masked output is written, so a plan
//! that echoes masked paths can be turned back into real ones with [`unmask`].

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{Map, Value};

use crate::error::{CoreError, Result};
use crate::model::now_unix_ns;
use crate::path::{separator_for, simplify_verbatim, to_hex, RawPath};

/// Vault file name; the CLI puts it next to the DB and the scan never indexes it.
pub const VAULT_NAME: &str = ".dedupe.mask.db";

/// ASCII case-insensitive exact segments kept readable (superset of agent/rules.py names).
pub const ALLOW: &[&str] = &[
    "node_modules",
    ".git",
    ".svn",
    ".hg",
    "venv",
    ".venv",
    "env",
    "site-packages",
    "dist-packages",
    "__pycache__",
    "target",
    "build",
    "dist",
    "debug",
    "release",
    "bin",
    "obj",
    "lib",
    "include",
    "scripts",
    ".fingerprint",
    ".cache",
    ".idea",
    ".vscode",
    "Program Files",
    "Program Files (x86)",
    "Windows",
    "System32",
    "AppData",
    "Local",
    "Roaming",
    "Temp",
    "tmp",
    "Library",
    "Caches",
    "Applications",
    "usr",
    "share",
    "opt",
    "var",
    "home",
    "Users",
    "Documents",
    "Downloads",
    "Desktop",
    "mnt",
    "Volumes",
    "$RECYCLE.BIN",
    ".Trash",
];
pub const ALLOW_PREFIX: &[&str] = &["cmake-", ".venv"];
/// Final `.suffix` kept readable on the last segment.
pub const EXT_ALLOW: &[&str] = &[
    "js", "ts", "py", "rs", "c", "h", "cpp", "java", "json", "txt", "md", "html", "css", "pdf",
    "doc", "docx", "xls", "xlsx", "zip", "gz", "tar", "7z", "iso", "exe", "dll", "so", "o", "a",
    "pyc", "class", "jar", "log",
];
/// Media suffixes masked as `.E<n>` (ASCII case-insensitive match; original bytes go to ext_tokens).
pub const MEDIA_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tif", "tiff", "webp", "heic", "heif", "raw", "cr2", "cr3",
    "nef", "arw", "dng", "svg", "psd", "mp4", "mov", "avi", "mkv", "wmv", "m4v", "webm", "3gp",
    "mts", "mp3", "wav", "flac", "aac", "m4a", "ogg", "opus", "wma", "aiff",
];
/// Duplicate-marker words kept readable (plus v\d{1,3} and 1-4 digit numbers, checked in code).
pub const WORD_ALLOW: &[&str] = &[
    "copy",
    "backup",
    "bak",
    "old",
    "new",
    "final",
    "draft",
    "orig",
    "original",
    "dup",
    "duplicate",
    "temp",
    "tmp",
    "kopie",
    "copia",
    "copie",
    "kopia",
    "kopio",
    "副本",
];

/// Rows per vault transaction in [`Masker::flush`].
const FLUSH_CHUNK: usize = 50_000;

const VAULT_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS exports(id INTEGER PRIMARY KEY, created_ns INTEGER NOT NULL, db_uuid BLOB NOT NULL);
CREATE TABLE IF NOT EXISTS tokens(export_id INTEGER NOT NULL, tok INTEGER NOT NULL, bytes BLOB NOT NULL,
  PRIMARY KEY(export_id, tok)) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS ext_tokens(export_id INTEGER NOT NULL, tok INTEGER NOT NULL, ext BLOB NOT NULL,
  PRIMARY KEY(export_id, tok)) WITHOUT ROWID;
";

/// One export's token maps. Tokens are new until [`Masker::flush`] writes them.
pub struct Masker {
    export_id: i64,
    words: HashMap<Vec<u8>, u32>,
    exts: HashMap<Vec<u8>, u32>,
    fresh: Vec<(u32, Vec<u8>)>,
    fresh_ext: Vec<(u32, Vec<u8>)>,
}

impl Masker {
    /// Opens or creates the vault (WAL, busy_timeout), inserts an `exports` row tied to
    /// `db_uuid` and returns its id inside the masker.
    pub fn begin(vault: &Path, db_uuid: &[u8]) -> Result<(Self, Connection)> {
        let conn = Connection::open(vault)?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
        conn.execute_batch(VAULT_SCHEMA)?;
        conn.execute(
            "INSERT INTO exports(created_ns, db_uuid) VALUES(?1, ?2)",
            params![now_unix_ns(), db_uuid],
        )?;
        let export_id = conn.last_insert_rowid();
        let masker = Masker {
            export_id,
            words: HashMap::new(),
            exts: HashMap::new(),
            fresh: Vec::new(),
            fresh_ext: Vec::new(),
        };
        Ok((masker, conn))
    }

    pub fn export_id(&self) -> i64 {
        self.export_id
    }

    /// Keeps the root marker, splits on the path's own separator and masks each segment.
    /// The result is always valid UTF-8: non-ASCII bytes only ever sit inside tokenized words.
    pub fn mask(&mut self, p: &RawPath) -> String {
        let b = simplify_verbatim(p.as_bytes());
        let sep = separator_for(b);
        let root = root_len(b);
        let mut out = String::with_capacity(b.len());
        out.push_str(&String::from_utf8_lossy(&b[..root])); // ASCII by construction
        let mut segs = b[root..].split(|&c| c == sep).peekable();
        let mut first = true;
        while let Some(seg) = segs.next() {
            if !first {
                out.push(sep as char);
            }
            first = false;
            if !seg.is_empty() {
                self.mask_segment(seg, segs.peek().is_none(), &mut out);
            }
        }
        out
    }

    fn mask_segment(&mut self, seg: &[u8], last: bool, out: &mut String) {
        if allowed_exact(seg) {
            if let Ok(s) = std::str::from_utf8(seg) {
                out.push_str(s);
                return;
            }
        }
        let p = allowed_prefix_len(seg);
        if p > 0 {
            // Prefix stays readable for agent rules; the rest can hold a client or project name.
            out.push_str(&String::from_utf8_lossy(&seg[..p])); // ASCII: matched a list entry
            self.mask_words(&seg[p..], out);
            return;
        }
        if last {
            if let Some(dot) = seg.iter().rposition(|&c| c == b'.').filter(|&i| i > 0) {
                let ext = &seg[dot + 1..];
                if in_list(MEDIA_EXT, ext) {
                    self.mask_words(&seg[..dot], out);
                    let n = intern(&mut self.exts, &mut self.fresh_ext, ext);
                    let _ = write!(out, ".E{n}"); // writing to String cannot fail
                    return;
                }
                if in_list(EXT_ALLOW, ext) {
                    self.mask_words(&seg[..dot], out);
                    out.push('.');
                    out.push_str(&String::from_utf8_lossy(ext)); // ASCII: matched a list entry
                    return;
                }
            }
        }
        self.mask_words(seg, out);
    }

    /// Splits at ASCII non-alnum (emitted verbatim) and lower->Upper camelCase; non-ASCII
    /// bytes stay in words. Readable words verbatim, others `W<n>`.
    fn mask_words(&mut self, s: &[u8], out: &mut String) {
        let mut start = 0;
        for i in 0..=s.len() {
            let delim = i < s.len() && is_delim(s[i]);
            let camel = i > start
                && i < s.len()
                && s[i - 1].is_ascii_lowercase()
                && s[i].is_ascii_uppercase();
            if i < s.len() && !delim && !camel {
                continue;
            }
            if i > start {
                self.word(&s[start..i], out);
            }
            if delim {
                out.push(s[i] as char);
                start = i + 1;
            } else {
                start = i;
            }
        }
    }

    fn word(&mut self, w: &[u8], out: &mut String) {
        if readable_word(w) {
            out.push_str(&String::from_utf8_lossy(w)); // ASCII or 副本
        } else {
            let n = intern(&mut self.words, &mut self.fresh, w);
            let _ = write!(out, "W{n}"); // writing to String cannot fail
        }
    }

    /// Writes fresh word and ext tokens in `FLUSH_CHUNK`-row transactions. Call BEFORE
    /// the masked output is written, so every token in the output is already durable.
    pub fn flush(&mut self, conn: &mut Connection) -> Result<()> {
        flush_table(conn, "tokens", "bytes", self.export_id, &mut self.fresh)?;
        flush_table(
            conn,
            "ext_tokens",
            "ext",
            self.export_id,
            &mut self.fresh_ext,
        )
    }
}

fn flush_table(
    conn: &mut Connection,
    table: &str,
    col: &str,
    export_id: i64,
    fresh: &mut Vec<(u32, Vec<u8>)>,
) -> Result<()> {
    let sql = format!("INSERT INTO {table}(export_id, tok, {col}) VALUES(?1, ?2, ?3)");
    while !fresh.is_empty() {
        let n = fresh.len().min(FLUSH_CHUNK);
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(&sql)?;
            for (tok, bytes) in &fresh[..n] {
                stmt.execute(params![export_id, tok, bytes])?;
            }
        }
        tx.commit()?;
        fresh.drain(..n);
    }
    Ok(())
}

/// Counter for `key`, allocating the next one (and queueing it for flush) when new.
fn intern(map: &mut HashMap<Vec<u8>, u32>, fresh: &mut Vec<(u32, Vec<u8>)>, key: &[u8]) -> u32 {
    if let Some(&n) = map.get(key) {
        return n;
    }
    let n = map.len() as u32 + 1;
    map.insert(key.to_vec(), n);
    fresh.push((n, key.to_vec()));
    n
}

/// Length of the leading root marker (`\\?\UNC\`, `\\?\`, `\\`, `X:`), 0 for POSIX.
fn root_len(b: &[u8]) -> usize {
    if b.starts_with(br"\\?\UNC\") {
        8
    } else if b.starts_with(br"\\?\") {
        4
    } else if b.starts_with(br"\\")
        || matches!(b, [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic())
    {
        2
    } else {
        0
    }
}

fn is_delim(b: u8) -> bool {
    b.is_ascii() && !b.is_ascii_alphanumeric()
}

fn in_list(list: &[&str], s: &[u8]) -> bool {
    list.iter().any(|a| a.as_bytes().eq_ignore_ascii_case(s))
}

/// Readable text must never hold `[WE][0-9]`, or unmask could not scan for tokens.
fn has_token(b: &[u8]) -> bool {
    b.windows(2)
        .any(|w| matches!(w[0], b'W' | b'E') && w[1].is_ascii_digit())
}

fn allowed_exact(seg: &[u8]) -> bool {
    in_list(ALLOW, seg) && !has_token(seg)
}

/// Length of a matching ALLOW_PREFIX (ASCII, never holds a token), else 0.
fn allowed_prefix_len(seg: &[u8]) -> usize {
    ALLOW_PREFIX
        .iter()
        .find(|p| seg.len() >= p.len() && seg[..p.len()].eq_ignore_ascii_case(p.as_bytes()))
        .map_or(0, |p| p.len())
}

fn readable_word(w: &[u8]) -> bool {
    let digits =
        |d: &[u8], max: usize| (1..=max).contains(&d.len()) && d.iter().all(u8::is_ascii_digit);
    in_list(WORD_ALLOW, w)
        || digits(w, 4)
        || (matches!(w.first(), Some(b'v' | b'V')) && digits(&w[1..], 3))
}

/// `(tok, real extension bytes)` rows.
pub type ExtRows = Vec<(u32, Vec<u8>)>;

/// sanitized_extension -> real_extension for one export (latest if `None`), ordered by tok.
pub fn ext_map(vault: &Connection, export_id: Option<i64>) -> Result<(i64, ExtRows)> {
    let id = match export_id {
        Some(id) => id,
        None => vault
            .query_row("SELECT MAX(id) FROM exports", [], |r| {
                r.get::<_, Option<i64>>(0)
            })?
            .ok_or_else(|| CoreError::NotFound("no mask exports in the vault".into()))?,
    };
    let mut stmt =
        vault.prepare("SELECT tok, ext FROM ext_tokens WHERE export_id=?1 ORDER BY tok")?;
    let rows = stmt
        .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok((id, rows))
}

/// Errors unless export `export_id` was made for a DB whose `meta.db_uuid` is `db_uuid`.
/// A separate fn (not a parameter of [`unmask`]) so callers that have no DB can skip it.
pub fn check_db_uuid(vault: &Connection, export_id: i64, db_uuid: &[u8]) -> Result<()> {
    let stored: Option<Vec<u8>> = vault
        .query_row(
            "SELECT db_uuid FROM exports WHERE id=?1",
            [export_id],
            |r| r.get(0),
        )
        .optional()?;
    match stored {
        None => Err(CoreError::InvalidArg(format!(
            "unknown mask export {export_id}"
        ))),
        Some(s) if s == db_uuid => Ok(()),
        Some(_) => Err(CoreError::InvalidArg(format!(
            "mask export {export_id} belongs to a different database"
        ))),
    }
}

struct Unmasker<'a> {
    vault: &'a Connection,
    export_id: i64,
    in_dup_group: &'a dyn Fn(&RawPath) -> Result<bool>,
    cache: HashMap<(u8, u32), Option<Vec<u8>>>,
    unknown: BTreeSet<String>,
    not_dup: BTreeSet<String>,
    bad: BTreeSet<String>,
}

impl Unmasker<'_> {
    fn lookup(&mut self, kind: u8, n: u32) -> Result<Option<Vec<u8>>> {
        if let Some(v) = self.cache.get(&(kind, n)) {
            return Ok(v.clone());
        }
        let sql = if kind == b'W' {
            "SELECT bytes FROM tokens WHERE export_id=?1 AND tok=?2"
        } else {
            "SELECT ext FROM ext_tokens WHERE export_id=?1 AND tok=?2"
        };
        let v: Option<Vec<u8>> = self
            .vault
            .query_row(sql, params![self.export_id, n], |r| r.get(0))
            .optional()?;
        self.cache.insert((kind, n), v.clone());
        Ok(v)
    }

    /// `Ok(None)` when `s` is not rooted (free text). Rooted strings are always rebuilt and
    /// scope-checked, with or without tokens: fail closed.
    fn rebuild(&mut self, s: &str) -> Result<Option<Vec<u8>>> {
        let b = s.as_bytes();
        let rooted = b.first() == Some(&b'/') || root_len(b) > 0;
        if !rooted {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(b.len());
        let mut ok = true;
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if matches!(c, b'W' | b'E') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                let end = i + 1 + b[i + 1..].iter().take_while(|d| d.is_ascii_digit()).count();
                let found = match s[i + 1..end].parse::<u32>() {
                    Ok(n) => self.lookup(c, n)?,
                    Err(_) => None,
                };
                match found {
                    Some(v) => out.extend_from_slice(&v),
                    None => {
                        self.unknown.insert(s[i..end].to_string());
                        ok = false;
                    }
                }
                i = end;
            } else {
                out.push(c);
                i += 1;
            }
        }
        if !ok {
            return Ok(Some(out));
        }
        if !(self.in_dup_group)(&RawPath::new(out.clone()))? {
            // Report the masked form: unmask errors can reach an agent's (and so an LLM's) context.
            self.not_dup.insert(s.to_string());
        }
        Ok(Some(out))
    }

    fn walk(&mut self, v: &Value) -> Result<Value> {
        Ok(match v {
            Value::String(s) => match self.rebuild(s)? {
                None => v.clone(),
                Some(b) => match String::from_utf8(b) {
                    Ok(s) => Value::String(s),
                    Err(_) => {
                        self.bad.insert(s.clone());
                        Value::Null
                    }
                },
            },
            Value::Array(a) => Value::Array(a.iter().map(|x| self.walk(x)).collect::<Result<_>>()?),
            Value::Object(o) => {
                let mut out = Map::new();
                for (k, val) in o {
                    match (k.as_str(), val) {
                        ("path", Value::String(s)) => match self.rebuild(s)? {
                            None => {
                                out.insert(k.clone(), val.clone());
                            }
                            Some(b) => match String::from_utf8(b) {
                                Ok(s) => {
                                    out.insert(k.clone(), s.into());
                                }
                                Err(e) => {
                                    let raw = e.into_bytes();
                                    out.insert(
                                        "path".into(),
                                        String::from_utf8_lossy(&raw).into_owned().into(),
                                    );
                                    out.insert("path_bytes".into(), to_hex(&raw).into());
                                }
                            },
                        },
                        _ => {
                            out.insert(k.clone(), self.walk(val)?);
                        }
                    }
                }
                Value::Object(out)
            }
            _ => v.clone(),
        })
    }
}

/// JSON-only unmask for one export. Every string value that starts with a root marker is
/// rebuilt from the vault and scope-checked through `in_dup_group`, with or without tokens,
/// so a real path without tokens is refused too. Every other value is copied as is.
///
/// Errors list all unknown tokens, all rebuilt paths that fail `in_dup_group`, and any
/// non-UTF-8 path found outside a `path` key. Under a `path` key a non-UTF-8 result is
/// written as a lossy `path` plus a `path_bytes` hex sibling (as `RawPath::json_fields`).
/// The DB uuid check is separate: call [`check_db_uuid`] first.
pub fn unmask(
    input: &Value,
    export_id: i64,
    vault: &Connection,
    in_dup_group: &dyn Fn(&RawPath) -> Result<bool>,
) -> Result<Value> {
    let mut u = Unmasker {
        vault,
        export_id,
        in_dup_group,
        cache: HashMap::new(),
        unknown: BTreeSet::new(),
        not_dup: BTreeSet::new(),
        bad: BTreeSet::new(),
    };
    let out = u.walk(input)?;
    let join = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(", ");
    if !u.unknown.is_empty() {
        return Err(CoreError::InvalidArg(format!(
            "unknown mask token(s) for export {export_id}: {}",
            join(&u.unknown)
        )));
    }
    if !u.not_dup.is_empty() {
        return Err(CoreError::InvalidArg(format!(
            "path(s) not in a current duplicate group: {}",
            join(&u.not_dup)
        )));
    }
    if !u.bad.is_empty() {
        return Err(CoreError::InvalidArg(format!(
            "non-UTF-8 path outside a `path` key: {}",
            join(&u.bad)
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vault() -> (tempfile::TempDir, Masker, Connection) {
        let d = tempfile::tempdir().unwrap();
        let (m, c) = Masker::begin(&d.path().join("v.db"), b"uuid").unwrap();
        (d, m, c)
    }

    fn from_hex(h: &str) -> Vec<u8> {
        (0..h.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap())
            .collect()
    }

    /// Masked text with every `W<n>`/`E<n>` run removed.
    fn strip_tokens(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if matches!(b[i], b'W' | b'E') && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                i += 1 + b[i + 1..].iter().take_while(|d| d.is_ascii_digit()).count();
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        out
    }

    /// Masks each path, flushes, unmasks under a `path` key, returns (masked, rebuilt bytes).
    fn round_trip(paths: &[&[u8]]) -> Vec<(String, Vec<u8>)> {
        let (_d, mut m, mut c) = vault();
        let masked: Vec<String> = paths
            .iter()
            .map(|p| m.mask(&RawPath::new(p.to_vec())))
            .collect();
        m.flush(&mut c).unwrap();
        let input =
            json!({ "members": masked.iter().map(|s| json!({ "path": s })).collect::<Vec<_>>() });
        let out = unmask(&input, m.export_id(), &c, &|_| Ok(true)).unwrap();
        masked
            .into_iter()
            .zip(out["members"].as_array().unwrap())
            .map(|(s, o)| {
                let bytes = match o.get("path_bytes") {
                    Some(h) => from_hex(h.as_str().unwrap()),
                    None => o["path"].as_str().unwrap().as_bytes().to_vec(),
                };
                (s, bytes)
            })
            .collect()
    }

    #[test]
    fn round_trip_is_byte_exact() {
        let inputs: [&[u8]; 9] = [
            "/home/alice/Blue Dog (1).jpg".as_bytes(),
            r"C:\Users\bob\BlueDog\a.xlsx".as_bytes(),
            "C:/x/y.txt".as_bytes(),
            r"\\?\C:\Data\f.mp4".as_bytes(),
            r"\\?\UNC\srv\sh\f.txt".as_bytes(),
            r"\\srv\sh\Budget_final_v2.docx".as_bytes(),
            "/x/副本/日本.txt".as_bytes(),
            &b"/x/\xff\xfe.bin"[..],
            "/a//b/".as_bytes(),
        ];
        for (input, (_, got)) in inputs.iter().zip(round_trip(&inputs)) {
            assert_eq!(got, simplify_verbatim(input));
        }
    }

    #[test]
    fn real_token_lookalikes_round_trip() {
        let inputs: [&[u8]; 5] = [
            "/W1/E5.jpg".as_bytes(),
            "/x/fooW3".as_bytes(),
            "/x/v12/E2024".as_bytes(),
            "/x/cmake-W9x/a".as_bytes(),
            "/x/.venvW1/b".as_bytes(),
        ];
        for ((masked, bytes), input) in round_trip(&inputs).iter().zip(inputs) {
            assert_eq!(bytes.as_slice(), input);
            assert!(!has_token(&strip_tokens(masked)));
        }
    }

    #[test]
    fn masked_output_hides_names() {
        let (_d, mut m, _c) = vault();
        let masked = m.mask(&RawPath::new(
            "/home/alice/AcmeCorp/cmake-AcmeClient/.venvSecret/ProjectX/notes.txt",
        ));
        for secret in ["alice", "Acme", "Client", "Secret", "Project"] {
            assert!(!masked.contains(secret), "{masked} leaks {secret}");
        }
        assert!(masked.contains("cmake-") && masked.contains(".venv"));
        assert!(masked.is_ascii());
        let raw = m.mask(&RawPath::new(&b"/x/\xff\xfe.bin"[..]));
        assert!(raw.is_ascii());
    }

    #[test]
    fn unmask_rejects_unknown_and_foreign_tokens() {
        let (_d, m, c) = vault();
        let err = unmask(&json!({ "p": "/W999" }), m.export_id(), &c, &|_| Ok(true)).unwrap_err();
        assert!(err.to_string().contains("W999"));

        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("v.db");
        let (mut m1, mut c1) = Masker::begin(&path, b"uuid").unwrap();
        let masked = m1.mask(&RawPath::new("/alpha"));
        m1.flush(&mut c1).unwrap();
        let (m2, _c2) = Masker::begin(&path, b"uuid").unwrap();
        let err = unmask(&json!({ "p": &masked }), m2.export_id(), &c1, &|_| Ok(true)).unwrap_err();
        assert!(err.to_string().contains("W1"));
    }

    #[test]
    fn unmask_scope_gate() {
        let (_d, mut m, mut c) = vault();
        let masked = m.mask(&RawPath::new("/alpha/beta.txt"));
        m.flush(&mut c).unwrap();
        let id = m.export_id();

        let err = unmask(&json!({ "p": &masked }), id, &c, &|_| Ok(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&masked) && !err.contains("alpha"), "{err}");

        let err = unmask(&json!({ "p": &masked }), id, &c, &|_| {
            Err(CoreError::InvalidArg("db down".into()))
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("db down"));

        let err = unmask(&json!({ "p": "/etc/hosts" }), id, &c, &|_| Ok(false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("/etc/hosts"));
    }

    #[test]
    fn check_db_uuid_cases() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("v.db");
        let (m, c) = Masker::begin(&path, b"uuid").unwrap();
        let (no_db, _c2) = Masker::begin(&path, b"no-db").unwrap();
        assert!(check_db_uuid(&c, m.export_id(), b"uuid").is_ok());
        assert!(check_db_uuid(&c, m.export_id(), b"other").is_err());
        assert!(check_db_uuid(&c, no_db.export_id(), b"uuid").is_err());
        let err = check_db_uuid(&c, 999, b"uuid").unwrap_err().to_string();
        assert!(err.contains("unknown"));
    }

    #[test]
    fn non_utf8_and_free_text() {
        let (_d, mut m, mut c) = vault();
        let masked = m.mask(&RawPath::new(&b"/x/\xff.bin"[..]));
        m.flush(&mut c).unwrap();
        let id = m.export_id();

        let out = unmask(&json!({ "path": &masked }), id, &c, &|_| Ok(true)).unwrap();
        assert_eq!(out["path_bytes"], "2f782fff2e62696e");
        assert!(out["path"].as_str().unwrap().contains('\u{FFFD}'));

        let err = unmask(&json!({ "other": &masked }), id, &c, &|_| Ok(true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-UTF-8"));

        let free = json!({ "other": "W1 is a word", "x": "a: keep W2" });
        assert_eq!(unmask(&free, id, &c, &|_| Ok(true)).unwrap(), free);
    }
}
