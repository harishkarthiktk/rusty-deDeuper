# xtask (`dedupe-xtask`)

Workspace dev tasks, in one file (`src/main.rs`, no dependencies). Nothing depends on it and it is not
shipped. Run from any folder: `cargo run -p dedupe-xtask -- <task>`. The workspace root comes from
`CARGO_MANIFEST_DIR`, so the cwd does not matter.

Running `boundaries` is a project script. Ask the user before running it.

## Tasks
- `schema`: stub. Prints `schema bump not implemented yet (schema is v1)` to stderr and exits 0.
- `boundaries`: OS-boundary check. Prints `boundaries: ok` on success. On a violation it prints one
  `path:line: needle (rule)` line per hit to stdout and exits 1.
- No argument: prints usage to stderr and exits 0. Unknown task name: exits 2.

## Boundary rules (`fn boundaries`)
1. `crates/core/src`: no OS code at all. No exemptions.
2. `crates/core/Cargo.toml`: no line starting with `[target.` (no target-specific dependencies).
3. `crates/cli/src`: OS code allowed only in `crates/cli/src/platform/mod.rs`.
4. `crates/backend-generic/src`: OS code allowed only under `sys/` and `parse/`.

`backend-windows` and `win-sys` are not checked. Their Windows code is the point of those crates.
`tests/`, `bench/` and `xtask/` are not checked either.

## How a line is matched
- Substring match, not a parser. `os_needle` returns the first hit.
- `ANY_LINE` needles match anywhere on the line: `cfg(target_os`, `cfg(windows`, `cfg(unix`,
  `cfg(not(windows`, `cfg(not(unix`, `target_family`, `std::os::`.
- `CFG_LINE` needles (`target_os`, `(windows`, `(unix`, ` windows`, ` unix`, `windows)`, `unix)`,
  `windows,`, `unix,`) count only when the line also contains `cfg`. This catches `cfg(any(..))`,
  `cfg(all(..))` and `cfg_attr(..)`.
- Lines whose trimmed start is `//` are skipped. A trailing comment on a code line is still checked.
- Shortcut (source comment on `boundaries`): a `cfg` split across lines escapes the check. Upgrade to
  `syn` if that happens.

## Exit codes
- 0: `boundaries: ok`, or `schema`, or no argument.
- 1: at least one violation (printed to stdout).
- 2: unknown task name.
