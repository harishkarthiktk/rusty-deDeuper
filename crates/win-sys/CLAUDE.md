# win-sys (`dedupe-win-sys`)

Shared Win32 handle and path helpers. It is the only crate that owns Win32 FFI used by more than one crate. It depends only on `windows-sys` (Windows target). Callers are `backend-generic` (`crates/backend-generic/src/sys/windows/`) and `backend-windows`. For layering, see the root CLAUDE.md.

## Module map

- `src/lib.rs`: `#![deny(unsafe_code)]`. Holds the pure `file_id_bytes` (`any(windows, test)`) and re-exports `ffi::*` on Windows.
- `src/ffi.rs`: Win32 FFI, `cfg(windows)`, with `#![expect(unsafe_code, reason = ...)]`.

Public items in `ffi.rs`:
- `OwnedHandle`: closes its handle on drop. `from_raw` is `unsafe`. `raw` returns the handle.
- `to_wide_nul`: NUL-terminated UTF-16 for `...W` calls.
- `open_handle(wide_path, access, share, flags)`: `CreateFileW` with `OPEN_EXISTING`. Rejects a slice without a trailing NUL with `InvalidInput`.
- `WinFileInfo` and `file_info(path)`: one attributes-only open (`FILE_FLAG_OPEN_REPARSE_POINT`, no read lock). Returns attrs, size, created, written, `change_time`, `file_id`, `nlink`, `volume_serial`. `None` when the open fails.
- `volume_root(path)`: `GetVolumePathNameW`, returned as text.
- `volume_root_cached(path)`: memoized per parent directory. `None` is memoized too. A path with no parent is not cached. The memo clears itself at `DIR_MEMO_CAP` (65,536) entries.

## How it works

- `file_id_bytes(id128, index)`: 8 bytes when the 128-bit id is the zero-extended 64-bit index (NTFS). All 16 bytes otherwise (ReFS, never truncated). With no 128-bit id, the 8-byte index.
- `file_info` takes everything from one handle, so a file costs one open. Failure means "use cached metadata", never an error.
- `volume_root_cached` takes only the read lock on a warm hit.

## Invariants and rules

- Rule: add a helper here only when more than one crate needs it. Single-use Win32 calls stay in their own module (for example `backend-windows` `caps`, `usn`, `volume_io`).
- Every `unsafe` block has a `SAFETY:` comment. `OwnedHandle::from_raw` requires a valid handle that the caller owns and that nothing else closes.
- `open_handle` checks the NUL terminator before the raw pointer is passed. Keep that check.
- `file_id_bytes` stays pure, so its tests run on every OS.
- `[lints.clippy]` in `Cargo.toml` repeats the workspace lints (`all` and `perf` deny). Cargo rejects `workspace = true` alongside per-crate overrides, so update both when workspace lints change.

## Callers

- `volume_root`: `backend-generic` `sys/windows/probe.rs`, `backend-windows` `caps.rs`.
- `volume_root_cached`: `backend-generic` `sys/windows/extents.rs`, `backend-windows` `handle.rs`.
- `open_handle`: `backend-generic` `extents.rs` and `probe.rs`, `backend-windows` `handle.rs`, `overlapped.rs`, `volume_io.rs`.
- `to_wide_nul`: `backend-generic` `extents.rs` and `probe.rs`, `backend-windows` `caps.rs`, `handle.rs`, `overlapped.rs`, `volume_io.rs`.
- `OwnedHandle`: `backend-generic` `extents.rs`, `backend-windows` `caps.rs`, `handle.rs`, `overlapped.rs`, `volume_io.rs`.
- `file_info`, `WinFileInfo`: `backend-generic` `sys/windows/meta.rs` and `mod.rs` only.

## Commands

```bash
# Lint the Windows target from macOS or Linux (compile and lint only, runs nothing)
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo clippy -p dedupe-win-sys --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings

# Tests (run only when the user asks)
cargo test -p dedupe-win-sys
```

## Tests

- `file_id_bytes` tests live in `src/lib.rs` (`file_id_tests`). They run on every OS.
- `ffi.rs` `memo_tests` runs only on Windows. It checks that `volume_root_cached` matches `volume_root` and makes one lookup per directory. It writes fixtures under the temp directory.
