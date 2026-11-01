//! `dedupe unmask`: turn a masked plan back into real paths, or print an export's extension map.

use std::io::Read;

use anyhow::{bail, Context, Result};
use dedupe_core::db::{queries, Db};
use dedupe_core::mask;
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::UnmaskArgs;

pub fn run(args: UnmaskArgs) -> Result<()> {
    let dir = crate::folder_or_cwd(args.dir)?;
    let db_path = if dir.is_file() {
        dir
    } else {
        crate::discover::discover_db(&dir).map_err(|e| {
            anyhow::anyhow!("{e}; scan with a DB (not --no-db) to unmask and act on a plan")
        })?
    };
    let vault_path = crate::mask::vault_path(&db_path);
    let vault = Connection::open_with_flags(&vault_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open mask vault {}", vault_path.display()))?;

    if args.ext_map {
        let (id, rows) = mask::ext_map(&vault, args.export)?;
        if args.json {
            let map: serde_json::Map<_, _> = rows
                .iter()
                .map(|(t, e)| (format!("E{t}"), json!(String::from_utf8_lossy(e))))
                .collect();
            let doc = json!({ "mask_export": id, "ext_map": map });
            println!("{}", serde_json::to_string_pretty(&doc)?);
        } else {
            for (t, e) in &rows {
                println!("E{t} -> {}", String::from_utf8_lossy(e));
            }
        }
        return Ok(());
    }

    let mut text = String::new();
    match args.file.as_deref().filter(|p| p.as_os_str() != "-") {
        Some(p) => {
            text = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?
        }
        None => {
            std::io::stdin()
                .read_to_string(&mut text)
                .context("read stdin")?;
        }
    }
    let input: Value = serde_json::from_str(&text).context("input is not valid JSON")?;
    let embedded = input.get("mask_export").and_then(Value::as_i64);
    if let (Some(flag), Some(doc)) = (args.export, embedded) {
        if flag != doc {
            bail!("--export {flag} does not match the input's mask_export {doc}");
        }
    }
    let Some(export) = args
        .export
        .or_else(|| input.get("mask_export").and_then(Value::as_i64))
    else {
        bail!("export id required: pass --export N or include a top-level mask_export");
    };

    let db = Db::open_readonly(&db_path).context("open database (read-only)")?;
    mask::check_db_uuid(&vault, export, &crate::mask::db_uuid(&db.conn)?)?;
    let mut out = mask::unmask(&input, export, &vault, &|p| {
        queries::file_in_dup_group(&db.conn, p)
    })?;
    // execute_plan.py refuses any plan that still carries mask_export.
    if let Some(obj) = out.as_object_mut() {
        obj.remove("mask_export");
    }
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
