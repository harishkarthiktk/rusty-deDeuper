//! Dev tasks: schema bump, OS-boundary check. Schema is a stub until the schema changes.

use std::fs;
use std::path::{Path, PathBuf};

/// Needles matched anywhere in a line (rules 1, 3, 4).
const ANY_LINE: &[&str] = &[
    "cfg(target_os",
    "cfg(windows",
    "cfg(unix",
    "cfg(not(windows",
    "cfg(not(unix",
    "target_family",
    "std::os::",
];

/// Needles matched only when the line also mentions `cfg`, to catch `cfg(any(..))`,
/// `cfg(all(..))` and `cfg_attr(..)` forms.
const CFG_LINE: &[&str] = &[
    "target_os",
    "(windows",
    "(unix",
    " windows",
    " unix",
    "windows)",
    "unix)",
    "windows,",
    "unix,",
];

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("schema") => eprintln!("schema bump not implemented yet (schema is v2)"),
        Some("boundaries") => match boundaries() {
            Ok(()) => println!("boundaries: ok"),
            Err(violations) => {
                violations.iter().for_each(|v| println!("{v}"));
                std::process::exit(1);
            }
        },
        Some(other) => {
            eprintln!("unknown task: {other}");
            std::process::exit(2);
        }
        None => eprintln!("usage: xtask <schema|boundaries>"),
    }
}

/// Workspace root, derived from this crate's manifest dir so any cwd works.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

/// All `.rs` files under `dir`, recursively. A missing or unreadable dir is an error, so the
/// check never passes on files it did not see.
fn rs_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(rs_files(&path)?);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(out)
}

/// The OS-specific needle found in `line`, if any. Comment lines are skipped.
fn os_needle(line: &str) -> Option<&'static str> {
    if line.trim_start().starts_with("//") {
        return None;
    }
    let any = ANY_LINE.iter().find(|n| line.contains(**n));
    let cfg = line
        .contains("cfg")
        .then(|| CFG_LINE.iter().find(|n| line.contains(**n)))
        .flatten();
    any.or(cfg).copied()
}

/// Violations for every `.rs` file under `src` that `allowed` does not exempt.
/// `allowed` receives the path relative to the workspace root, with `/` separators.
fn check_src(root: &Path, src: &str, rule: &str, allowed: impl Fn(&str) -> bool) -> Vec<String> {
    let files = match rs_files(&root.join(src)) {
        Ok(files) if files.is_empty() => {
            return vec![format!("{src}: no .rs files found ({rule})")]
        }
        Ok(files) => files,
        Err(e) => return vec![format!("{src}: cannot list ({e}) ({rule})")],
    };
    let mut out = Vec::new();
    for file in files {
        let rel = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .replace('\\', "/");
        if allowed(&rel) {
            continue;
        }
        let text = match fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) => {
                out.push(format!("{rel}: unreadable ({e}) ({rule})"));
                continue;
            }
        };
        for (i, line) in text.lines().enumerate() {
            if let Some(n) = os_needle(line) {
                out.push(format!("{rel}:{}: {n} ({rule})", i + 1));
            }
        }
    }
    out
}

/// Fails (exit 1) when a platform cfg or OS import appears outside the places allowed to hold it.
// shortcut: substring match, not a parser; a cfg split across lines escapes it, upgrade to syn if that happens.
fn boundaries() -> Result<(), Vec<String>> {
    let root = root();
    let mut v = check_src(&root, "crates/core/src", "core: no OS code", |_| false);
    let toml = fs::read_to_string(root.join("crates/core/Cargo.toml")).unwrap_or_else(|e| {
        v.push(format!(
            "crates/core/Cargo.toml: unreadable ({e}) (core: no target deps)"
        ));
        String::new()
    });
    for (i, line) in toml.lines().enumerate() {
        if line.starts_with("[target.") {
            v.push(format!(
                "crates/core/Cargo.toml:{}: [target. (core: no target deps)",
                i + 1
            ));
        }
    }
    v.extend(check_src(
        &root,
        "crates/cli/src",
        "cli: only platform/mod.rs",
        |p| p == "crates/cli/src/platform/mod.rs",
    ));
    v.extend(check_src(
        &root,
        "crates/backend-generic/src",
        "backend-generic: only sys/ and parse/",
        |p| {
            p.starts_with("crates/backend-generic/src/sys/")
                || p.starts_with("crates/backend-generic/src/parse/")
        },
    ));
    if v.is_empty() {
        Ok(())
    } else {
        Err(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_src_should_report_violation_when_dir_missing() {
        let v = check_src(&root(), "crates/does-not-exist/src", "test rule", |_| false);
        assert_eq!(
            v.len(),
            1,
            "a missing dir must give exactly one violation, got {v:?}"
        );
        assert!(
            v[0].contains("cannot list"),
            "violation must say the dir was not listed: {v:?}"
        );
    }
}
