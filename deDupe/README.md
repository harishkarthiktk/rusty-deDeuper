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

| Option | What it does |
|---|---|
| `--db <PATH>` | Save the database to this path instead of the default. |
| `--no-db` | Do not save anything. Scan in memory and print the report only. |
| `--resume` | Continue the last interrupted scan from where it stopped. |
| `--force` | Ignore safety checks when the database does not match the current settings or the disk. Use with care. |
| `--cross-mounts` | Also descend into other mounted drives. Off by default. |
| `--include-placeholders` | Include cloud-only files (for example, OneDrive or iCloud placeholders). Off by default, because reading them downloads the file. |
| `--mem-budget <BYTES>` | Memory budget for the in-memory working set. |
| `--format <human\|json>` | Output format. Default: `human`. |
| `--progress <auto\|always\|never>` | Progress bar on stderr. Default: `auto` (shown only in a terminal). |

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

Prints the duplicate groups from the last completed scan. It reads the database only, so it is fast and does not touch your files.

| Option | What it does |
|---|---|
| `--db <PATH>` | Read this database. Default: `./.dedupe.db` in the current folder. |
| `--min-wasted <BYTES>` | Hide groups that waste less than this many bytes. |
| `--format <human\|json>` | Output format. |

Example output (values are illustrative):

```
2 duplicate group(s), 1234 bytes wasted (logical)

3f9c...e210  617 bytes  x2 (2 inode(s), wasted 617)
    /Users/me/Pictures/a.jpg
    /Users/me/Pictures/copy/a.jpg
```

Each group shows the content hash, the file size, how many copies exist, and the members of the group.

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

Log verbosity is a global option: `dedupe --log-level debug scan ~/Pictures`. Levels: `error`, `warn`, `info` (default), `debug`, `trace`.

## Typical workflow

```bash
dedupe scan ~/Photos --db ~/photo-index.db    # once, or after big changes
dedupe report --db ~/photo-index.db --min-wasted 1048576   # groups wasting 1 MiB or more
dedupe lookup ~/Downloads/new-photo.jpg --db ~/photo-index.db --insert
```

## Notes and limits

- **Read-only by design.** deDupe does not delete, move, or link files. Acting on the report is up to you.
- **Hard links.** On macOS and Linux, files that are hard links to the same data are counted once. On Windows, hard-link detection is not available yet.
- **Symlinks** are not followed.
- **Space numbers are logical.** "Wasted" means the size of the extra copies. Copy-on-write clones (reflinks) that share disk blocks are not yet reported separately.
- **Verification.** Byte-by-byte verification of matches (`--verify`) is planned but not available yet.
- **Platforms.** CI builds and tests on Ubuntu, macOS, and Windows. Platform-specific fast paths are planned.

## Development

See `CLAUDE.md` for the build, lint, and test commands and the code layout. The full engineering plan and status are in `../.overall_plan.md`.

## License

MIT OR Apache-2.0
