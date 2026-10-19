//! Duplicate-group construction and rendering (JSON + human).
//!
//! Phase 0 builds groups from an in-memory scan; Phase 1 builds them from
//! `dup_groups` (see [`crate::db::queries::recompute_groups`]).

use rusqlite::Connection;
use serde_json::json;

use crate::cancel::Token;
use crate::db::queries;
use crate::error::{CoreError, Result};
use crate::model::Hash;
use crate::path::RawPath;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Member {
    pub volume_id: i64,
    pub path: RawPath,
    pub size: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub full_hash: Hash,
    pub size: i64,
    pub distinct_inodes: i64,
    pub wasted_logical: i64,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub groups: Vec<Group>,
    pub files_enumerated: usize,
    pub files_skipped: usize,
    pub files_errored: usize,
}

/// Progress emitted while a report is built. Core only emits these; `cli` draws them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReportEvent {
    /// A volume with a finished run starts. `groups` is the exact number of groups
    /// that will be emitted for it.
    Volume { volume_id: i64, groups: u64 },
    /// Groups finished on the current volume. Restarts at 1 for each volume.
    Group { done: u64 },
    /// The whole report is built.
    Done,
}

/// Receives [`ReportEvent`]s on the building thread, so the callback should stay cheap.
pub type ReportProgress<'a> = &'a mut dyn FnMut(ReportEvent);

impl Report {
    pub fn total_wasted(&self) -> i64 {
        self.groups.iter().map(|g| g.wasted_logical).sum()
    }

    /// Stable ordering: most wasted bytes first, then by hash, then by sorted member paths.
    pub fn normalize(&mut self) {
        for g in &mut self.groups {
            g.members.sort_by(|a, b| a.path.cmp(&b.path));
        }
        self.groups.sort_by(|a, b| {
            b.wasted_logical
                .cmp(&a.wasted_logical)
                .then_with(|| a.full_hash.cmp(&b.full_hash))
                .then_with(|| a.size.cmp(&b.size))
                .then_with(|| a.members.cmp(&b.members))
        });
    }

    pub fn to_json(&self) -> String {
        let groups: Vec<_> = self
            .groups
            .iter()
            .map(|g| {
                json!({
                    "full_hash": g.full_hash.to_hex(),
                    "size": g.size,
                    "member_count": g.members.len(),
                    "distinct_inodes": g.distinct_inodes,
                    "wasted_logical": g.wasted_logical,
                    "members": g.members.iter().map(|m| {
                        let mut o = serde_json::Map::new();
                        o.insert("volume_id".into(), m.volume_id.into());
                        m.path.json_fields(&mut o);
                        serde_json::Value::Object(o)
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        let doc = json!({
            "group_count": self.groups.len(),
            "wasted_logical_total": self.total_wasted(),
            "files_enumerated": self.files_enumerated,
            "files_skipped": self.files_skipped,
            "files_errored": self.files_errored,
            "groups": groups,
        });
        serde_json::to_string_pretty(&doc).expect("report serializes")
    }

    /// Buffered report without progress events or cancellation.
    pub fn from_db(conn: &Connection, min_wasted: i64) -> Result<Report> {
        Self::from_db_with(conn, min_wasted, &Token::new(), &mut |_: ReportEvent| {})
    }

    /// Buffered report with progress events and cooperative cancellation.
    pub fn from_db_with(
        conn: &Connection,
        min_wasted: i64,
        cancel: &Token,
        on: ReportProgress<'_>,
    ) -> Result<Report> {
        let mut groups: Vec<Group> = Vec::new();
        let (mut enumerated, mut skipped, mut errored) = (0_i64, 0_i64, 0_i64);
        for (volume_id, run_id) in completed_volumes(conn)? {
            let pending = load_groups(conn, volume_id, run_id, min_wasted)?;
            emit_groups(conn, volume_id, pending, cancel, on, |g| {
                groups.push(g);
                Ok(())
            })?;
            let (e, s, x) = queries::run_file_counts(conn, volume_id, run_id)?;
            enumerated += e;
            skipped += s;
            errored += x;
        }
        on(ReportEvent::Done);
        let to_count =
            |n: i64| usize::try_from(n).map_err(|_| CoreError::Other("negative file count".into()));
        let mut report = Report {
            groups,
            files_enumerated: to_count(enumerated)?,
            files_skipped: to_count(skipped)?,
            files_errored: to_count(errored)?,
        };
        report.normalize();
        Ok(report)
    }

    pub fn to_human(&self) -> String {
        let mut out = String::new();
        write_header(&mut out, self.groups.len(), self.total_wasted())
            .expect("writing to a String cannot fail");
        for g in &self.groups {
            write_group(&mut out, g).expect("writing to a String cannot fail");
        }
        out
    }

    /// Human report written group by group as it is built.
    ///
    /// Writes the same bytes as [`Report::to_human`] within one volume. Volumes are
    /// visited in id order and each volume's groups in wasted-bytes order, so with
    /// several volumes the cross-volume order can differ from the buffered path.
    pub fn stream_human<W: std::io::Write>(
        conn: &Connection,
        min_wasted: i64,
        cancel: &Token,
        on: ReportProgress<'_>,
        out: &mut W,
    ) -> Result<()> {
        let mut loaded = Vec::new();
        for (volume_id, run_id) in completed_volumes(conn)? {
            cancel.check()?;
            loaded.push((volume_id, load_groups(conn, volume_id, run_id, min_wasted)?));
        }
        let count: usize = loaded.iter().map(|(_, g)| g.len()).sum();
        let wasted: i64 = loaded
            .iter()
            .flat_map(|(_, g)| g)
            .map(|g| g.wasted_logical)
            .sum();
        // Reused for each block so a group is one write to `out`.
        let mut line = String::new();
        write_header(&mut line, count, wasted).expect("writing to a String cannot fail");
        out.write_all(line.as_bytes())?;
        for (volume_id, groups) in loaded {
            emit_groups(conn, volume_id, groups, cancel, on, |g| {
                line.clear();
                write_group(&mut line, &g).expect("writing to a String cannot fail");
                out.write_all(line.as_bytes())?;
                Ok(())
            })?;
        }
        on(ReportEvent::Done);
        out.flush()?;
        Ok(())
    }
}

/// Id of a `running` scan newer than every finished run, if any. While one exists,
/// files it already re-enumerated are missing from the report until it completes.
pub fn unfinished_newer_run(conn: &Connection) -> Result<Option<i64>> {
    let running = queries::latest_running_run(conn)?;
    let newest_finished = completed_volumes(conn)?.into_iter().map(|(_, r)| r).max();
    Ok(running.filter(|r| newest_finished.is_none_or(|f| *r > f)))
}

/// `(volume id, run id)` for every volume with a finished run, in id order.
fn completed_volumes(conn: &Connection) -> Result<Vec<(i64, i64)>> {
    let mut out = Vec::new();
    for v in queries::list_volumes(conn)? {
        if let Some(run_id) = queries::latest_finished_run(conn, v.id)? {
            out.push((v.id, run_id));
        }
    }
    Ok(out)
}

/// One group's rows before path resolution. Stats come from `members`, never from
/// the stored `dup_groups` columns, so header, group line and member list agree.
struct PendingGroup {
    full_hash: Hash,
    size: i64,
    distinct_inodes: i64,
    wasted_logical: i64,
    members: Vec<(i64, RawPath)>, // (folder_id, name)
}

/// Folds adjacent rows with the same `(full_hash, size)` into groups, keeps groups
/// with more than one distinct inode and at least `min_wasted` wasted bytes, and
/// orders them most-wasted first, then by hash and size.
fn fold_groups(rows: Vec<queries::GroupMemberRow>, min_wasted: i64) -> Vec<PendingGroup> {
    let mut out = Vec::new();
    let mut it = rows.into_iter().peekable();
    while let Some(first) = it.next() {
        let (full_hash, size) = (first.full_hash, first.size);
        let mut inodes = vec![first.inode];
        let mut members = vec![(first.folder_id, first.name)];
        while let Some(r) = it.next_if(|r| r.full_hash == full_hash && r.size == size) {
            inodes.push(r.inode);
            members.push((r.folder_id, r.name));
        }
        inodes.sort_unstable();
        inodes.dedup();
        let distinct_inodes = inodes.len() as i64;
        let wasted_logical = size.saturating_mul(distinct_inodes - 1);
        if distinct_inodes > 1 && wasted_logical >= min_wasted {
            out.push(PendingGroup {
                full_hash,
                size,
                distinct_inodes,
                wasted_logical,
                members,
            });
        }
    }
    out.sort_by(|a, b| {
        b.wasted_logical
            .cmp(&a.wasted_logical)
            .then_with(|| a.full_hash.cmp(&b.full_hash))
            .then_with(|| a.size.cmp(&b.size))
    });
    out
}

/// One volume's groups, read in a single statement (one snapshot).
fn load_groups(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
    min_wasted: i64,
) -> Result<Vec<PendingGroup>> {
    let rows = queries::volume_group_members(conn, volume_id, run_id, min_wasted)?;
    Ok(fold_groups(rows, min_wasted))
}

/// Resolves paths and hands each group to `sink`. Emits `Volume` with the exact
/// group count, then one `Group` per group; checks cancellation before each group.
fn emit_groups(
    conn: &Connection,
    volume_id: i64,
    groups: Vec<PendingGroup>,
    cancel: &Token,
    on: &mut dyn FnMut(ReportEvent),
    mut sink: impl FnMut(Group) -> Result<()>,
) -> Result<()> {
    on(ReportEvent::Volume {
        volume_id,
        groups: groups.len() as u64,
    });
    let mut paths = queries::PathCache::new();
    for (index, g) in groups.into_iter().enumerate() {
        cancel.check()?;
        let mut members = g
            .members
            .into_iter()
            .map(|(folder_id, name)| {
                Ok(Member {
                    volume_id,
                    path: paths.file_path(conn, folder_id, &name)?,
                    size: g.size,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        members.sort_by(|a, b| a.path.cmp(&b.path));
        sink(Group {
            full_hash: g.full_hash,
            size: g.size,
            distinct_inodes: g.distinct_inodes,
            wasted_logical: g.wasted_logical,
            members,
        })?;
        on(ReportEvent::Group {
            done: index as u64 + 1,
        });
    }
    Ok(())
}

/// Formats a byte count for people: binary units with one decimal, plus the exact
/// count once it reaches 1 KiB (`11.7 MiB (12271312 bytes)`); smaller values are
/// just `617 bytes`. Negative values clamp to 0.
fn fmt_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    let bytes = bytes.max(0);
    if bytes < 1024 {
        return format!("{bytes} bytes");
    }
    // Precision loss above 2^53 is irrelevant for a one-decimal display.
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {} ({bytes} bytes)", UNITS[unit])
}

/// Summary line that opens the human report.
fn write_header<W: std::fmt::Write>(
    out: &mut W,
    groups: impl std::fmt::Display,
    wasted: i64,
) -> std::fmt::Result {
    writeln!(
        out,
        "{groups} duplicate group(s), {} wasted (logical)",
        fmt_size(wasted)
    )
}

/// One group block: a hash line followed by one indented line per member.
fn write_group<W: std::fmt::Write>(out: &mut W, g: &Group) -> std::fmt::Result {
    writeln!(
        out,
        "\n{}  {}  x{} ({} inode(s), wasted {})",
        g.full_hash.to_hex(),
        fmt_size(g.size),
        g.members.len(),
        g.distinct_inodes,
        fmt_size(g.wasted_logical)
    )?;
    for m in &g.members {
        writeln!(out, "    {}", m.path.to_terminal())?;
    }
    Ok(())
}
