// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of curve tables (`docs/specs/51-curve-tables-and-choosers.md` § 5.1,
//! ADR-1649): the `curves.*` methods over `regia_framework::curve_table::CurveTables`.
//!
//! Extension methods: a host that adds `CurveTablePlugin` calls [`install_curve_table_methods`]
//! once to register them on the world's [`AgentExtension`]. Without the plugin every method
//! answers `{ "available": false }`. Edits change the in-memory table and bump its generation;
//! `curves.export { write: true }` saves it. Documented on `book/agents/curve-tables.md`.
//!
//! Errors are `F0002` (bad params) or `F0003` (not found) with `data.reason`:
//! `curve_table_not_found`, `curve_not_found`, `key_not_found`, `bad_value`, `validation_failed`,
//! `read_only_asset`, or a `curve_*` validator code. A failed write is `F0001` with `data.reason`
//! `io_failed`.

use serde_json::{Map, Value, json};

use regia_app::prelude::*;
use regia_framework::curve_table::{
    CurveIssue, CurveKey, CurveTable, CurveTables, Extrap, NamedCurve, TableInterp,
};

use crate::extension::AgentExtension;
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, ParamSchema, Permission};

const HINT: &str = "see book/agents/curve-tables.md";

/// Most points `curves.sample` returns.
pub const MAX_SAMPLE_POINTS: u64 = 4096;

fn error(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": HINT })),
    }
}

fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::INVALID_PARAMS, "F0002", reason, message)
}

fn missing(reason: &str, message: impl Into<String>) -> RpcError {
    error(protocol::NOT_FOUND, "F0003", reason, message)
}

/// A refused edit or load: the first error's code as the reason, every issue in `data.issues`.
fn refused(issues: &[CurveIssue]) -> RpcError {
    let reason = issues
        .iter()
        .find(|i| i.is_error())
        .map_or("validation_failed", |i| i.code);
    let code = if matches!(
        reason,
        "curve_not_found" | "key_not_found" | "curve_table_not_found"
    ) {
        (protocol::NOT_FOUND, "F0003")
    } else {
        (protocol::INVALID_PARAMS, "F0002")
    };
    let message = issues
        .first()
        .map_or_else(|| "refused".to_owned(), ToString::to_string);
    RpcError {
        code: code.0,
        message,
        data: Some(json!({ "code": code.1, "reason": reason, "hint": HINT, "issues": issues_json(issues) })),
    }
}

/// The issues as `[{ code, severity, message, curve, line }]`.
#[must_use]
pub fn issues_json(issues: &[CurveIssue]) -> Value {
    Value::Array(
        issues
            .iter()
            .map(|i| {
                json!({ "code": i.code, "severity": i.severity.name(), "message": i.message, "curve": i.curve, "line": i.line })
            })
            .collect(),
    )
}

const fn param(
    name: &'static str,
    kind: &'static str,
    required: bool,
    description: &'static str,
) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const PATH: ParamSchema = param(
    "path",
    "string",
    true,
    "asset path of the table (`tables/falloff.curves.ron`)",
);
const NAME: ParamSchema = param("name", "string", true, "curve name");
const ONLY_PATH: &[ParamSchema] = &[PATH];
const PATH_NAME: &[ParamSchema] = &[PATH, NAME];
const SAMPLE_PARAMS: &[ParamSchema] = &[
    PATH,
    NAME,
    param("x", "number", false, "one position"),
    param("from", "number", false, "first position of a sweep"),
    param("to", "number", false, "last position of a sweep"),
    param("count", "number", false, "points in the sweep (1..=4096)"),
];
const SET_KEY_PARAMS: &[ParamSchema] = &[
    PATH,
    NAME,
    param("x", "number", true, "key position"),
    param("y", "number", true, "key value"),
    param(
        "interp",
        "string",
        false,
        "step | linear | cubic for the segment starting here",
    ),
    param("in_tangent", "number", false, "explicit incoming tangent"),
    param("out_tangent", "number", false, "explicit outgoing tangent"),
];
const REMOVE_KEY_PARAMS: &[ParamSchema] = &[PATH, NAME, param("x", "number", true, "key position")];
const SET_CURVE_PARAMS: &[ParamSchema] = &[
    PATH,
    NAME,
    param(
        "keys",
        "array",
        false,
        "[{ x, y, interp?, in_tangent?, out_tangent? }] or [[x, y], ...]",
    ),
    param(
        "pre",
        "string",
        false,
        "hold | linear | cycle | cycle_offset | ping_pong",
    ),
    param("post", "string", false, "same as pre"),
    param(
        "interp",
        "string",
        false,
        "step | linear | cubic, or null for the table default",
    ),
    param("rename", "string", false, "new name"),
];
const EXPORT_PARAMS: &[ParamSchema] = &[
    PATH,
    param("format", "string", false, "ron (default) | csv"),
    param("write", "boolean", false, "save to the table's file (atomic)"),
];
const IMPORT_PARAMS: &[ParamSchema] = &[
    PATH,
    param("csv", "string", true, "CSV text"),
    param("interp", "string", false, "step | linear (default) | cubic"),
];

fn schema(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
    permission: Permission,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating: permission != Permission::Read,
        permission,
    }
}

/// The `curves.*` schema rows with their handlers.
fn methods() -> Vec<(MethodSchema, crate::extension::AgentHandler)> {
    vec![
        (
            schema(
                "curves.list",
                "curves_list",
                "Loaded curve tables: path, interpolation, curve count, generation, errors, warnings, file.",
                &[],
                "{ available, tables: [{ path, interp, curves, generation, errors, warnings, file, cooked }] }",
                Permission::Read,
            ),
            curves_list,
        ),
        (
            schema(
                "curves.describe",
                "curves_describe",
                "One curve table: every curve's key count, extrapolation, interpolation and ranges, plus the last issues.",
                ONLY_PATH,
                "{ path, interp, generation, curves: [{ name, keys, pre, post, interp, x_range, y_range }], last_issues }",
                Permission::Read,
            ),
            curves_describe,
        ),
        (
            schema(
                "curves.get",
                "curves_get",
                "One curve's keys, extrapolation and interpolation.",
                PATH_NAME,
                "{ keys: [{ x, y, interp, in_tangent, out_tangent }], pre, post, interp }",
                Permission::Read,
            ),
            curves_get,
        ),
        (
            schema(
                "curves.sample",
                "curves_sample",
                "Sample a curve at x, or at count evenly spaced points from..to (count <= 4096).",
                SAMPLE_PARAMS,
                "{ y } or { points: [[x, y], ...] }",
                Permission::Read,
            ),
            curves_sample,
        ),
        (
            schema(
                "curves.set_key",
                "curves_set_key",
                "Insert or replace the key at x (in memory; curves.export { write } saves).",
                SET_KEY_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            curves_set_key,
        ),
        (
            schema(
                "curves.remove_key",
                "curves_remove_key",
                "Remove the key at exactly x (a curve keeps at least one key).",
                REMOVE_KEY_PARAMS,
                "{ generation }",
                Permission::Assets,
            ),
            curves_remove_key,
        ),
        (
            schema(
                "curves.set_curve",
                "curves_set_curve",
                "Create or change a curve: keys, extrapolation, interpolation, rename.",
                SET_CURVE_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            curves_set_curve,
        ),
        (
            schema(
                "curves.delete",
                "curves_delete",
                "Delete a curve.",
                PATH_NAME,
                "{ generation }",
                Permission::Assets,
            ),
            curves_delete,
        ),
        (
            schema(
                "curves.validate",
                "curves_validate",
                "Validation issues of a table (the last load's when it did not load).",
                ONLY_PATH,
                "{ issues: [{ code, severity, message, curve, line }] }",
                Permission::Read,
            ),
            curves_validate,
        ),
        (
            schema(
                "curves.export",
                "curves_export",
                "The table as RON or CSV text; with write: true, saved to its source file.",
                EXPORT_PARAMS,
                "{ text, file? }",
                Permission::Assets,
            ),
            curves_export,
        ),
        (
            schema(
                "curves.import",
                "curves_import",
                "Replace a table's curves with CSV text (header name,<x>,...).",
                IMPORT_PARAMS,
                "{ curves, generation, issues }",
                Permission::Assets,
            ),
            curves_import,
        ),
    ]
}

/// Registers the `curves.*` methods on the world's [`AgentExtension`] (creating it when the app
/// has none yet). Call it once after adding `CurveTablePlugin`.
pub fn install_curve_table_methods(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    for (schema, handler) in methods() {
        extension.register(schema, handler);
    }
    world.insert_resource(extension);
}

fn unavailable() -> Value {
    json!({ "available": false })
}

fn str_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("bad_value", format!("`{name}` (a string) is required")))
}

fn num_param(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|f| Some(f as f32))
            .ok_or_else(|| invalid("bad_value", format!("`{name}` must be a number"))),
    }
}

fn table<'a>(tables: &'a CurveTables, path: &str) -> Result<&'a CurveTable, RpcError> {
    tables.get(path).ok_or_else(|| {
        missing(
            "curve_table_not_found",
            format!("no curve table at `{path}`; see curves.list"),
        )
    })
}

fn interp_json(interp: Option<TableInterp>) -> Value {
    interp.map_or(Value::Null, |i| json!(i.name()))
}

fn key_json(key: &CurveKey) -> Value {
    json!({
        "x": key.x, "y": key.y, "interp": interp_json(key.interp),
        "in_tangent": key.in_tangent, "out_tangent": key.out_tangent,
    })
}

fn parse_interp(value: &Value, name: &str) -> Result<Option<TableInterp>, RpcError> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => TableInterp::from_name(s)
            .map(Some)
            .ok_or_else(|| invalid("bad_value", format!("`{name}` must be step, linear or cubic"))),
        _ => Err(invalid("bad_value", format!("`{name}` must be a string"))),
    }
}

fn parse_extrap(value: &Value, name: &str) -> Result<Extrap, RpcError> {
    value.as_str().and_then(Extrap::from_name).ok_or_else(|| {
        invalid(
            "bad_value",
            format!("`{name}` must be hold, linear, cycle, cycle_offset or ping_pong"),
        )
    })
}

fn parse_key(value: &Value) -> Result<CurveKey, RpcError> {
    let number = |v: Option<&Value>, what: &str| -> Result<f32, RpcError> {
        v.and_then(Value::as_f64)
            .map(|f| f as f32)
            .ok_or_else(|| invalid("bad_value", format!("a key needs a number `{what}`")))
    };
    match value {
        Value::Array(pair) if pair.len() == 2 => Ok(CurveKey::new(
            number(pair.first(), "x")?,
            number(pair.get(1), "y")?,
        )),
        Value::Object(map) => {
            let opt = |name: &str| -> Result<Option<f32>, RpcError> {
                num_param(&Value::Object(map.clone()), name)
            };
            Ok(CurveKey {
                x: number(map.get("x"), "x")?,
                y: number(map.get("y"), "y")?,
                interp: parse_interp(map.get("interp").unwrap_or(&Value::Null), "interp")?,
                in_tangent: opt("in_tangent")?,
                out_tangent: opt("out_tangent")?,
            })
        }
        _ => Err(invalid(
            "bad_value",
            "a key is [x, y] or { x, y, interp?, in_tangent?, out_tangent? }",
        )),
    }
}

fn list_entry(path: &str, tables: &CurveTables) -> Value {
    let Some(slot) = tables.slot(path) else {
        return Value::Null;
    };
    let errors = slot.last_issues.iter().filter(|i| i.is_error()).count();
    json!({
        "path": path,
        "interp": slot.table.interp().name(),
        "curves": slot.table.len(),
        "generation": slot.generation,
        "errors": errors,
        "warnings": slot.last_issues.len() - errors,
        "file": slot.file.as_ref().map(|f| f.display().to_string()),
        "cooked": slot.cooked,
    })
}

fn curves_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(tables) = world.get_resource::<CurveTables>() else {
        return Ok(unavailable());
    };
    let list: Vec<Value> = tables.iter().map(|(path, _)| list_entry(path, tables)).collect();
    Ok(json!({ "available": true, "tables": list }))
}

fn curves_describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(tables) = world.get_resource::<CurveTables>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let t = table(tables, path)?;
    let curves: Vec<Value> = t
        .curves()
        .iter()
        .map(|c| {
            let range = t.handle(&c.name).map(|h| t.range(h)).unwrap_or_default();
            json!({
                "name": c.name, "keys": c.keys.len(), "pre": c.pre.name(), "post": c.post.name(),
                "interp": interp_json(c.interp), "x_range": [range.0, range.1], "y_range": [range.2, range.3],
            })
        })
        .collect();
    let mut entry = list_entry(path, tables);
    if let Value::Object(map) = &mut entry {
        map.insert("curves".into(), Value::Array(curves));
        let issues = tables
            .slot(path)
            .map(|s| issues_json(&s.last_issues))
            .unwrap_or(Value::Null);
        map.insert("last_issues".into(), issues);
    }
    Ok(entry)
}

fn curves_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(tables) = world.get_resource::<CurveTables>() else {
        return Ok(unavailable());
    };
    let t = table(tables, str_param(params, "path")?)?;
    let name = str_param(params, "name")?;
    let curve = t
        .curve(name)
        .ok_or_else(|| missing("curve_not_found", format!("no curve named `{name}`")))?;
    Ok(json!({
        "keys": curve.keys.iter().map(key_json).collect::<Vec<_>>(),
        "pre": curve.pre.name(), "post": curve.post.name(), "interp": interp_json(curve.interp),
    }))
}

fn curves_sample(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(tables) = world.get_resource::<CurveTables>() else {
        return Ok(unavailable());
    };
    let t = table(tables, str_param(params, "path")?)?;
    let name = str_param(params, "name")?;
    let handle = t
        .handle(name)
        .ok_or_else(|| missing("curve_not_found", format!("no curve named `{name}`")))?;
    if let Some(x) = num_param(params, "x")? {
        return Ok(json!({ "y": t.sample(handle, x) }));
    }
    let (Some(from), Some(to)) = (num_param(params, "from")?, num_param(params, "to")?) else {
        return Err(invalid("bad_value", "give `x`, or `from`, `to` and `count`"));
    };
    let count = crate::params::opt_u64(params, "count")?.unwrap_or(2);
    if count == 0 || count > MAX_SAMPLE_POINTS {
        return Err(invalid(
            "bad_value",
            format!("`count` must be 1..={MAX_SAMPLE_POINTS}"),
        ));
    }
    let points: Vec<Value> = (0..count)
        .map(|i| {
            let x = if count == 1 {
                from
            } else {
                from + (to - from) * (i as f32 / (count - 1) as f32)
            };
            json!([x, t.sample(handle, x)])
        })
        .collect();
    Ok(json!({ "points": points }))
}

fn edit(
    world: &mut World,
    path: &str,
    f: impl FnOnce(&mut CurveTable) -> Result<(), regia_framework::curve_table::CurveTableError>,
) -> Result<Value, RpcError> {
    let Some(mut tables) = world.get_resource_mut::<CurveTables>() else {
        return Ok(unavailable());
    };
    match tables.edit(path, f) {
        Ok(generation) => {
            let issues = tables
                .slot(path)
                .map(|s| issues_json(&s.last_issues))
                .unwrap_or(Value::Null);
            Ok(json!({ "generation": generation, "issues": issues }))
        }
        Err(error) => Err(refused(&error.issues)),
    }
}

/// `{ generation }` from an edit answer (`{ available: false }` passes through).
fn generation_only(answer: Value) -> Value {
    match answer.get("generation") {
        Some(generation) => json!({ "generation": generation }),
        None => answer,
    }
}

fn curves_set_key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let name = str_param(params, "name")?.to_owned();
    let key = parse_key(params)?;
    edit(world, &path, |t| t.set_key(&name, key))
}

fn curves_remove_key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let name = str_param(params, "name")?.to_owned();
    let x = num_param(params, "x")?.ok_or_else(|| invalid("bad_value", "`x` is required"))?;
    edit(world, &path, |t| t.remove_key(&name, x)).map(generation_only)
}

fn curves_set_curve(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let name = str_param(params, "name")?.to_owned();
    let keys = match params.get("keys") {
        None | Some(Value::Null) => None,
        Some(Value::Array(items)) => Some(items.iter().map(parse_key).collect::<Result<Vec<_>, _>>()?),
        Some(_) => return Err(invalid("bad_value", "`keys` must be an array")),
    };
    let pre = params.get("pre").map(|v| parse_extrap(v, "pre")).transpose()?;
    let post = params.get("post").map(|v| parse_extrap(v, "post")).transpose()?;
    let interp = params
        .get("interp")
        .map(|v| parse_interp(v, "interp"))
        .transpose()?;
    let rename = match params.get("rename") {
        None | Some(Value::Null) => None,
        Some(Value::String(to)) => Some(to.clone()),
        Some(_) => return Err(invalid("bad_value", "`rename` must be a string")),
    };
    // One `CurveTables::edit`: it works on a copy, so a refused rename also drops the key change.
    edit(world, &path, |t| {
        let mut curve = t
            .curve(&name)
            .cloned()
            .unwrap_or_else(|| NamedCurve::new(&name, Vec::new()));
        if let Some(keys) = keys {
            curve.keys = keys;
        }
        if let Some(pre) = pre {
            curve.pre = pre;
        }
        if let Some(post) = post {
            curve.post = post;
        }
        if let Some(interp) = interp {
            curve.interp = interp;
        }
        t.upsert_curve(curve)?;
        if let Some(rename) = rename {
            t.rename_curve(&name, &rename)?;
        }
        Ok(())
    })
}

fn curves_delete(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let name = str_param(params, "name")?.to_owned();
    edit(world, &path, |t| t.delete_curve(&name)).map(generation_only)
}

fn curves_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(tables) = world.get_resource::<CurveTables>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let slot = tables
        .slot(path)
        .ok_or_else(|| missing("curve_table_not_found", format!("no curve table at `{path}`")))?;
    let mut issues = slot.table.validate();
    if slot.generation == 0 || issues.is_empty() {
        issues.extend(slot.last_issues.iter().cloned());
    }
    Ok(json!({ "issues": issues_json(&issues) }))
}

/// Writes `text` to `file` through a temporary file and a rename.
pub(crate) fn write_atomic(file: &std::path::Path, text: &str) -> Result<(), RpcError> {
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, text.as_bytes())
        .and_then(|()| std::fs::rename(&tmp, file))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            error(
                protocol::INTERNAL_ERROR,
                "F0001",
                "io_failed",
                format!("{}: {e}", file.display()),
            )
        })
}

fn curves_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(tables) = world.get_resource::<CurveTables>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let slot = tables
        .slot(path)
        .ok_or_else(|| missing("curve_table_not_found", format!("no curve table at `{path}`")))?;
    let text = match crate::params::opt_str(params, "format")?.unwrap_or("ron") {
        "ron" => slot.table.to_ron_string(),
        "csv" => slot.table.to_csv_string().map_err(|e| refused(&e.issues))?,
        other => {
            return Err(invalid(
                "bad_value",
                format!("`format` must be ron or csv, not `{other}`"),
            ));
        }
    };
    let mut out = Map::new();
    out.insert("text".into(), json!(text));
    if crate::params::opt_bool(params, "write")?.unwrap_or(false) {
        let file = slot.file.clone().ok_or_else(|| {
            invalid(
                "read_only_asset",
                format!("`{path}` has no source file (packaged build)"),
            )
        })?;
        write_atomic(&file, &text)?;
        out.insert("file".into(), json!(file.display().to_string()));
    }
    Ok(Value::Object(out))
}

fn curves_import(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // Degrade before parsing: without the plugin every method answers `{ available: false }`,
    // whatever the CSV holds (spec 51 § 3).
    if !world.contains_resource::<CurveTables>() {
        return Ok(unavailable());
    }
    let path = str_param(params, "path")?.to_owned();
    let csv = str_param(params, "csv")?.to_owned();
    let interp =
        parse_interp(params.get("interp").unwrap_or(&Value::Null), "interp")?.unwrap_or(TableInterp::Linear);
    let imported = CurveTable::from_csv_str(&csv, interp).map_err(|e| refused(&e.issues))?;
    let curves = imported.len();
    let Some(mut tables) = world.get_resource_mut::<CurveTables>() else {
        return Ok(unavailable());
    };
    if tables.slot(&path).is_none() {
        return Err(missing(
            "curve_table_not_found",
            format!("no curve table at `{path}`"),
        ));
    }
    let generation = tables.insert(&path, imported);
    let issues = tables
        .slot(&path)
        .map(|s| issues_json(&s.last_issues))
        .unwrap_or(Value::Null);
    Ok(json!({ "curves": curves, "generation": generation, "issues": issues }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RON: &str = r#"(format_version: 1, interp: Linear, curves: {
        "falloff": (keys: [(0.0, 1.0), (10.0, 0.6), (30.0, 0.0)]),
    })"#;

    fn world() -> World {
        let mut world = World::new();
        let mut tables = CurveTables::default();
        tables.insert(
            "t/falloff.curves.ron",
            CurveTable::from_ron_str(RON).expect("table"),
        );
        world.insert_resource(tables);
        install_curve_table_methods(&mut world);
        world
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .method(method)
            .map(|(_, handler)| handler)
            .expect("registered");
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn every_method_is_registered_with_its_permission() {
        let world = world();
        let extension = world.resource::<AgentExtension>();
        for (name, permission) in [
            ("curves.list", "read"),
            ("curves.describe", "read"),
            ("curves.get", "read"),
            ("curves.sample", "read"),
            ("curves.validate", "read"),
            ("curves.set_key", "assets"),
            ("curves.remove_key", "assets"),
            ("curves.set_curve", "assets"),
            ("curves.delete", "assets"),
            ("curves.export", "assets"),
            ("curves.import", "assets"),
        ] {
            let (schema, _) = extension
                .method(name)
                .unwrap_or_else(|| panic!("{name} registered"));
            assert_eq!(schema.permission.name(), permission, "{name}");
            assert_eq!(schema.tool, name.replace('.', "_"));
        }
    }

    #[test]
    fn list_describe_get_and_sample() {
        let mut world = world();
        let list = call(&mut world, "curves.list", json!({})).expect("list");
        assert_eq!(list["tables"][0]["path"], "t/falloff.curves.ron");
        assert_eq!(list["tables"][0]["curves"], 1);
        let described = call(
            &mut world,
            "curves.describe",
            json!({ "path": "t/falloff.curves.ron" }),
        )
        .expect("describe");
        assert_eq!(described["curves"][0]["x_range"], json!([0.0, 30.0]));
        let got = call(
            &mut world,
            "curves.get",
            json!({ "path": "t/falloff.curves.ron", "name": "falloff" }),
        )
        .expect("get");
        assert_eq!(got["keys"].as_array().map(Vec::len), Some(3));
        let y = call(
            &mut world,
            "curves.sample",
            json!({ "path": "t/falloff.curves.ron", "name": "falloff", "x": 5.0 }),
        )
        .expect("sample");
        assert!((y["y"].as_f64().expect("y") - 0.8).abs() < 1e-6);
        let sweep = call(
            &mut world,
            "curves.sample",
            json!({ "path": "t/falloff.curves.ron", "name": "falloff", "from": 0.0, "to": 30.0, "count": 4 }),
        )
        .expect("sweep");
        assert_eq!(sweep["points"].as_array().map(Vec::len), Some(4));
        let err = call(
            &mut world,
            "curves.get",
            json!({ "path": "t/none.curves.ron", "name": "x" }),
        )
        .expect_err("none");
        assert_eq!(reason(&err), "curve_table_not_found");
        let err = call(
            &mut world,
            "curves.get",
            json!({ "path": "t/falloff.curves.ron", "name": "x" }),
        )
        .expect_err("none");
        assert_eq!(reason(&err), "curve_not_found");
    }

    #[test]
    fn edits_bump_the_generation_and_refuse_invalid_tables() {
        let mut world = world();
        let path = "t/falloff.curves.ron";
        let r = call(
            &mut world,
            "curves.set_key",
            json!({ "path": path, "name": "falloff", "x": 5.0, "y": 0.9 }),
        )
        .expect("set_key");
        assert_eq!(r["generation"], 2);
        let err = call(
            &mut world,
            "curves.remove_key",
            json!({ "path": path, "name": "falloff", "x": 7.0 }),
        )
        .expect_err("no key");
        assert_eq!(reason(&err), "key_not_found");
        let r = call(
            &mut world,
            "curves.set_curve",
            json!({ "path": path, "name": "ramp", "keys": [[0, 0], [1, 2]], "post": "linear", "interp": "cubic" }),
        )
        .expect("set_curve");
        assert_eq!(r["generation"], 3);
        let err = call(
            &mut world,
            "curves.set_curve",
            json!({ "path": path, "name": "dup", "keys": [[0, 0], [0, 1]] }),
        )
        .expect_err("duplicate x");
        assert_eq!(reason(&err), "curve_duplicate_x");
        let y = call(
            &mut world,
            "curves.sample",
            json!({ "path": path, "name": "ramp", "x": 2.0 }),
        )
        .expect("sample");
        assert!(y["y"].as_f64().expect("y") > 2.0, "linear post extrapolation");
        call(
            &mut world,
            "curves.delete",
            json!({ "path": path, "name": "ramp" }),
        )
        .expect("delete");
        let csv = call(
            &mut world,
            "curves.export",
            json!({ "path": path, "format": "csv" }),
        )
        .expect("export");
        assert!(csv["text"].as_str().expect("text").starts_with("name,"));
        let err = call(
            &mut world,
            "curves.export",
            json!({ "path": path, "write": true }),
        )
        .expect_err("no file");
        assert_eq!(reason(&err), "read_only_asset");
        let imported = call(
            &mut world,
            "curves.import",
            json!({ "path": path, "csv": "name,0,1\r\na,1,2\r\nb,3,\r\n" }),
        )
        .expect("import");
        assert_eq!(imported["curves"], 2);
        let validated = call(&mut world, "curves.validate", json!({ "path": path })).expect("validate");
        assert_eq!(validated["issues"], json!([]));
    }

    #[test]
    fn set_curve_with_a_bad_rename_keeps_the_keys_and_the_generation() {
        let mut world = world();
        let path = "t/falloff.curves.ron";
        let err = call(
            &mut world,
            "curves.set_curve",
            json!({ "path": path, "name": "falloff", "keys": [[0, 5]], "rename": "bad name" }),
        )
        .expect_err("bad rename");
        assert_eq!(reason(&err), "curve_bad_name");
        let tables = world.resource::<CurveTables>();
        assert_eq!(
            tables.generation(path),
            1,
            "a refused edit does not bump the generation"
        );
        let table = tables.get(path).expect("table");
        assert_eq!(
            table.sample_named("falloff", 10.0),
            Some(0.6),
            "the key change was rolled back with the rename"
        );
        let r = call(
            &mut world,
            "curves.set_curve",
            json!({ "path": path, "name": "falloff", "keys": [[0, 5]], "rename": "falloff_2" }),
        )
        .expect("good rename");
        assert_eq!(r["generation"], 2);
        let table = world.resource::<CurveTables>().get(path).expect("table");
        assert_eq!(table.sample_named("falloff_2", 10.0), Some(5.0));
        assert!(table.curve("falloff").is_none());
    }

    #[test]
    fn answers_without_the_plugin() {
        let mut world = World::new();
        install_curve_table_methods(&mut world);
        assert_eq!(
            call(&mut world, "curves.list", json!({})).expect("list"),
            json!({ "available": false })
        );
        assert_eq!(
            call(&mut world, "curves.get", json!({ "path": "x", "name": "y" })).expect("get"),
            json!({ "available": false })
        );
    }

    /// Spec 51 § 3 / test 18: every `curves.*` method degrades to `{ available: false }` without
    /// the plugin. `curves.import` used to parse the CSV first, so bad CSV answered an error.
    #[test]
    fn every_curves_method_answers_without_the_plugin() {
        let mut world = World::new();
        install_curve_table_methods(&mut world);
        let p = "t/none.curves.ron";
        let calls = [
            ("curves.list", json!({})),
            ("curves.describe", json!({ "path": p })),
            ("curves.get", json!({ "path": p, "name": "c" })),
            ("curves.sample", json!({ "path": p, "name": "c", "x": 1.0 })),
            (
                "curves.set_key",
                json!({ "path": p, "name": "c", "x": 1.0, "y": 2.0 }),
            ),
            ("curves.remove_key", json!({ "path": p, "name": "c", "x": 1.0 })),
            (
                "curves.set_curve",
                json!({ "path": p, "name": "c", "keys": [[0, 1]] }),
            ),
            ("curves.delete", json!({ "path": p, "name": "c" })),
            ("curves.validate", json!({ "path": p })),
            ("curves.export", json!({ "path": p, "format": "csv" })),
            (
                "curves.import",
                json!({ "path": p, "csv": "name,0,1\nc,1,2\n", "interp": "linear" }),
            ),
            (
                "curves.import",
                json!({ "path": p, "csv": "name,1,0\nc,nope,2\n" }),
            ),
        ];
        for (method, params) in calls {
            let answer = call(&mut world, method, params.clone())
                .unwrap_or_else(|e| panic!("{method} {params} answered an error: {e:?}"));
            assert_eq!(answer, json!({ "available": false }), "{method} {params}");
        }
    }
}
