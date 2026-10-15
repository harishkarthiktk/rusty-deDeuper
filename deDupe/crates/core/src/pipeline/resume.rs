//! Resume protocol (4.7): configuration guard, volume-UUID guard, mount-move
//! update, and the phase to restart from.

use std::collections::{BTreeMap, HashMap};

use rusqlite::Connection;

use crate::config::Config;
use crate::db::queries;
use crate::error::{CoreError, Result};
use crate::model::{now_unix_ns, Phase};
use crate::traits::{PathCodec, VolumeIdentity};

use super::VolumeScan;

pub struct ResumePlan {
    pub run_id: i64,
    pub volumes: Vec<VolumeScan>,
    pub phases: HashMap<i64, Phase>,
}

/// Refuse to reuse a DB written with a different hash algorithm or prefix config,
/// unless `--force`, which wipes the hash columns.
pub fn check_config_guard(conn: &Connection, cfg: &Config) -> Result<()> {
    let Some(algo) = queries::meta_get(conn, "hash_algo")? else {
        return Ok(()); // fresh DB
    };
    let fb: Option<u64> =
        queries::meta_get(conn, "prefix_first_bytes")?.and_then(|s| s.parse().ok());
    let lb: Option<u64> =
        queries::meta_get(conn, "prefix_last_bytes")?.and_then(|s| s.parse().ok());
    let mismatch = algo != cfg.hash_algo
        || fb != Some(cfg.prefix_first_bytes)
        || lb != Some(cfg.prefix_last_bytes);
    if mismatch {
        if cfg.force {
            tracing::warn!("hash config changed; wiping hashes because --force was given");
            conn.execute(
                "UPDATE files SET prefix_hash=NULL, full_hash=NULL, prefix_bytes=NULL, state=0",
                [],
            )?;
            init_meta(conn, cfg, now_unix_ns())?;
        } else {
            return Err(CoreError::HashConfigMismatch(format!(
                "db={algo}:{}/{} current={}:{}/{}",
                fb.unwrap_or(0),
                lb.unwrap_or(0),
                cfg.hash_algo,
                cfg.prefix_first_bytes,
                cfg.prefix_last_bytes
            )));
        }
    }
    Ok(())
}

pub fn init_meta(conn: &Connection, cfg: &Config, now: i64) -> Result<()> {
    if queries::meta_get(conn, "created_at")?.is_none() {
        queries::meta_set(conn, "created_at", &now.to_string())?;
    }
    queries::meta_set(conn, "tool_version", env!("CARGO_PKG_VERSION"))?;
    queries::meta_set(conn, "hash_algo", &cfg.hash_algo)?;
    queries::meta_set(
        conn,
        "prefix_first_bytes",
        &cfg.prefix_first_bytes.to_string(),
    )?;
    queries::meta_set(
        conn,
        "prefix_last_bytes",
        &cfg.prefix_last_bytes.to_string(),
    )?;
    queries::meta_set(conn, "db_uuid", &format!("{now}-{}", std::process::id()))?;
    Ok(())
}

pub fn plan_resume(
    conn: &Connection,
    cfg: &Config,
    vi: &dyn VolumeIdentity,
    codec: &dyn PathCodec,
) -> Result<ResumePlan> {
    check_config_guard(conn, cfg)?;
    let run_id = queries::latest_running_run(conn)?.ok_or(CoreError::NothingToResume)?;

    let mut phases = HashMap::new();
    let mut by_vol: BTreeMap<i64, VolumeScan> = BTreeMap::new();
    for vid in queries::volumes_for_run(conn, run_id)? {
        phases.insert(vid, queries::get_phase(conn, run_id, vid)?);
        let mount = queries::get_volume_mount(conn, vid)?
            .ok_or_else(|| CoreError::NotFound(format!("volume {vid}")))?;
        by_vol.insert(
            vid,
            VolumeScan {
                volume_id: vid,
                mount,
                roots: Vec::new(),
            },
        );
    }

    for (vid, root) in queries::roots_for_run(conn, run_id)? {
        let native = codec.raw_to_path(&root);
        let info = vi.identify(&native)?;
        let db_uuid = queries::get_volume_uuid(conn, vid)?.unwrap_or_default();
        if info.uuid != db_uuid {
            if !cfg.force {
                return Err(CoreError::VolumeMismatch {
                    mount: info.mount.to_display(),
                    db_uuid,
                    found_uuid: info.uuid,
                });
            }
            tracing::warn!(vid, "volume UUID mismatch; continuing because --force");
        } else {
            let current = queries::get_volume_mount(conn, vid)?.unwrap_or_default();
            if info.mount != current {
                tracing::warn!(vid, "volume mount moved; updating");
                queries::update_volume_mount(conn, vid, &info.mount)?;
            }
        }
        if let Some(vs) = by_vol.get_mut(&vid) {
            if !vs.roots.contains(&root) {
                vs.roots.push(root);
            }
        }
    }

    Ok(ResumePlan {
        run_id,
        volumes: by_vol.into_values().collect(),
        phases,
    })
}
