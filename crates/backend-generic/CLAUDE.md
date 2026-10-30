# crates/backend-generic (`dedupe-backend-generic`)

Portable backend: walkdir enumeration, positional file reads, volume identity, storage probe, and the path codec. It implements the `core` traits `Enumerator`, `ContentReader`, `VolumeIdentity`, `StorageProbe`, and `PathCodec`, and `GenericBackend` bundles them. Linux and macOS use it directly. `backend-windows` imports `WalkdirEnumerator`, `FsContentReader`, and `GenericPathCodec` for its fallback and read path. `cli` (`platform/`, `scan.rs`, `lookup.rs`) and `bench` consume it. It depends on `dedupe-core` and, on Windows only, `dedupe-win-sys`. For workspace layering and cross-crate rules, see the root CLAUDE.md.

## Module map

Facade modules hold no target `cfg` and no `unsafe`. They call `crate::sys::Os::<fn>` or re-export `sys::exports`.

- `src/lib.rs`: `#![deny(unsafe_code)]`, module list, re-exports, `GenericBackend` (`new()` builds all five trait objects). `parse` and `sys` are private.
- `src/codec.rs`: `GenericPathCodec`, the `PathCodec` impl (native <-> `RawPath`).
- `src/meta.rs`: `file_meta_from` (shared `Metadata` -> `FileMeta`), `file_meta_at` (Windows handle path, else `file_meta_from`).
- `src/probe.rs`: `GenericStorageProbe`, `FsLocality` enum, `fs_locality()`.
- `src/volume.rs`: `GenericVolumeIdentity`.
- `src/reader.rs`: `FsContentReader`, `FsHandle`, and `pub(crate) open_regular` (the common open for reads and for the Linux FIEMAP lookup).
- `src/walk.rs`: `WalkdirEnumerator`, `walk_error`, `file_meta_for`.
- `src/extents.rs`: re-export of `first_extent_offset`, present on Linux and Windows only.
- `src/sys/mod.rs`: `pub(crate) trait Platform` (14 functions), the cfg-selected `pub(crate) type Os`, and `pub(crate) mod exports` (items that exist on some targets only: `parse::wtf8` on Windows, `first_extent_offset` on Linux and Windows, `file_meta_from_info` on Windows).
- `src/sys/linux/`: `Linux` impl. `mod.rs` (trait glue), `probe.rs` (sysfs `rotational`, mountinfo fallback for anonymous devices, `statfs` locality), `volume.rs` (filesystem UUID from `/dev/disk/by-uuid`, else fsid), `extents.rs` (`FS_IOC_FIEMAP`).
- `src/sys/macos/`: `Macos` impl. `probe.rs` (`diskutil info -plist` `SolidState`, `statfs` locality), `volume.rs` (`ATTR_VOL_UUID` via `getattrlist`, else fsid).
- `src/sys/windows/`: `Windows` impl. `probe.rs` (seek-penalty ioctl, `GetDriveTypeW` for remote), `volume.rs` (volume GUID, else serial), `extents.rs` (`FSCTL_GET_RETRIEVAL_POINTERS`), `meta.rs` (`file_meta_at`, `file_meta_from_info`, attribute bits).
- `src/sys/unix.rs`: `#[cfg(unix)]` helpers shared by Linux, macOS, and unix targets on the `other` path (`identity_bits`, `ctime_ns`, `configure_open`, `after_open`, `read_once`, path bytes, `mount_root`, `output_with_timeout`).
- `src/sys/other.rs`: `Other` impl for targets that are not Linux, macOS, or Windows. Probe and extents are unknown. `identify` returns `none:other:<path>`.
- `src/sys/tests/`: unit tests of facade behaviour that varies by target. `meta.rs` and `reader.rs` compile on every target (some cases are `cfg(unix)`). `probe.rs` is unix only. `volume.rs` is Linux and macOS only.
- `src/parse/`: pure parsers (`sysfs`, `mountinfo`, `plist`, `seek_penalty`, `wide`, `device_path`, `extents`, `wtf8`). No I/O, no `unsafe`. Each is gated by target OS or `test`. `mountinfo` is Linux only. All others compile under `cfg(test)` on every OS, so their tests run on every OS.
- `tests/walk.rs`: walker contract tests (`#![cfg(unix)]`).

## How it works

1. `cli` builds the backend through `platform/generic.rs` (or `GenericBackend::new()`). The core pipeline calls the traits; the facade picks the OS impl at compile time.
2. Walk: `WalkdirEnumerator::walk` uses `follow_links(false)`, optional `same_file_system`, and `filter_entry` to prune excluded directories before they are read. The root (depth 0) is never pruned. Directories are yielded before their children as `WalkItem::Dir`. Files get `WalkItem::File` with metadata from `Os::walk_file_meta`. Walkdir errors become `WalkItem::Error`.
3. Read: `FsContentReader::open` or `read_at` calls `open_regular`: `Os::configure_open`, `open`, a regular-file check, then `Os::after_open`. Reads go through `Os::read_once` and retry `Interrupted`. The `opens` counter increments once per open.
4. Metadata: `ContentReader::metadata` uses `symlink_metadata` (lstat) and then `meta::file_meta_at`.
5. Volume: `identify` returns `VolumeInfo` (uuid, `UuidKind`, mount, `FsType`) for the resume guard in core.
6. Storage: `is_rotational` returns `Option<bool>`. `None` means unknown, and core then uses SSD policy.
7. Extent offset: `first_extent_hint` orders HDD reads. Errors become `None`.

## Invariants and rules

- Every OS-specific behaviour goes through `Platform`. Adding a behaviour means adding a `Platform` function, implementing it in every OS module, and calling `Os::<fn>` from a facade. Facades never import `sys::linux` or `sys::windows` directly, except through `exports`.
- `Os` is static dispatch. A missing `Platform` method fails only that target's build. Linux `cargo check` does not compile `macos/` or `other.rs`, so a missing macOS method is found only on a Mac.
- `unsafe` policy: `lib.rs` denies `unsafe_code`. A sys module that needs `unsafe` opens with `#![expect(unsafe_code, reason = "...")]`. Current users: `sys/unix.rs`, `sys/linux/extents.rs`, `sys/macos/probe.rs`, `sys/macos/volume.rs`, `sys/windows/extents.rs`, `sys/windows/probe.rs`, `sys/windows/volume.rs`. Test-only `mkfifo` in `sys/tests/reader.rs` uses a function-level `#[expect]`. Use `expect`, not `allow`, so a stale exception is reported. Each `unsafe` block should carry a `SAFETY:` comment. `undocumented_unsafe_blocks` warns when one is missing (`Cargo.toml`).
- Target `cfg` appears only under `sys/` and `parse/`. `cargo run -p dedupe-xtask -- boundaries` checks this (see root CLAUDE.md).
- Enumeration never follows links: `follow_links(false)`. On Unix, `open` uses `O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC`, and `O_NONBLOCK` is cleared after open so a swapped-in FIFO cannot hang a worker. `open_regular` rejects non-regular files with `InvalidInput`.
- Reads are positional (`FileExt::read_at` on Unix, `seek_read` on Windows), so the handle has no cursor. The non-Unix fallback in `other.rs` uses seek plus read and does keep cursor state there.
- Extent offsets and volume-probe results are hints. A failed lookup returns `None`, never an error, so a scan does not abort. Open failures in `first_extent_offset` are also folded to `None` by the Linux and Windows `Platform` impls.
- Identity: Unix `file_id` is the inode as little-endian bytes, and `dev` is `st_dev`. Windows `file_id`, `dev`, and `nlink` come from one `file_info` handle open. If that open fails, `file_id` is `None`, `nlink` is 1, and `dev` is `None`.
- Times: an out-of-range or missing mtime becomes `0`. Pre-1970 mtimes are negative.
- Win32: handle and path helpers (`open_handle`, `to_wide_nul`, `volume_root`, `volume_root_cached`, `file_info`, `OwnedHandle`) come from `dedupe-win-sys`. Some calls are direct `windows-sys` calls in this crate (`GetVolumePathNameW`, `GetDriveTypeW`, `DeviceIoControl`, `GetDiskFreeSpaceW`). A new Win32 item needs its feature added under `[target.'cfg(windows)'.dependencies]` in `Cargo.toml`.

## Gotchas

- Storage probe: Linux reads sysfs, macOS asks `diskutil`, Windows reads the seek-penalty ioctl. None of these is verified on hardware. Network volumes report unknown. `diskutil` and `zpool` run under `output_with_timeout` (5 s). `diskutil` is called by absolute path `/usr/sbin/diskutil`.
- `diskutil` reports `SolidState` (the inverse). `macos/probe.rs` inverts it once at the trait boundary.
- Linux anonymous devices (major 0: btrfs, bcachefs, zfs, overlay) resolve through `/proc/self/mountinfo`. Overlay recurses one level. Deeper is unknown.
- `fs_locality`: NFS, SMB, SMB2, CIFS, FUSE, and Ceph are `Remote`. The 9p magic is `Virtualized` (WSL). A failed `statfs` is `Unknown`. macOS treats a missing `MNT_LOCAL` as `Remote`. Windows treats `DRIVE_REMOTE` as `Remote`. The CLI policy that acts on these values (refusing remote default DBs, WSL note) is in `crates/cli/CLAUDE.md`.
- Walker errors: `WalkItem::Error` carries the path, `is_dir`, and an error kind (`Permission` or `Io`). Walkdir errors without a path are skipped. A walkdir error whose path cannot be stat'ed is treated as a directory. Core passes unreadable directories to `Batch::DeleteStale` as protected folders, so their old rows survive. `is_dir` decides whether core protects the folder. A wrong `false` lets stale deletion remove rows under a directory that was never read.
- Windows walker: `walk_file_meta` opens each regular file once through `dedupe_win_sys::file_info` (used by `file_meta_at` in `sys/windows/meta.rs`) and takes identity, size, times, and attributes from that open. `ctime` is the `FILE_BASIC_INFO` ChangeTime. The std `Metadata` path has no ChangeTime, so `ctime_ns` falls back to creation time there.
- `dev` on Windows is `Some(volume serial)` only when the handle open succeeds. The fallback path leaves it `None`.
- Linux `identify` uses the fs UUID when present. Otherwise it uses the fsid, and if the fsid is zero, `none:<fstype>:<source>` from mountinfo (or the mount path). `none:` is a weak resume guard and logs a warning.
- Linux `fs_type_from_magic` values are from memory. The source comment says to confirm them against the kernel headers.
- macOS `mount` in `VolumeInfo` is the dev-climbed path, but `getattrlist` needs the volume root from `statfs`. Firmlinked roots are a known follow-up (GEN-14).
- macOS `platform_attr_bits` maps `SF_DATALESS` to `attrs::PLACEHOLDER`. Reading such a file triggers an iCloud download.
- Windows attribute bits: offline and recall-on-open or recall-on-data-access map to `PLACEHOLDER`.
- Build coverage: `macos/`, `other.rs`, MSVC, and aarch64 are never compiled on the Linux host. Check them on a Mac or a Windows machine. The root Commands cross-check only covers `x86_64-pc-windows-gnu`.
- Windows code is compile-checked and linted from the dev host only. The ungated cases in `sys/tests/reader.rs` run the Windows open and positional-read paths when the test binary runs on Windows. Run the tests there before trusting the Windows branch.

## Commands

Run these scoped to this crate. Lint and format checks run without asking. Tests run only when the user asks (see root CLAUDE.md).

```bash
# Lint and format (allowed without asking)
cargo fmt -p dedupe-backend-generic --check
cargo clippy -p dedupe-backend-generic --all-targets --all-features -- -D warnings

# Windows cross-check for this crate (compile and lint only; same env as root)
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar cargo check -p dedupe-backend-generic --all-targets --locked --target x86_64-pc-windows-gnu

# Tests (user request only; use a subagent)
cargo test -p dedupe-backend-generic --lib parse::          # pure parsers
cargo test -p dedupe-backend-generic --lib sys::            # sys unit tests and sys::tests
cargo test -p dedupe-backend-generic --test walk            # walker contract (Unix only)
```

## Tests

- Inline `#[cfg(test)]` modules: every `src/parse/*.rs` file, `sys/unix.rs` (`output_with_timeout`, `mount_root`, byte-exact codec round trip), `sys/linux/extents.rs` (FIEMAP flag handling, temp files), `sys/linux/volume.rs` (`fs_type_from_magic`), and `sys/macos/volume.rs` (`format_uuid`).
- `src/sys/tests/`: facade-level tests. `meta.rs` covers `file_meta_from` kinds, identity, and time mapping. `reader.rs` covers open counting, positional reads, symlink refusal, and FIFO refusal (Unix). `probe.rs` covers `fs_locality` on Unix. `volume.rs` covers `identify` on Linux and macOS.
- `tests/walk.rs`: eight walker contract tests (exclude pruning, root not pruned, dir before children, symlink not followed, `nlink` and hardlink bit, error item for missing root, error item and skipped children for unreadable subdir). Each builds its own tree under `std::env::temp_dir()`.
- No `#[ignore]` tests exist in this crate.
- The Windows branch is compile-checked from the dev host, and the macOS branch is not compiled there. Run the test commands on each OS before trusting its branch.

## How to add or change OS behaviour

1. Add the function to `trait Platform` in `src/sys/mod.rs`, with a doc comment that names the facade caller.
2. Implement it in every OS module: `sys/linux/mod.rs`, `sys/macos/mod.rs`, `sys/windows/mod.rs`, and `sys/other.rs`. Use `sys/unix.rs` helpers where Unix behaviour is shared.
3. Call `Os::<fn>` from the facade. Do not add a `cfg` to the facade.
4. Put any pure parsing in `src/parse/` with a `#[cfg]` that includes `test`, and add unit tests there.
5. Run the Windows cross-check and check the macOS impl on a Mac.

New OS: create `sys/<os>/` with a `Platform` impl, add a `#[cfg(target_os = "<os>")]` arm to `sys/mod.rs` for `mod`, the `Os` alias, and any `exports` entries, and move it out of `other.rs`. Follow the `unsafe` policy above.
