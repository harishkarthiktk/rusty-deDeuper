# deDupe

deDupe finds duplicate files by content. It is a command-line tool written in Rust that scans one or more folders, groups files whose contents are identical, and reports how much space the extra copies use.

It never deletes anything. It only reads files and tells you what it found.

## Requirements

- Rust toolchain 1.92.0. The repo pins it in `rust-toolchain.toml`, so `rustup` installs it automatically.
- macOS, Linux, or Windows.

## Build

```bash
cargo build --release
```

The binary is at `target/release/dedupe`. To put it on your `PATH`:

```bash
cargo install --path crates/cli
```

## Quick start

```bash
# 1. Scan a folder. Prints the duplicate groups when done.
dedupe scan ~/Pictures

# 2. Look at the results again later, without rescanning.
dedupe report

# 3. Check whether a new file already exists somewhere in the index.
dedupe lookup ~/Downloads/IMG_0042.jpg
```

## Commands

### `scan`: find duplicates

```bash
dedupe scan <ROOT>...
```

Walks each folder you give it, hashes the files, and prints the duplicate groups.

By default, the results are saved to a database named `.dedupe.db` inside the first folder you scan. The database is what makes `report`, `lookup`, and `--resume` work.

Only one command can write to a database at a time. A second `scan` or `lookup --insert` on the same database stops with a "database is in use" error. The database must live on a local disk: a default location on a network share (NFS, SMB, FUSE) is refused, so pass `--db <local path>`. On WSL's `/mnt/c`, deDupe prints a note, because SQLite locking can be unreliable there. If the filesystem does not support file locking at all, `scan` and `lookup --insert` stop with an error. Pass `--no-lock` to run without the lock in that case, and make sure no other deDupe process uses the same database.

A rescan reuses a file's saved hash only when its size, modification time, change time, and file id are all unchanged. On macOS and Linux, a `chmod`, `chown`, or extended-attribute change on a file therefore makes the next scan hash it again.

| Option | What it does |
|---|---|
| `--db <PATH>` | Save the database to this path instead of the default. |
| `--no-db` | Do not save anything. Scan in memory and print the report only. |
| `--resume` | Continue the last interrupted scan from where it stopped. |
| `--force` | Ignore safety checks when the database does not match the current settings or the disk. Use with care. |
| `--no-lock` | Run without the database lock when the filesystem does not support file locking. Has no effect where locking works. |
| `--cross-mounts` | Also descend into other mounted drives. Off by default. |
| `--include-placeholders` | Include cloud-only files (for example, OneDrive or iCloud placeholders). Off by default, because reading them downloads the file. |
| `--no-mft` | Windows only: skip the NTFS `$MFT` fast path and use the standard directory walker. Ignored on other systems. See [Windows](#windows). |
| `--mem-budget <SIZE>` | Memory budget for the in-memory working set. Accepts bytes or a unit, such as `512MiB` or `1GiB`. |
| `--format <human\|json>` | Output format. Default: `human`. |
| `--progress <auto\|always\|never>` | Progress bar on stderr. Default: `auto` (shown only in a terminal). |
| `--verify <off\|byte>` | `byte` compares duplicate candidates byte for byte after hashing. Default: `off`. Ignored with `--no-db`. |
| `--profile <auto\|hdd\|ssd>` | Storage policy. `auto` checks whether the first root is on rotational media. `hdd` or `ssd` forces one. Default: `auto`. Ignored with `--no-db`. |
| `--queue-depth <N>` | Number of full-file hashes to run in parallel. Default: CPU count, capped at 8 on SSD; 1 on HDD; at most 2 when the storage type is unknown. Must be at least 1. Ignored with `--no-db`. |
| `--buf-size <SIZE>` | Read buffer for hashing, such as `64KiB` or `1MiB`. Must be 1 byte to 64MiB. Default: 64KiB on SSD, 1MiB on HDD. Ignored with `--no-db`. |

**Storage detection.** HDD/SSD detection uses sysfs on Linux, `diskutil` on macOS, and the drive's seek-penalty flag on Windows (not yet verified on Windows hardware). Network volumes, and any device the probe cannot read, are detected as unknown. Unknown storage reads 2 files at a time and prints a one-line note on stderr. The 2 is a conservative judgment value, not a measured one. Override it with `--profile hdd` or `--profile ssd`, or set the depth directly with `--queue-depth N`. External USB enclosures can be misreported by the bridge chip, so use `--profile` when the detected result looks wrong. Extent ordering is implemented on Linux and Windows (Windows not yet verified on hardware), so `--profile hdd` on macOS keeps enumeration order.

Example:

```bash
dedupe scan ~/Documents ~/Desktop --db ~/dedupe-index.db
```

If a scan is interrupted (for example, Ctrl-C or a power loss), run it again with `--resume`:

```bash
dedupe scan ~/Documents ~/Desktop --db ~/dedupe-index.db --resume
```

### `report`: show duplicate groups

```bash
dedupe report
```

Prints the duplicate groups from the last completed scan of each volume. If that scan covered only some folders, duplicates under the folders it did not cover are not shown; scan all folders together to see them. It reads the database only, so it is fast and does not touch your files.

| Option | What it does |
|---|---|
| `--db <PATH>` | Read this database. Default: `./.dedupe.db` in the current folder. |
| `--min-wasted <SIZE>` | Hide groups that waste less than this size. Accepts a plain byte count or a size with a unit, such as `10MB` or `1.5GiB`. |
| `--format <human\|json>` | Output format. |
| `--progress <auto\|always\|never>` | Draw a progress bar on stderr while the report is built. `auto` draws only on a terminal and never with `--format json`. |

Human output streams group by group as it is built, so large databases show results immediately. Press Ctrl-C to cancel.

`--min-wasted` is a per-group threshold. A group wastes `size × (copies − 1)`, counted by logical size. For example, a 5 MB file with 3 copies wastes 10 MB.

Accepted units (case-insensitive, optional space before the unit):

| Unit | Meaning | Bytes |
|---|---|---|
| none or `B` | bytes | 1 |
| `KB`, `MB`, `GB`, `TB` | decimal (powers of 1000) | 1,000 / 1,000,000 / 1,000,000,000 / 1,000,000,000,000 |
| `KiB`, `MiB`, `GiB`, `TiB` | binary (powers of 1024) | 1,024 / 1,048,576 / 1,073,741,824 / 1,099,511,627,776 |

Fractions work with a unit (`1.5GB`). The result is rounded down to whole bytes. Bare `K`, `M`, `G`, `T` are rejected as ambiguous.

```bash
dedupe report --min-wasted 10MB       # 10,000,000 bytes
dedupe report --min-wasted 1MiB       # 1,048,576 bytes
dedupe report --min-wasted 1.5GiB
dedupe report --min-wasted 1048576    # plain bytes still work
```

Example output (values are illustrative):

```
2 duplicate group(s), 1.2 KiB (1234 bytes) wasted (logical)

3f9c...e210  617 bytes  x2 (2 inode(s), wasted 617 bytes)
    /Users/me/Pictures/a.jpg
    /Users/me/Pictures/copy/a.jpg

b81d...47aa  11.7 MiB (12271312 bytes)  x2 (2 inode(s), wasted 11.7 MiB (12271312 bytes))
    /Users/me/Videos/clip.mp4
    /Users/me/Videos/backup/clip.mp4
```

Each group shows the content hash, the file size, how many copies exist, and the members of the group.
Sizes in human output use binary units (KiB, MiB, GiB, TiB) with one decimal, followed by the exact byte count once a value reaches 1 KiB. `--format json` always emits raw byte counts.

### `lookup`: check one file against the index

```bash
dedupe lookup <FILE>
```

Tells you whether a file already has a duplicate in the database. It hashes only that one file, so it is useful for checking new downloads or photos without rescanning everything.

Example output:

```
1 match(es)
    [fresh] /Users/me/Pictures/a.jpg
```

A file never matches itself, and neither does another hard link to the same data. With `--insert`, a file that is already indexed and unchanged is left as it is.

Each match has a freshness tag:

| Tag | Meaning |
|---|---|
| `fresh` | The indexed file still has the same size and modification time. |
| `stale` | The file changed since it was scanned. Rescan to refresh the index. |
| `missing` | The indexed file no longer exists at that path. |
| `unmounted` | The drive holding the indexed file is not connected right now. |

Other options:

| Option | What it does |
|---|---|
| `--db <PATH>` | Query this database. Default: `./.dedupe.db`. |
| `--insert` | Save the file and its hash into the database, so later lookups and reports include it. Creates the database if it does not exist. |
| `--no-lock` | With `--insert`, run without the database lock when the filesystem does not support file locking. |
| `--format <human\|json>` | Output format. |

Files that are not checked: empty files, symlinks, special files (pipes, sockets, devices), and cloud placeholders.

### `db`: maintain the database

```bash
dedupe db info              # schema version, group count, per-volume state counts
dedupe db vacuum            # compact the database file
dedupe db drop --yes        # delete the database and its WAL files (irreversible)
```

Each subcommand takes `--db <PATH>`. The default is `./.dedupe.db`. `drop` refuses to run without `--yes`.

## Output formats

Every command that prints results accepts `--format json`, which is useful in scripts:

```bash
dedupe report --format json | jq '.group_count'
```

Progress bars and log messages go to stderr, so stdout stays clean for piping.

In JSON, each member has a `path` string. When the real path is not valid UTF-8, `path` is lossy and the member also has `path_bytes`, the exact bytes as lowercase hex. Human output escapes control characters (shown as `\u{1b}`) so a file name cannot rewrite your terminal.

Log verbosity is a global option: `dedupe --log-level debug scan ~/Pictures`. Levels: `error`, `warn`, `info` (default), `debug`, `trace`.

## Typical workflow

```bash
dedupe scan ~/Photos --db ~/photo-index.db    # once, or after big changes
dedupe report --db ~/photo-index.db --min-wasted 1MiB   # groups wasting 1 MiB or more
dedupe lookup ~/Downloads/new-photo.jpg --db ~/photo-index.db --insert
```

## Windows

On Windows, `scan` can read the NTFS master file table (`$MFT`) directly instead of walking every folder. On a large volume this is much faster. It also gives each file a stable id, which is what lets deDupe detect hard links.

- **The fast path needs an elevated shell and an NTFS volume.** Start the terminal as administrator. When either is missing, deDupe uses the standard walker and prints a `note:` line on stderr saying why. The scan still completes.
- **`--no-mft` forces the standard walker.** Use it when you do not want deDupe to read the raw volume. The flag is accepted on every platform and ignored outside Windows.
- **File ids and hard-link detection exist only on the fast path.** The standard walker reports no file id, so it cannot tell hard links apart. Hashes saved by one mode are not reused by a rescan in the other mode, so the rescan hashes those files again.
- **Network and mapped drives.** Scanning `\\server\share` or a mapped drive uses the walker. Volume identity there is the share path plus the volume serial, so `Z:\` and `\\server\share\` are two different volumes in the database, and a `subst` drive is a separate volume from its target.
- **OneDrive folders.** The fast path and `--no-mft` both descend into cloud-backed folders. The walker can make the cloud provider populate an empty cloud folder; the fast path sees only the files already local.
- **The fast path never trades correctness for speed.** If any `$MFT` record cannot be read twice in a row, the scan root is not a directory, or `$MFT` itself is split across extension records, deDupe uses the standard walker and says why on stderr.
- **Not yet verified on real Windows.** The Windows code is cross-compiled and linted from macOS, and its portable parts are unit-tested there. The raw-volume read has not run on a Windows machine. Check a scan of a folder you know before relying on the results.

## Notes and limits

- **Read-only by design.** deDupe does not delete, move, or link files. Acting on the report is up to you.
- **Hard links.** On macOS and Linux, files that are hard links to the same data are counted once. On Windows, this works only on the NTFS fast path; see [Windows](#windows).
- **Symlinks** are not followed.
- **Space numbers are logical.** "Wasted" means the size of the extra copies. Copy-on-write clones (APFS `clonefile`, btrfs/XFS reflinks, ReFS block clones) share disk blocks, so deleting one of them may free little or no space.
- **Reports cover every completed scan.** `report` shows the groups from all completed scans of each volume, including folders that a later scan did not cover. Files deleted from a folder you have not rescanned stay listed until you rescan it. If a scan is interrupted, `report` warns that files it already touched are missing until `scan --resume` finishes it.
- **Unreadable folders.** A folder the scanner cannot read keeps its previously indexed rows instead of being treated as deleted, and the failure is recorded as an error.
- **Verification.** Byte-by-byte verification of matches (`--verify`) is planned but not available yet.
- **Platforms.** There is no CI; builds and tests are run by hand on Linux, macOS, and Windows as needed. The NTFS fast path on Windows is described in [Windows](#windows).

## Development

See `CLAUDE.md` for the build, lint, and test commands and the code layout. The full engineering plan and status are in `../.overall_plan.md`.

## License

MIT OR Apache-2.0
