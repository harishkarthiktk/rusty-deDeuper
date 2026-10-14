//! Duplicate-group construction and rendering (JSON + human).
//!
//! Phase 0 builds groups from an in-memory scan; Phase 1 builds them from
//! `dup_groups` (see [`crate::db::queries::recompute_groups`]).

use rusqlite::Connection;
use serde_json::json;

use crate::db::queries;
use crate::error::Result;
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

    pub fn from_db(conn: &Connection, min_wasted: i64) -> Result<Report> {
        let mut groups: Vec<Group> = Vec::new();
        for v in queries::list_volumes(conn)? {
            let Some(run_id) = queries::latest_finished_run(conn, v.id)? else {
                continue;
            };
            for g in queries::list_groups(conn, v.id, min_wasted)? {
                let mut members: Vec<Member> = Vec::new();
                for (_id, folder_id, name, size) in
                    queries::group_members(conn, v.id, &g.full_hash, run_id)?
                {
                    let path = queries::reconstruct_file_path(conn, folder_id, &name)?;
                    members.push(Member {
                        volume_id: v.id,
                        path,
                        size,
                    });
                }
                if members.len() >= 2 {
                    groups.push(Group {
                        full_hash: g.full_hash,
                        size: g.size,
                        distinct_inodes: g.distinct_inodes,
                        wasted_logical: g.wasted_logical,
                        members,
                    });
                }
            }
        }
        let mut report = Report {
            groups,
            files_enumerated: queries::total_files(conn)? as usize,
            files_skipped: 0,
            files_errored: 0,
        };
        report.normalize();
        Ok(report)
    }

    pub fn to_human(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{} duplicate group(s), {} bytes wasted (logical)\n",
            self.groups.len(),
            self.total_wasted()
        ));
        for g in &self.groups {
            out.push_str(&format!(
                "\n{}  {} bytes  x{} ({} inode(s), wasted {})\n",
                g.full_hash.to_hex(),
                g.size,
                g.members.len(),
                g.distinct_inodes,
                g.wasted_logical
            ));
            for m in &g.members {
                out.push_str(&format!("    {}\n", m.path.to_display()));
            }
        }
        out
    }
}
