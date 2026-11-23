// SPDX-License-Identifier: MPL-2.0
//! `tables.*`: the app's data tables (book `agents/data-tables.md`, `docs/specs/content-data.md`
//! § 3.5): list them, read their columns and rows, edit cells and rows, validate, and export or
//! save them as RON or CSV.
//!
//! Cells travel as RON literal texts (`"30"`, `"Rifle"`, `"(x: 1)"`); a `String` column also takes
//! plain text. An edit applies only when the edited row still parses and passes the game's row
//! rules, and bumps the table's `generation`.

use regia_asset::data_table::{Column, DataTables, TableEditError, TableFormat, TableIssue, TableSlot};
use regia_ecs::prelude::*;
use serde_json::{Map, Value, json};

use crate::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};

/// Most rows `tables.get` returns when `limit` is not given.
const DEFAULT_ROW_LIMIT: usize = 200;

fn invalid(code: &str, message: impl Into<String>, hint: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

fn issue_json(issue: &TableIssue) -> Value {
    json!({
        "code": issue.code.code(),
        "severity": issue.severity.name(),
        "message": issue.message,
        "key": issue.key,
        "column": issue.column,
        "line": issue.line,
    })
}

fn issues_json(issues: &[TableIssue]) -> Value {
    Value::Array(issues.iter().map(issue_json).collect())
}

fn column_json(column: &Column) -> Value {
    json!({
        "name": column.name,
        "type_path": column.type_path,
        "text": column.text,
        "doc": column.doc,
    })
}

fn edit_error(error: &TableEditError) -> RpcError {
    let code = error.code();
    let (rpc, hint) = match error {
        TableEditError::UnknownKey(_) => (NOT_FOUND, "tables.get lists the keys"),
        TableEditError::UnknownColumn(_) => (NOT_FOUND, "tables.describe lists the columns"),
        TableEditError::DuplicateKey(_) => (INVALID_PARAMS, "edit the existing row, or remove it first"),
        TableEditError::EmptyKey => (INVALID_PARAMS, "give the row a non-empty key"),
        TableEditError::BadCell { .. } => (
            INVALID_PARAMS,
            "values are RON literals (30, 0.5, true, Rifle, (x: 1), [\"a\"]); a String column also takes plain text",
        ),
        TableEditError::Rule(_) => (
            INVALID_PARAMS,
            "the game's row rule rejected the edited row; nothing changed",
        ),
        TableEditError::Encode(_) => (INTERNAL_ERROR, "the row type does not serialize as a struct"),
    };
    RpcError::new(rpc, code, error.to_string(), hint)
}

fn path_param(params: &Value) -> Result<&str, RpcError> {
    crate::params::opt_str(params, "path")?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            invalid(
                "bad_path",
                "`path` must be a table's asset path",
                "tables.list names the loaded tables (`tables/weapons.table.ron`)",
            )
        })
}

fn unknown_table(path: &str) -> RpcError {
    RpcError::new(
        NOT_FOUND,
        "table_unknown",
        format!("no data table is loaded at `{path}`"),
        "tables.list names the loaded tables; games add them with app.register_data_table::<Row>(path)",
    )
}

fn slot<'w>(world: &'w World, path: &str) -> Result<&'w TableSlot, RpcError> {
    world
        .get_resource::<DataTables>()
        .and_then(|tables| tables.slot(path))
        .ok_or_else(|| unknown_table(path))
}

fn slot_mut<'w>(world: &'w mut World, path: &str) -> Result<Mut<'w, DataTables>, RpcError> {
    let tables = world
        .get_resource_mut::<DataTables>()
        .ok_or_else(|| unknown_table(path))?;
    if tables.slot(path).is_none() {
        return Err(unknown_table(path));
    }
    Ok(tables)
}

fn count(issues: &[TableIssue], errors: bool) -> usize {
    issues.iter().filter(|i| i.is_error() == errors).count()
}

fn summary(path: &str, slot: &TableSlot) -> Value {
    json!({
        "path": path,
        "row_type": slot.table.row_type(),
        "format": slot.format.name(),
        "rows": slot.table.len(),
        "columns": slot.table.columns().len(),
        "generation": slot.generation,
        "reloads": slot.reloads,
        "rejected_reloads": slot.rejected_reloads,
        "cooked": slot.cooked,
        "errors": count(&slot.last_issues, true),
        "warnings": count(&slot.last_issues, false),
        "file": slot.file.as_ref().map(|f| f.display().to_string()),
    })
}

/// A cell value parameter: a JSON string is the RON text; a number or boolean is written as RON.
fn value_text(value: &Value, what: &str) -> Result<String, RpcError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) => Ok(n.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        _ => Err(invalid(
            "bad_value",
            format!(
                "`{what}` must be a string (a RON literal or, for a String column, plain text), a number or a boolean"
            ),
            "write compound values as RON text: \"(x: 1, y: 2)\", \"[\\\"a\\\"]\"",
        )),
    }
}

fn row_json(key: &str, cells: Vec<(String, String)>) -> Value {
    let mut map = Map::new();
    for (column, text) in cells {
        map.insert(column, Value::String(text));
    }
    json!({ "key": key, "cells": map })
}

/// `tables.list {}`.
pub(crate) fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let tables: Vec<Value> = world
        .get_resource::<DataTables>()
        .map(|tables| {
            tables
                .paths()
                .filter_map(|path| tables.slot(path).map(|slot| summary(path, slot)))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "tables": tables }))
}

/// `tables.describe { path }`.
pub(crate) fn describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = path_param(params)?;
    let slot = slot(world, path)?;
    let mut out = summary(path, slot);
    out["columns"] = Value::Array(slot.table.columns().iter().map(column_json).collect());
    out["keys"] = json!(slot.table.keys());
    out["last_issues"] = issues_json(&slot.last_issues);
    Ok(out)
}

/// `tables.get { path, key?, keys?, offset?, limit? }`.
pub(crate) fn get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = path_param(params)?;
    let slot = slot(world, path)?;
    let table = &slot.table;
    let wanted: Vec<String> = if let Some(key) = params.get("key").filter(|v| !v.is_null()) {
        let key = key
            .as_str()
            .ok_or_else(|| invalid("bad_key", "`key` must be a string", "row keys are strings"))?;
        vec![key.to_owned()]
    } else if let Some(keys) = params.get("keys").filter(|v| !v.is_null()) {
        keys.as_array()
            .and_then(|items| {
                items
                    .iter()
                    .map(|k| k.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or_else(|| {
                invalid(
                    "bad_key",
                    "`keys` must be an array of strings",
                    "row keys are strings",
                )
            })?
    } else {
        let offset = crate::params::opt_u64(params, "offset")?.unwrap_or(0) as usize;
        let limit = crate::params::opt_u64(params, "limit")?.map_or(DEFAULT_ROW_LIMIT, |l| l as usize);
        table.keys().into_iter().skip(offset).take(limit).collect()
    };
    let mut rows = Vec::with_capacity(wanted.len());
    for key in &wanted {
        let cells = table.row(key).map_err(|e| edit_error(&e))?;
        rows.push(row_json(key, cells));
    }
    Ok(json!({
        "path": path,
        "generation": slot.generation,
        "total": table.len(),
        "rows": rows,
    }))
}

/// `tables.set { path, key, column?, value?, cells?, remove? }`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = path_param(params)?.to_owned();
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("bad_key", "`key` must be a string", "the row to change (or add)"))?
        .to_owned();
    let remove = crate::params::opt_bool(params, "remove")?.unwrap_or(false);
    let column = params.get("column").filter(|v| !v.is_null());
    let cells = params.get("cells").filter(|v| !v.is_null());
    let modes = usize::from(remove) + usize::from(column.is_some()) + usize::from(cells.is_some());
    if modes != 1 {
        return Err(invalid(
            "bad_edit",
            "give exactly one of `column` + `value`, `cells`, or `remove: true`",
            "column + value sets one cell; cells sets several (or adds the row); remove deletes it",
        ));
    }
    let mut tables = slot_mut(world, &path)?;
    let Some(slot) = tables.slot_mut(&path) else {
        return Err(unknown_table(&path));
    };
    let mut action = "set";
    if remove {
        if !slot.table.remove_row(&key) {
            return Err(edit_error(&TableEditError::UnknownKey(key)));
        }
        action = "removed";
    } else if let Some(column) = column {
        let column = column.as_str().ok_or_else(|| {
            invalid(
                "bad_column",
                "`column` must be a string",
                "tables.describe lists the columns",
            )
        })?;
        let value = params
            .get("value")
            .ok_or_else(|| invalid("bad_value", "`value` is required with `column`", "a RON literal"))?;
        let value = value_text(value, "value")?;
        slot.table
            .set_cell(&key, column, &value)
            .map_err(|e| edit_error(&e))?;
    } else if let Some(cells) = cells {
        let object = cells.as_object().ok_or_else(|| {
            invalid(
                "bad_value",
                "`cells` must be an object { column: value }",
                "values as for `value`",
            )
        })?;
        let mut pairs = Vec::with_capacity(object.len());
        for (column, value) in object {
            pairs.push((column.clone(), value_text(value, column)?));
        }
        if slot.table.keys().iter().any(|k| k == &key) {
            // All cells or none: apply to a copy of the row's cells first.
            let before = slot.table.row(&key).map_err(|e| edit_error(&e))?;
            for (column, value) in &pairs {
                if let Err(error) = slot.table.set_cell(&key, column, value) {
                    for (column, value) in &before {
                        let _ = slot.table.set_cell(&key, column, value);
                    }
                    return Err(edit_error(&error));
                }
            }
        } else {
            slot.table.insert_row(&key, &pairs).map_err(|e| edit_error(&e))?;
            action = "added";
        }
    }
    slot.generation += 1;
    let row = if remove {
        Value::Null
    } else {
        slot.table
            .row(&key)
            .map(|cells| row_json(&key, cells))
            .unwrap_or(Value::Null)
    };
    Ok(json!({
        "path": path,
        "action": action,
        "generation": slot.generation,
        "rows": slot.table.len(),
        "row": row,
    }))
}

/// `tables.validate { path? }`.
pub(crate) fn validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let only = match params.get("path").filter(|v| !v.is_null()) {
        Some(_) => Some(path_param(params)?),
        None => None,
    };
    if let Some(path) = only {
        slot(world, path)?;
    }
    let Some(tables) = world.get_resource::<DataTables>() else {
        return Ok(json!({ "tables": [], "errors": 0, "warnings": 0 }));
    };
    let mut out = Vec::new();
    let (mut errors, mut warnings) = (0usize, 0usize);
    for path in tables.paths().filter(|p| only.is_none_or(|o| o == *p)) {
        let Some(slot) = tables.slot(path) else {
            continue;
        };
        let mut issues = slot.table.validate();
        // A table that did not load (or a refused reload) keeps those errors until fixed.
        issues.extend(slot.last_issues.iter().filter(|i| i.is_error()).cloned());
        errors += count(&issues, true);
        warnings += count(&issues, false);
        out.push(json!({
            "path": path,
            "ok": count(&issues, true) == 0,
            "issues": issues_json(&issues),
        }));
    }
    Ok(json!({ "tables": out, "errors": errors, "warnings": warnings }))
}

/// `tables.export { path, format?, write? }`.
pub(crate) fn export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = path_param(params)?.to_owned();
    let slot = slot(world, &path)?;
    let format = match params.get("format").filter(|v| !v.is_null()).map(Value::as_str) {
        None => slot.format,
        Some(Some("ron")) => TableFormat::Ron,
        Some(Some("csv")) => TableFormat::Csv,
        Some(_) => {
            return Err(invalid(
                "bad_format",
                "`format` must be \"ron\" or \"csv\"",
                "default: the table's own form",
            ));
        }
    };
    let text = slot.table.export(format).map_err(|message| {
        RpcError::new(
            INTERNAL_ERROR,
            "table_encode",
            message,
            "the row type must serialize as a struct",
        )
    })?;
    let write = crate::params::opt_bool(params, "write")?.unwrap_or(false);
    let mut written = Value::Null;
    if write {
        let Some(file) = slot.file.clone() else {
            return Err(RpcError::new(
                NOT_FOUND,
                "table_no_file",
                format!("`{path}` has no file on disk to save to"),
                "saving needs a dev build with an asset server; export without write returns the text",
            ));
        };
        if format != slot.format {
            return Err(invalid(
                "table_format_mismatch",
                format!(
                    "`{path}` is a {} file; it cannot be saved as {}",
                    slot.format.name(),
                    format.name()
                ),
                "leave out `format` to save, or export without `write` to convert",
            ));
        }
        write_atomic(&file, &text).map_err(|e| {
            RpcError::new(
                INTERNAL_ERROR,
                "table_write_failed",
                format!("{}: {e}", file.display()),
                "check that the project directory is writable",
            )
        })?;
        written = Value::String(file.display().to_string());
    }
    Ok(json!({
        "path": path,
        "format": format.name(),
        "bytes": text.len(),
        "text": text,
        "written": written,
    }))
}

/// Writes through a uniquely named sibling and a rename, so the live reload never reads a torn file.
fn write_atomic(file: &std::path::Path, text: &str) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = file.with_file_name(format!(".{name}.{}-{n}.tmp", std::process::id()));
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    if let Err(error) =
        crate::req_path::write_new_temp(&tmp, text.as_bytes()).and_then(|()| std::fs::rename(&tmp, file))
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use regia_asset::data_table::DataTable;
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Reflect, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
    struct Row {
        damage: u32,
        name: String,
        #[serde(default)]
        weight: f32,
    }

    const PATH: &str = "tables/arms.table.ron";

    fn world() -> World {
        let table = DataTable::<Row>::from_ron_str(
            r#"(format_version: 1, row_type: "Row", rows: {
                "rifle": (damage: 30, name: "Rifle"),
                "pistol": (damage: 12, name: "Pistol"),
            })"#,
        )
        .expect("table")
        .with_rule(|_, row| {
            if row.damage > 100 {
                Err("too strong".into())
            } else {
                Ok(())
            }
        });
        let mut tables = DataTables::default();
        tables.insert(PATH, table);
        let mut world = World::new();
        world.insert_resource(tables);
        world
    }

    fn code(err: RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn list_and_describe() {
        let mut world = world();
        let out = list(&mut world, &json!({})).expect("list");
        assert_eq!(out["tables"][0]["path"], PATH);
        assert_eq!(out["tables"][0]["rows"], 2);
        let out = describe(&mut world, &json!({ "path": PATH })).expect("describe");
        assert_eq!(out["columns"][0]["name"], "damage");
        assert_eq!(out["columns"][1]["text"], true);
        assert_eq!(out["keys"], json!(["rifle", "pistol"]));
        assert_eq!(
            code(describe(&mut world, &json!({ "path": "nope" })).expect_err("error")),
            "table_unknown"
        );
        assert_eq!(
            code(describe(&mut world, &json!({})).expect_err("error")),
            "bad_path"
        );
        let empty = list(&mut World::new(), &json!({})).expect("no tables");
        assert_eq!(empty["tables"], json!([]));
    }

    #[test]
    fn get_set_and_export() {
        let mut world = world();
        let out = get(&mut world, &json!({ "path": PATH, "key": "rifle" })).expect("get");
        assert_eq!(out["rows"][0]["cells"]["damage"], "30");
        assert_eq!(out["rows"][0]["cells"]["name"], "\"Rifle\"");
        let out = set(
            &mut world,
            &json!({ "path": PATH, "key": "rifle", "column": "damage", "value": 45 }),
        )
        .expect("set");
        assert_eq!(out["generation"], 1);
        assert_eq!(out["row"]["cells"]["damage"], "45");
        let err = set(
            &mut world,
            &json!({ "path": PATH, "key": "rifle", "column": "damage", "value": "lots" }),
        )
        .expect_err("error");
        assert_eq!(code(err), "table_bad_cell");
        let err = set(
            &mut world,
            &json!({ "path": PATH, "key": "rifle", "column": "damage", "value": 500 }),
        )
        .expect_err("error");
        assert_eq!(code(err), "table_rule");
        // cells: all or nothing.
        let err = set(
            &mut world,
            &json!({ "path": PATH, "key": "rifle", "cells": { "name": "Carbine", "damage": "x" } }),
        )
        .expect_err("error");
        assert_eq!(code(err), "table_bad_cell");
        let out = get(&mut world, &json!({ "path": PATH, "key": "rifle" })).expect("get");
        assert_eq!(out["rows"][0]["cells"]["name"], "\"Rifle\"", "rolled back");
        let out = set(
            &mut world,
            &json!({ "path": PATH, "key": "shotgun", "cells": { "damage": 60, "name": "Shotgun" } }),
        )
        .expect("add");
        assert_eq!(out["action"], "added");
        let out = set(
            &mut world,
            &json!({ "path": PATH, "key": "pistol", "remove": true }),
        )
        .expect("remove");
        assert_eq!(out["action"], "removed");
        assert_eq!(
            code(set(&mut world, &json!({ "path": PATH, "key": "x" })).expect_err("error")),
            "bad_edit"
        );
        assert_eq!(
            code(get(&mut world, &json!({ "path": PATH, "key": "pistol" })).expect_err("error")),
            "table_unknown_key"
        );
        let out = export(&mut world, &json!({ "path": PATH, "format": "csv" })).expect("export");
        assert_eq!(
            out["text"],
            "key,damage,name,weight\r\nrifle,45,Rifle,0.0\r\nshotgun,60,Shotgun,0.0\r\n"
        );
        let err = export(&mut world, &json!({ "path": PATH, "write": true })).expect_err("error");
        assert_eq!(code(err), "table_no_file");
        let err = export(&mut world, &json!({ "path": PATH, "format": "xml" })).expect_err("error");
        assert_eq!(code(err), "bad_format");
    }

    #[test]
    fn export_write_saves_the_file() {
        let mut world = world();
        let dir = regia_test_utils::unique_temp_dir("agent_tables_write");
        let file = dir.join("arms.table.ron");
        world
            .resource_mut::<DataTables>()
            .slot_mut(PATH)
            .expect("slot")
            .file = Some(file.clone());
        let err = export(
            &mut world,
            &json!({ "path": PATH, "format": "csv", "write": true }),
        )
        .expect_err("error");
        assert_eq!(code(err), "table_format_mismatch");
        let out = export(&mut world, &json!({ "path": PATH, "write": true })).expect("write");
        assert_eq!(out["written"], file.display().to_string());
        let saved = std::fs::read_to_string(&file).expect("saved");
        assert!(
            DataTable::<Row>::from_ron_str(&saved)
                .expect("reads back")
                .contains_key("pistol")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_reports_rules() {
        let mut world = world();
        world
            .resource_mut::<DataTables>()
            .get_mut::<Row>(PATH)
            .expect("typed")
            .insert(
                "cannon",
                Row {
                    damage: 900,
                    name: "Cannon".into(),
                    weight: 0.0,
                },
            );
        let out = validate(&mut world, &json!({})).expect("validate");
        assert_eq!(out["errors"], 1);
        assert_eq!(out["tables"][0]["issues"][0]["code"], "table_rule");
        assert_eq!(
            code(validate(&mut world, &json!({ "path": "nope" })).expect_err("error")),
            "table_unknown"
        );
    }
}
