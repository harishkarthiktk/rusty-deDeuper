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
    /// A volume with a finished run starts. `groups` is its candidate group count,
    /// before the two-member guard.
    Volume { volume_id: i64, groups: u64 },
    /// Candidate groups finished on the current volume. Restarts at 1 for each volume.
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

    /// Stable ordering: by hash, then by sorted member paths.
    pub fn normalize(&mut self) {
        for g in &mut self.groups {
            g.members.sort_by(|a, b| a.path.cmp(&b.path));
        }
        self.groups.sort_by(|a, b| {
            a.full_hash
                .cmp(&b.full_hash)
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
                    "members": g.members.iter().map(|m| json!({
                        "volume_id": m.volume_id,
                        "path": m.path.to_display(),
                    })).collect::<Vec<_>>(),
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
        for (volume_id, run_id) in completed_volumes(conn)? {
            for_each_group(conn, volume_id, run_id, min_wasted, cancel, on, |g| {
                groups.push(g);
                Ok(())
            })?;
        }
        on(ReportEvent::Done);
        let files_enumerated = usize::try_from(queries::total_files(conn)?)
            .map_err(|_| CoreError::Other("negative file count".into()))?;
        let mut report = Report {
            groups,
            files_enumerated,
            files_skipped: 0,
            files_errored: 0,
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
    /// visited in id order and each volume's groups in hash order, so with several
    /// volumes the cross-volume order can differ from the buffered path.
    pub fn stream_human<W: std::io::Write>(
        conn: &Connection,
        min_wasted: i64,
        cancel: &Token,
        on: ReportProgress<'_>,
        out: &mut W,
    ) -> Result<()> {
        let volumes = completed_volumes(conn)?;
        let (mut count, mut wasted) = (0, 0);
        for (volume_id, run_id) in &volumes {
            let (c, w) = queries::group_totals(conn, *volume_id, *run_id, min_wasted)?;
            count += c;
            wasted += w;
        }
        // Reused for each block so a group is one write to `out`.
        let mut line = String::new();
        write_header(&mut line, count, wasted).expect("writing to a String cannot fail");
        out.write_all(line.as_bytes())?;
        for (volume_id, run_id) in volumes {
            for_each_group(conn, volume_id, run_id, min_wasted, cancel, on, |g| {
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

/// Builds one volume's groups in `full_hash` order and passes each to `sink`.
///
/// Emits `Volume` first, then one `Group` event per candidate group. Cancellation is
/// checked before each group.
fn for_each_group(
    conn: &Connection,
    volume_id: i64,
    run_id: i64,
    min_wasted: i64,
    cancel: &Token,
    on: &mut dyn FnMut(ReportEvent),
    mut sink: impl FnMut(Group) -> Result<()>,
) -> Result<()> {
    let metas = queries::list_groups(conn, volume_id, min_wasted)?;
    on(ReportEvent::Volume {
        volume_id,
        groups: metas.len() as u64,
    });
    // Both lists are ordered by full_hash, so one forward pass pairs them.
    let mut rows = queries::volume_group_members(conn, volume_id, run_id, min_wasted)?
        .into_iter()
        .peekable();
    let mut paths = queries::PathCache::new();
    for (index, g) in metas.into_iter().enumerate() {
        cancel.check()?;
        // Rows below this hash match no listed group (the tables changed
        // between the two reads), so skip them to keep the pairing aligned.
        while rows.next_if(|r| r.0 < g.full_hash).is_some() {}
        let mut members: Vec<Member> = Vec::new();
        while let Some((_, _, folder_id, name, size)) = rows.next_if(|r| r.0 == g.full_hash) {
            members.push(Member {
                volume_id,
                path: paths.file_path(conn, folder_id, &name)?,
                size,
            });
        }
        if members.len() >= 2 {
            members.sort_by(|a, b| a.path.cmp(&b.path));
            sink(Group {
                full_hash: g.full_hash,
                size: g.size,
                distinct_inodes: g.distinct_inodes,
                wasted_logical: g.wasted_logical,
                members,
            })?;
        }
        on(ReportEvent::Group {
            done: index as u64 + 1,
        });
    }
    Ok(())
}

/// Summary line that opens the human report.
fn write_header<W: std::fmt::Write>(
    out: &mut W,
    groups: impl std::fmt::Display,
    wasted: impl std::fmt::Display,
) -> std::fmt::Result {
    writeln!(
        out,
        "{groups} duplicate group(s), {wasted} bytes wasted (logical)"
    )
}

/// One group block: a hash line followed by one indented line per member.
fn write_group<W: std::fmt::Write>(out: &mut W, g: &Group) -> std::fmt::Result {
    writeln!(
        out,
        "\n{}  {} bytes  x{} ({} inode(s), wasted {})",
        g.full_hash.to_hex(),
        g.size,
        g.members.len(),
        g.distinct_inodes,
        g.wasted_logical
    )?;
    for m in &g.members {
        writeln!(out, "    {}", m.path.to_display())?;
    }
    Ok(())
}
