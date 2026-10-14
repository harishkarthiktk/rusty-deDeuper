-- db/schema/v1.sql
-- user_version is the single source of truth for schema version; set by schema.rs
-- after apply, not here.

CREATE TABLE meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE volumes (
  id         INTEGER PRIMARY KEY,
  uuid       TEXT NOT NULL UNIQUE,
  uuid_kind  TEXT NOT NULL,
  label      TEXT,
  mount      BLOB NOT NULL,
  fs_type    TEXT NOT NULL,
  profile    TEXT NOT NULL,
  first_seen INTEGER NOT NULL,
  last_scan  INTEGER
);

CREATE TABLE folders (
  id         INTEGER PRIMARY KEY,
  volume_id  INTEGER NOT NULL REFERENCES volumes(id),
  parent_id  INTEGER NOT NULL,
  name       BLOB NOT NULL,
  last_seen_run_id INTEGER,
  UNIQUE(volume_id, parent_id, name)
);

CREATE TABLE files (
  id            INTEGER PRIMARY KEY,
  volume_id     INTEGER NOT NULL REFERENCES volumes(id),
  folder_id     INTEGER NOT NULL REFERENCES folders(id),
  name          BLOB NOT NULL,
  file_id       BLOB,
  nlink         INTEGER NOT NULL DEFAULT 1,
  size          INTEGER NOT NULL,
  mtime_ns      INTEGER NOT NULL,
  ctime_ns      INTEGER,
  attrs         INTEGER NOT NULL,
  prefix_hash   BLOB,
  full_hash     BLOB,
  prefix_bytes  INTEGER,
  state         INTEGER NOT NULL,
  err           INTEGER,
  scanned_at    INTEGER,
  last_seen_run_id INTEGER NOT NULL,
  UNIQUE(volume_id, folder_id, name)
);

CREATE INDEX idx_files_size          ON files(volume_id, size)        WHERE state NOT IN (-1, -2);
CREATE INDEX idx_files_prefix        ON files(volume_id, prefix_hash) WHERE prefix_hash IS NOT NULL;
CREATE INDEX idx_files_full          ON files(full_hash)              WHERE full_hash IS NOT NULL;
CREATE INDEX idx_files_state         ON files(volume_id, state);
CREATE INDEX idx_files_folder        ON files(folder_id);
CREATE INDEX idx_files_fileid        ON files(volume_id, file_id)     WHERE file_id IS NOT NULL;
CREATE INDEX idx_files_seen          ON files(volume_id, last_seen_run_id);

CREATE TABLE dup_groups (
  id            INTEGER PRIMARY KEY,
  volume_id     INTEGER NOT NULL REFERENCES volumes(id),
  full_hash     BLOB NOT NULL,
  size          INTEGER NOT NULL,
  member_count  INTEGER NOT NULL,
  distinct_inodes INTEGER NOT NULL,
  wasted_logical INTEGER NOT NULL,
  wasted_physical INTEGER,
  sharing_kind  INTEGER NOT NULL DEFAULT 0,
  computed_at   INTEGER NOT NULL,
  UNIQUE(volume_id, full_hash)
);

CREATE TABLE scan_runs (
  id            INTEGER PRIMARY KEY,
  started_at    INTEGER NOT NULL,
  finished_at   INTEGER,
  cli_args      TEXT NOT NULL,
  profile       TEXT NOT NULL,
  status        TEXT NOT NULL DEFAULT 'running'
);

CREATE TABLE run_volumes (
  run_id        INTEGER NOT NULL REFERENCES scan_runs(id),
  volume_id     INTEGER NOT NULL REFERENCES volumes(id),
  phase_reached INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (run_id, volume_id)
);

CREATE TABLE run_roots (
  run_id        INTEGER NOT NULL,
  volume_id     INTEGER NOT NULL,
  root          BLOB NOT NULL,
  PRIMARY KEY (run_id, volume_id, root),
  FOREIGN KEY (run_id, volume_id) REFERENCES run_volumes(run_id, volume_id)
);

CREATE TABLE errors (
  id            INTEGER PRIMARY KEY,
  run_id        INTEGER NOT NULL REFERENCES scan_runs(id),
  file_row_id   INTEGER,
  path          BLOB,
  kind          INTEGER NOT NULL,
  message       TEXT
);
