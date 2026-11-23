// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of choosers (`docs/specs/51-curve-tables-and-choosers.md` § 5.2,
//! ADR-1649): the `chooser.*` methods over `regia_framework::chooser::Choosers`.
//!
//! Extension methods: a host that adds `ChooserPlugin` calls [`install_chooser_methods`] once to
//! register them on the world's [`AgentExtension`]. Without the plugin every method answers
//! `{ "available": false }`. `chooser.evaluate` is side-effect free (a fresh `Rng::new(seed)`,
//! no recent picks, no components touched); `chooser.evaluate_binding` advances the entity's own
//! random stream and records its recent pick. Documented on `book/agents/choosers.md`.
//!
//! Cells and values cross as JSON: numbers, booleans, strings; tags as arrays of names; ranges as
//! `[min, max]`; enum and asset cells as `{ "is": v }`, `{ "is_not": v }`, `{ "one_of": [..] }`,
//! `{ "kind": k }`; tag queries as `{ "query": "<RON>" }`; `"Any"`; and any cell as
//! `{ "ron": "<RON text>" }`. Results: `"nothing"`, `{ "asset": p }`, `{ "chooser": p }`,
//! `{ "value": v }`, `{ "ron": "<RON text>" }`.

use serde_json::{Map, Value, json};

use regia_app::prelude::*;
use regia_core::math::Rng;
use regia_framework::chooser::{
    CatchAll, Cell, ChooserBinding, ChooserContext, ChooserIssue, ChooserOutcome, ChooserProviders,
    ChooserTable, ChooserTrace, ChooserTraceLog, ChooserValue, Choosers, Column, ColumnKind, Mode, Recency,
    ResultEntry, RowOutcome, TraceDetail, ValueKind, Verdict, cell_ron, parse_cell, parse_column_kind,
    parse_result, result_ron,
};
use regia_framework::tags::TagContainer;

use crate::extension::{AgentExtension, AgentHandler};
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, ParamSchema, Permission};

const HINT: &str = "see book/agents/choosers.md";

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

/// The issues as `[{ code, severity, message, row, column, line }]` (`line` is always null: RON
/// spans are not kept per cell).
#[must_use]
pub fn issues_json(issues: &[ChooserIssue]) -> Value {
    Value::Array(
        issues
            .iter()
            .map(|i| {
                json!({ "code": i.code, "severity": i.severity.name(), "message": i.message, "row": i.row, "column": i.column, "line": null })
            })
            .collect(),
    )
}

fn refused(issues: &[ChooserIssue]) -> RpcError {
    let reason = issues
        .iter()
        .find(|i| i.is_error())
        .map_or("validation_failed", |i| i.code);
    let (code, regia) = if matches!(reason, "chooser_not_found" | "row_not_found" | "column_not_found") {
        (protocol::NOT_FOUND, "F0003")
    } else {
        (protocol::INVALID_PARAMS, "F0002")
    };
    RpcError {
        code,
        message: issues
            .first()
            .map_or_else(|| "refused".to_owned(), ToString::to_string),
        data: Some(json!({ "code": regia, "reason": reason, "hint": HINT, "issues": issues_json(issues) })),
    }
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
    "asset path of the chooser (`anim/hit.chooser.ron`)",
);
const ONLY_PATH: &[ParamSchema] = &[PATH];
const GET_PARAMS: &[ParamSchema] = &[PATH, param("row", "string", false, "one row id")];
const SET_CELL_PARAMS: &[ParamSchema] = &[
    PATH,
    param("row", "string", true, "row id"),
    param("column", "string", true, "column id"),
    param("value", "any", true, "the cell (JSON form, \"Any\", or { ron })"),
];
const SET_ROW_PARAMS: &[ParamSchema] = &[
    PATH,
    param("row", "string", true, "row id (created when absent)"),
    param("enabled", "boolean", false, "enable or disable"),
    param(
        "result",
        "any",
        false,
        "\"nothing\" | { asset } | { chooser } | { value } | { ron }",
    ),
    param(
        "cells",
        "object",
        false,
        "{ column: cell } (a null cell removes it)",
    ),
    param("note", "string", false, "author note"),
    param("before", "string", false, "move or insert before this row id"),
];
const ROW_PARAMS: &[ParamSchema] = &[PATH, param("row", "string", true, "row id")];
const SET_COLUMN_PARAMS: &[ParamSchema] = &[
    PATH,
    param(
        "column",
        "object",
        true,
        "{ id, input?, output?, kind (RON, e.g. \"FloatDistance(max_distance: 10.0)\"), wrap?, case_sensitive?, disabled? }",
    ),
    param("before", "string", false, "insert before this column id"),
];
const COLUMN_PARAMS: &[ParamSchema] = &[PATH, param("column", "string", true, "column id")];
const OPTIONS_PARAMS: &[ParamSchema] = &[
    PATH,
    param("mode", "string", false, "first | best | weighted | all"),
    param(
        "result",
        "string",
        false,
        "result type RON (`Asset(\"clip\")`, `Value(Float)`, `OutputOnly`)",
    ),
    param("catch_all", "any", false, "{ result, outputs? } or null"),
    param("near_best", "any", false, "number or null"),
    param("recency", "any", false, "{ window, penalty } or null"),
];
const EVALUATE_PARAMS: &[ParamSchema] = &[
    PATH,
    param("context", "object", false, "{ input: value }"),
    param("seed", "number", false, "seed of the fresh Rng (default 0)"),
    param("trace", "string", false, "none (default) | brief | full"),
    param("all", "boolean", false, "every candidate (evaluate_all)"),
];
const BINDING_PARAMS: &[ParamSchema] = &[
    param("entity", "number", true, "entity id with a ChooserBinding"),
    param("trace", "string", false, "none (default) | brief | full"),
];
const TRACE_PARAMS: &[ParamSchema] = &[
    param("site", "string", false, "binding site"),
    param("limit", "number", false, "newest traces to return (default 32)"),
];
const EXPORT_PARAMS: &[ParamSchema] = &[
    PATH,
    param("write", "boolean", false, "save to the source file (atomic)"),
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

fn methods() -> Vec<(MethodSchema, AgentHandler)> {
    vec![
        (
            schema(
                "chooser.list",
                "chooser_list",
                "Loaded choosers: mode, result type, row and column counts, generation, errors, warnings.",
                &[],
                "{ available, tables: [{ path, mode, result, rows, columns, generation, errors, warnings }] }",
                Permission::Read,
            ),
            chooser_list,
        ),
        (
            schema(
                "chooser.describe",
                "chooser_describe",
                "One chooser: inputs, outputs, columns, row ids and the last issues.",
                ONLY_PATH,
                "{ path, mode, result, ..., inputs, outputs, columns, row_ids, last_issues }",
                Permission::Read,
            ),
            chooser_describe,
        ),
        (
            schema(
                "chooser.get",
                "chooser_get",
                "A whole chooser table as JSON, or one row.",
                GET_PARAMS,
                "{ table } or { row }",
                Permission::Read,
            ),
            chooser_get,
        ),
        (
            schema(
                "chooser.set_cell",
                "chooser_set_cell",
                "Set one cell (in memory; chooser.export { write } saves).",
                SET_CELL_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            chooser_set_cell,
        ),
        (
            schema(
                "chooser.set_row",
                "chooser_set_row",
                "Create or change a row: enabled, result, cells, note, position.",
                SET_ROW_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            chooser_set_row,
        ),
        (
            schema(
                "chooser.remove_row",
                "chooser_remove_row",
                "Remove a row.",
                ROW_PARAMS,
                "{ generation }",
                Permission::Assets,
            ),
            chooser_remove_row,
        ),
        (
            schema(
                "chooser.set_column",
                "chooser_set_column",
                "Create or replace a column.",
                SET_COLUMN_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            chooser_set_column,
        ),
        (
            schema(
                "chooser.remove_column",
                "chooser_remove_column",
                "Remove a column and its cells.",
                COLUMN_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            chooser_remove_column,
        ),
        (
            schema(
                "chooser.set_options",
                "chooser_set_options",
                "Set mode, result type, catch-all, near_best or recency.",
                OPTIONS_PARAMS,
                "{ generation, issues }",
                Permission::Assets,
            ),
            chooser_set_options,
        ),
        (
            schema(
                "chooser.validate",
                "chooser_validate",
                "Validation issues, nested references resolved against the loaded choosers.",
                ONLY_PATH,
                "{ issues: [{ code, severity, message, row, column, line }] }",
                Permission::Read,
            ),
            chooser_validate,
        ),
        (
            schema(
                "chooser.evaluate",
                "chooser_evaluate",
                "Evaluate with a context (side-effect free: fresh Rng::new(seed), no recent picks), optionally traced.",
                EVALUATE_PARAMS,
                "{ result, row, used_catch_all, outputs, draw?, trace? } or { results }",
                Permission::Read,
            ),
            chooser_evaluate,
        ),
        (
            schema(
                "chooser.evaluate_binding",
                "chooser_evaluate_binding",
                "Evaluate an entity's binding with the live context its provider builds (advances its Rng, records recent picks).",
                BINDING_PARAMS,
                "{ result, row, used_catch_all, outputs, context, trace? }",
                Permission::Play,
            ),
            chooser_evaluate_binding,
        ),
        (
            schema(
                "chooser.trace",
                "chooser_trace",
                "Logged binding traces (ChooserTraceLog; enable it first).",
                TRACE_PARAMS,
                "{ enabled, traces: [{ site, frame, outcome, trace }] }",
                Permission::Read,
            ),
            chooser_trace,
        ),
        (
            schema(
                "chooser.export",
                "chooser_export",
                "The chooser as canonical RON text; with write: true, saved to its source file.",
                EXPORT_PARAMS,
                "{ text, file? }",
                Permission::Assets,
            ),
            chooser_export,
        ),
    ]
}

/// Registers the `chooser.*` methods on the world's [`AgentExtension`] (creating it when the app
/// has none yet). Call it once after adding `ChooserPlugin`.
pub fn install_chooser_methods(world: &mut World) {
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

fn lookup<'a>(choosers: &'a Choosers, path: &str) -> Result<&'a ChooserTable, RpcError> {
    choosers.get(path).ok_or_else(|| {
        missing(
            "chooser_not_found",
            format!("no chooser at `{path}`; see chooser.list"),
        )
    })
}

// ------------------------------------------------------------------ JSON forms

/// A value as JSON (tags as an array of names).
#[must_use]
pub fn value_json(value: &ChooserValue) -> Value {
    match value {
        ChooserValue::Bool(b) => json!(b),
        ChooserValue::Int(i) => json!(i),
        ChooserValue::Float(f) => json!(f),
        ChooserValue::Enum(s) | ChooserValue::Name(s) | ChooserValue::Asset(s) => json!(s),
        ChooserValue::Tags(tags) => json!(tags.names()),
    }
}

/// A JSON value of `kind`.
///
/// # Errors
///
/// A message when the JSON is not a value of `kind`.
pub fn value_from_json(kind: &ValueKind, value: &Value) -> Result<ChooserValue, String> {
    let wrong = || format!("expected a {} value, got {value}", kind.name());
    let parsed = match kind {
        ValueKind::Bool => ChooserValue::Bool(value.as_bool().ok_or_else(wrong)?),
        ValueKind::Int => ChooserValue::Int(value.as_i64().ok_or_else(wrong)?),
        ValueKind::Float => ChooserValue::Float(value.as_f64().ok_or_else(wrong)? as f32),
        ValueKind::Enum(_) => ChooserValue::Enum(value.as_str().ok_or_else(wrong)?.to_owned()),
        ValueKind::Name => ChooserValue::Name(value.as_str().ok_or_else(wrong)?.to_owned()),
        ValueKind::Asset => ChooserValue::Asset(value.as_str().ok_or_else(wrong)?.to_owned()),
        ValueKind::Tags => {
            let names: Vec<&str> = match value {
                Value::String(s) => vec![s.as_str()],
                Value::Array(items) => items
                    .iter()
                    .map(|v| v.as_str().ok_or_else(wrong))
                    .collect::<Result<_, _>>()?,
                _ => return Err(wrong()),
            };
            ChooserValue::Tags(TagContainer::from_names(names).map_err(|e| e.to_string())?)
        }
    };
    if !kind.admits(&parsed) {
        return Err(format!("{value} is not one of the declared variants"));
    }
    Ok(parsed)
}

/// A cell as JSON (see the module docs).
#[must_use]
pub fn cell_json(cell: &Cell) -> Value {
    match cell {
        Cell::Any => json!("Any"),
        Cell::Bool(b) => json!(b),
        Cell::IntRange(a, b) => json!([a, b]),
        Cell::FloatRange(a, b) => json!([a, b]),
        Cell::Is(v) => json!({ "is": v }),
        Cell::IsNot(v) => json!({ "is_not": v }),
        Cell::OneOf(v) => json!({ "one_of": v }),
        Cell::Kind(v) => json!({ "kind": v }),
        Cell::Name(v) => json!(v),
        Cell::Tags(_) => json!({ "query": cell_ron(cell) }),
        Cell::Target(v) | Cell::Weight(v) => json!(v),
        Cell::Value(v) => value_json(v),
    }
}

/// The RON text of a JSON cell for `column`.
fn cell_text(column: &Column, value: &Value) -> Result<String, String> {
    let quoted = |s: &str| format!("{s:?}");
    Ok(match value {
        Value::Null => "Any".to_owned(),
        Value::String(s) if s == "Any" => "Any".to_owned(),
        Value::String(s) if matches!(column.kind, ColumnKind::Tags) => s.clone(),
        Value::String(s) => quoted(s),
        Value::Bool(b) if matches!(column.kind, ColumnKind::Bool) => {
            if *b { "True" } else { "False" }.to_owned()
        }
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items)
            if matches!(column.kind, ColumnKind::IntRange | ColumnKind::FloatRange) && items.len() == 2 =>
        {
            format!("({}, {})", items[0], items[1])
        }
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(quoted)
                        .ok_or_else(|| format!("{v} is not a string"))
                })
                .collect::<Result<_, _>>()?;
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) if map.len() == 1 => {
            let (key, inner) = map.iter().next().ok_or("empty object")?;
            let text = |v: &Value| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("`{key}` needs a string"))
            };
            match key.as_str() {
                "ron" | "query" => text(inner)?,
                "is" => format!("Is({})", quoted(&text(inner)?)),
                "is_not" => format!("IsNot({})", quoted(&text(inner)?)),
                "kind" => format!("Kind({})", quoted(&text(inner)?)),
                "one_of" => {
                    let parts: Vec<String> = inner
                        .as_array()
                        .ok_or("`one_of` needs an array")?
                        .iter()
                        .map(|v| {
                            v.as_str()
                                .map(quoted)
                                .ok_or_else(|| format!("{v} is not a string"))
                        })
                        .collect::<Result<_, _>>()?;
                    format!("OneOf([{}])", parts.join(", "))
                }
                other => return Err(format!("unknown cell form `{other}`")),
            }
        }
        Value::Object(_) => return Err("a cell object has exactly one key".to_owned()),
    })
}

/// A JSON cell (the forms of `chooser.set_cell`, see the module docs) for `column` of `table`.
///
/// # Errors
///
/// `bad_value` when the JSON is not a cell of the column's kind.
pub fn cell_from_json(table: &ChooserTable, column: &Column, value: &Value) -> Result<Cell, RpcError> {
    let text = cell_text(column, value).map_err(|e| invalid("bad_value", e))?;
    parse_cell(column, &table.schema, &text).map_err(|e| invalid("bad_value", format!("`{text}`: {e}")))
}

/// A result entry as JSON (see the module docs).
#[must_use]
pub fn result_json(result: &ResultEntry) -> Value {
    match result {
        ResultEntry::Asset(p) => json!({ "asset": p }),
        ResultEntry::Value(v) => json!({ "value": value_json(v) }),
        ResultEntry::Chooser(p) => json!({ "chooser": p }),
        ResultEntry::Inline(_) => json!({ "ron": result_ron(result) }),
        ResultEntry::Nothing => json!("nothing"),
    }
}

fn result_from_json(table: &ChooserTable, value: &Value) -> Result<ResultEntry, RpcError> {
    let bad = |m: String| invalid("bad_value", m);
    match value {
        Value::String(s) if s == "nothing" => Ok(ResultEntry::Nothing),
        Value::Object(map) if map.len() == 1 => {
            let (key, inner) = map.iter().next().ok_or_else(|| bad("empty result".into()))?;
            let text = || {
                inner
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad(format!("`{key}` needs a string")))
            };
            match key.as_str() {
                "asset" => Ok(ResultEntry::Asset(text()?)),
                "chooser" => Ok(ResultEntry::Chooser(text()?)),
                "ron" => parse_result(&table.result_type, &text()?).map_err(|issues| refused(&issues)),
                "value" => match &table.result_type {
                    regia_framework::chooser::ResultType::Value(kind) => {
                        let kind = match kind {
                            regia_framework::chooser::ScalarKind::Bool => ValueKind::Bool,
                            regia_framework::chooser::ScalarKind::Int => ValueKind::Int,
                            regia_framework::chooser::ScalarKind::Float => ValueKind::Float,
                            regia_framework::chooser::ScalarKind::Name => ValueKind::Name,
                            regia_framework::chooser::ScalarKind::Tags => ValueKind::Tags,
                        };
                        value_from_json(&kind, inner).map(ResultEntry::Value).map_err(bad)
                    }
                    _ => Err(invalid(
                        "chooser_result_type_mismatch",
                        "this table does not yield values",
                    )),
                },
                other => Err(bad(format!("unknown result form `{other}`"))),
            }
        }
        _ => Err(bad(
            "a result is \"nothing\", { asset }, { chooser }, { value } or { ron }".into(),
        )),
    }
}

fn ron_of<T: serde::Serialize>(value: &T) -> Value {
    ron::to_string(value).map_or(Value::Null, Value::String)
}

fn column_json(column: &Column) -> Value {
    let kind = match &column.kind {
        ColumnKind::FloatDistance {
            influence,
            max_distance,
            wrap,
            drop_beyond,
        } => {
            let wrap = wrap.map_or_else(|| "None".to_owned(), |(lo, hi)| format!("Some(({lo:?}, {hi:?}))"));
            format!(
                "FloatDistance(influence: {influence:?}, max_distance: {max_distance:?}, wrap: {wrap}, drop_beyond: {drop_beyond})"
            )
        }
        other => other.ron_name().to_owned(),
    };
    json!({
        "id": column.id, "input": column.input, "output": column.output, "kind": kind,
        "wrap": column.wrap.map(|(lo, hi)| json!([lo, hi])), "case_sensitive": column.case_sensitive,
        "disabled": column.disabled,
    })
}

fn row_json(row: &regia_framework::chooser::Row) -> Value {
    let cells: Map<String, Value> = row.cells.iter().map(|(k, c)| (k.clone(), cell_json(c))).collect();
    json!({ "id": row.id, "enabled": row.enabled, "cells": cells, "result": result_json(&row.result), "note": row.note })
}

fn kinds_json(map: &std::collections::BTreeMap<String, ValueKind>) -> Value {
    Value::Object(map.iter().map(|(k, v)| (k.clone(), ron_of(v))).collect())
}

fn table_json(table: &ChooserTable) -> Value {
    json!({
        "mode": table.mode.name(),
        "result": ron_of(&table.result_type),
        "inputs": kinds_json(&table.schema.inputs),
        "outputs": kinds_json(&table.schema.outputs),
        "columns": table.columns.iter().map(column_json).collect::<Vec<_>>(),
        "rows": table.rows.iter().map(row_json).collect::<Vec<_>>(),
        "catch_all": table.catch_all.as_ref().map(|c| json!({
            "result": result_json(&c.result),
            "outputs": c.outputs.iter().map(|(k, v)| (k.clone(), value_json(v))).collect::<Map<_, _>>(),
        })),
        "near_best": table.near_best,
        "recency": table.recency.map(|r| json!({ "window": r.window, "penalty": r.penalty })),
    })
}

fn outputs_json(outputs: &[(String, ChooserValue)]) -> Value {
    Value::Object(outputs.iter().map(|(k, v)| (k.clone(), value_json(v))).collect())
}

fn outcome_json(outcome: Option<&ChooserOutcome>) -> Value {
    match outcome {
        Some(o) => json!({
            "result": result_json(&o.result), "row": o.row, "used_catch_all": o.used_catch_all,
            "outputs": outputs_json(&o.outputs),
        }),
        None => json!({ "result": null, "row": null, "used_catch_all": false, "outputs": {} }),
    }
}

fn outcome_name(outcome: &RowOutcome) -> Value {
    match outcome {
        RowOutcome::Disabled => json!({ "kind": "disabled" }),
        RowOutcome::RejectedBy(column) => json!({ "kind": "rejected_by", "column": column }),
        RowOutcome::Candidate { cost, weight } => {
            json!({ "kind": "candidate", "cost": cost, "weight": weight })
        }
        RowOutcome::NotEvaluated => json!({ "kind": "not_evaluated" }),
        RowOutcome::NestedEmpty => json!({ "kind": "nested_empty" }),
        RowOutcome::Selected => json!({ "kind": "selected" }),
    }
}

/// A trace as JSON.
#[must_use]
pub fn trace_json(trace: &ChooserTrace) -> Value {
    let rows: Vec<Value> = trace
        .rows
        .iter()
        .map(|r| {
            let cells: Vec<Value> = r
                .cells
                .iter()
                .map(|c| {
                    let (verdict, cost) = match c.verdict {
                        Verdict::Pass => ("pass", None),
                        Verdict::Fail => ("fail", None),
                        Verdict::Score(cost) => ("score", Some(cost)),
                        Verdict::Skipped => ("skipped", None),
                    };
                    json!({ "column": c.column, "verdict": verdict, "cost": cost, "detail": c.detail })
                })
                .collect();
            json!({ "id": r.id, "enabled": r.enabled, "cells": cells, "outcome": outcome_name(&r.outcome) })
        })
        .collect();
    json!({
        "table": trace.table, "mode": trace.mode.name(),
        "inputs": outputs_json(&trace.inputs), "rows": rows, "draw": trace.draw,
        "used_catch_all": trace.used_catch_all, "selected": trace.selected,
        "outputs": outputs_json(&trace.outputs),
        "nested": trace.nested.iter().map(trace_json).collect::<Vec<_>>(),
        "issues": trace.issues,
    })
}

fn trace_detail(params: &Value) -> Result<Option<TraceDetail>, RpcError> {
    match crate::params::opt_str(params, "trace")?.unwrap_or("none") {
        "none" => Ok(None),
        "brief" => Ok(Some(TraceDetail::Brief)),
        "full" => Ok(Some(TraceDetail::Full)),
        other => Err(invalid(
            "bad_value",
            format!("`trace` must be none, brief or full, not `{other}`"),
        )),
    }
}

// ------------------------------------------------------------------ handlers

fn list_entry(path: &str, choosers: &Choosers) -> Value {
    let Some(slot) = choosers.slot(path) else {
        return Value::Null;
    };
    let errors = slot.last_issues.iter().filter(|i| i.is_error()).count();
    let warnings = slot
        .last_issues
        .iter()
        .filter(|i| i.severity == regia_framework::chooser::IssueSeverity::Warning)
        .count();
    json!({
        "path": path, "mode": slot.table.mode.name(), "result": ron_of(&slot.table.result_type),
        "rows": slot.table.rows.len(), "columns": slot.table.columns.len(), "generation": slot.generation,
        "errors": errors, "warnings": warnings,
        "file": slot.file.as_ref().map(|f| f.display().to_string()),
        "cooked": slot.cooked,
    })
}

fn chooser_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let tables: Vec<Value> = choosers
        .iter()
        .map(|(path, _)| list_entry(path, choosers))
        .collect();
    Ok(json!({ "available": true, "tables": tables }))
}

fn chooser_describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let table = lookup(choosers, path)?;
    let mut entry = list_entry(path, choosers);
    if let Value::Object(map) = &mut entry {
        map.insert("inputs".into(), kinds_json(&table.schema.inputs));
        map.insert("outputs".into(), kinds_json(&table.schema.outputs));
        map.insert(
            "columns".into(),
            json!(table.columns.iter().map(column_json).collect::<Vec<_>>()),
        );
        map.insert(
            "row_ids".into(),
            json!(table.rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>()),
        );
        let issues = choosers
            .slot(path)
            .map(|s| issues_json(&s.last_issues))
            .unwrap_or(Value::Null);
        map.insert("last_issues".into(), issues);
    }
    Ok(entry)
}

fn chooser_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let table = lookup(choosers, str_param(params, "path")?)?;
    match crate::params::opt_str(params, "row")? {
        Some(id) => {
            let row = table
                .row(id)
                .ok_or_else(|| missing("row_not_found", format!("no row `{id}`")))?;
            Ok(json!({ "row": row_json(row) }))
        }
        None => Ok(json!({ "table": table_json(table) })),
    }
}

/// Runs a validated edit on a copy of the table at `path`.
fn edit(
    world: &mut World,
    path: &str,
    f: impl FnOnce(&mut ChooserTable) -> Result<(), RpcError>,
) -> Result<Value, RpcError> {
    let Some(mut choosers) = world.get_resource_mut::<Choosers>() else {
        return Ok(unavailable());
    };
    let mut failure: Option<RpcError> = None;
    let result = choosers.edit(path, |table| {
        f(table).map_err(|e| {
            failure = Some(e);
            ChooserIssue::error("bad_value", "edit refused")
        })
    });
    if let Some(e) = failure {
        return Err(e);
    }
    match result {
        Ok(generation) => {
            let issues = choosers
                .slot(path)
                .map(|s| issues_json(&s.last_issues))
                .unwrap_or(Value::Null);
            Ok(json!({ "generation": generation, "issues": issues }))
        }
        Err(issues) => Err(refused(&issues)),
    }
}

fn chooser_set_cell(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let row = str_param(params, "row")?.to_owned();
    let column = str_param(params, "column")?.to_owned();
    let Some(value) = params.get("value").cloned() else {
        return Err(invalid(
            "bad_value",
            "`value` is required (null is Any and clears the cell)",
        ));
    };
    edit(world, &path, |table| {
        let col = table
            .column(&column)
            .cloned()
            .ok_or_else(|| missing("column_not_found", format!("no column `{column}`")))?;
        let cell = cell_from_json(table, &col, &value)?;
        let target = table
            .rows
            .iter_mut()
            .find(|r| r.id == row)
            .ok_or_else(|| missing("row_not_found", format!("no row `{row}`")))?;
        if cell == Cell::Any {
            target.cells.remove(&column);
        } else {
            target.cells.insert(column.clone(), cell);
        }
        Ok(())
    })
}

/// `chooser.set_row`'s edit on `table`: `row` (create when absent), `enabled?`, `result?`,
/// `cells?`, `note?`, `before?` (position when creating or moving). The one implementation the
/// agent method and the editor's Tables panel (Chooser tab) share, so both write the same file
/// bytes for the same change.
///
/// # Errors
///
/// `bad_value`, `row_not_found` (a `before` that does not exist), `column_not_found`, or the
/// cell and result conversion errors.
pub fn set_row(table: &mut ChooserTable, params: &Value) -> Result<(), RpcError> {
    let id = str_param(params, "row")?.to_owned();
    let result = match params.get("result") {
        None | Some(Value::Null) => None,
        Some(v) => Some(result_from_json(table, v)?),
    };
    let mut cells = Vec::new();
    if let Some(Value::Object(map)) = params.get("cells") {
        for (column_id, value) in map {
            let column = table
                .column(column_id)
                .ok_or_else(|| missing("column_not_found", format!("no column `{column_id}`")))?;
            let cell = if value.is_null() {
                Cell::Any
            } else {
                cell_from_json(table, column, value)?
            };
            cells.push((column_id.clone(), cell));
        }
    }
    let at = table.rows.iter().position(|r| r.id == id);
    let mut row = match at {
        Some(i) => table.rows.remove(i),
        None => regia_framework::chooser::Row::new(&id, ResultEntry::Nothing),
    };
    if let Some(enabled) = crate::params::opt_bool(params, "enabled")? {
        row.enabled = enabled;
    }
    if let Some(result) = result {
        row.result = result;
    }
    if let Some(note) = crate::params::opt_str(params, "note")? {
        row.note = note.to_owned();
    }
    for (column, cell) in cells {
        if cell == Cell::Any {
            row.cells.remove(&column);
        } else {
            row.cells.insert(column, cell);
        }
    }
    let position = match crate::params::opt_str(params, "before")? {
        Some(before) => table
            .rows
            .iter()
            .position(|r| r.id == before)
            .ok_or_else(|| missing("row_not_found", format!("no row `{before}`")))?,
        None => at.unwrap_or(table.rows.len()).min(table.rows.len()),
    };
    table.rows.insert(position, row);
    Ok(())
}

fn chooser_set_row(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let params = params.clone();
    edit(world, &path, |table| set_row(table, &params))
}

fn generation_only(answer: Value) -> Value {
    match answer.get("generation") {
        Some(generation) => json!({ "generation": generation }),
        None => answer,
    }
}

/// `chooser.remove_row`'s edit on `table` (the shared edit of the agent method and the Tables
/// panel's row delete button).
///
/// # Errors
///
/// `row_not_found` when the row does not exist.
pub fn remove_row(table: &mut ChooserTable, row: &str) -> Result<(), RpcError> {
    let at = table
        .rows
        .iter()
        .position(|r| r.id == row)
        .ok_or_else(|| missing("row_not_found", format!("no row `{row}`")))?;
    table.rows.remove(at);
    Ok(())
}

fn chooser_remove_row(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let id = str_param(params, "row")?.to_owned();
    edit(world, &path, |table| remove_row(table, &id)).map(generation_only)
}

/// `chooser.set_column`'s edit on `table`: `column` (the definition object, replacing a column
/// with the same id or creating one) and `before?` (position when creating). The one
/// implementation the agent method and the Tables panel's column controls share.
///
/// # Errors
///
/// `bad_value` (a malformed definition) or `column_not_found` (a `before` that does not exist).
pub fn set_column(table: &mut ChooserTable, params: &Value) -> Result<(), RpcError> {
    let spec = crate::params::opt_object(params, "column")?
        .cloned()
        .ok_or_else(|| invalid("bad_value", "`column` must be an object"))?;
    let before = crate::params::opt_str(params, "before")?.map(str::to_owned);
    let id = spec
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("bad_value", "`column.id` is required"))?
        .to_owned();
    let kind_text = spec
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("bad_value", "`column.kind` (RON text) is required"))?;
    let kind = parse_column_kind(kind_text).map_err(|e| invalid("bad_value", format!("column kind: {e}")))?;
    let mut column = Column::new(&id, kind);
    column.input = crate::params::opt_str(&spec, "input")?.map(str::to_owned);
    column.output = crate::params::opt_str(&spec, "output")?.map(str::to_owned);
    column.wrap = match spec.get("wrap") {
        Some(Value::Array(pair)) if pair.len() == 2 => match (pair[0].as_f64(), pair[1].as_f64()) {
            (Some(lo), Some(hi)) => Some((lo, hi)),
            _ => return Err(invalid("bad_value", "`wrap` is [lo, hi]")),
        },
        None | Some(Value::Null) => None,
        Some(_) => return Err(invalid("bad_value", "`wrap` is [lo, hi]")),
    };
    column.case_sensitive = crate::params::opt_bool(&spec, "case_sensitive")?.unwrap_or(true);
    column.disabled = crate::params::opt_bool(&spec, "disabled")?.unwrap_or(false);
    match table.columns.iter().position(|c| c.id == id) {
        Some(at) => table.columns[at] = column,
        None => {
            let at = match &before {
                Some(before) => table
                    .columns
                    .iter()
                    .position(|c| &c.id == before)
                    .ok_or_else(|| missing("column_not_found", format!("no column `{before}`")))?,
                None => table.columns.len(),
            };
            table.columns.insert(at, column);
        }
    }
    Ok(())
}

fn chooser_set_column(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    edit(world, &path, |table| set_column(table, params))
}

/// `chooser.remove_column`'s edit on `table`: drops the column and every row's cell for it (the
/// shared edit of the agent method and the Tables panel's column delete button).
///
/// # Errors
///
/// `column_not_found` when the column does not exist.
pub fn remove_column(table: &mut ChooserTable, column: &str) -> Result<(), RpcError> {
    let at = table
        .columns
        .iter()
        .position(|c| c.id == column)
        .ok_or_else(|| missing("column_not_found", format!("no column `{column}`")))?;
    table.columns.remove(at);
    for row in &mut table.rows {
        row.cells.remove(column);
    }
    Ok(())
}

fn chooser_remove_column(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let id = str_param(params, "column")?.to_owned();
    edit(world, &path, |table| remove_column(table, &id)).map(generation_only)
}

/// `chooser.set_options`'s edit on `table`: `mode?`, `result?` (the result type as RON text),
/// `catch_all?` (`null` or `{ result, outputs? }`), `near_best?`, `recency?` (`null` or
/// `{ window?, penalty? }`). The one implementation the agent method and the Tables panel's
/// options bar share.
///
/// # Errors
///
/// `bad_value` for every malformed field, `unknown_input` for an undeclared catch-all output.
pub fn set_options(table: &mut ChooserTable, params: &Value) -> Result<(), RpcError> {
    if let Some(mode) = params.get("mode") {
        table.mode = mode
            .as_str()
            .and_then(Mode::from_name)
            .ok_or_else(|| invalid("bad_value", "`mode` must be first, best, weighted or all"))?;
    }
    if let Some(result) = params.get("result") {
        let text = result
            .as_str()
            .ok_or_else(|| invalid("bad_value", "`result` is RON text"))?;
        table.result_type =
            ron::from_str(text).map_err(|e| invalid("bad_value", format!("result type: {e}")))?;
    }
    match params.get("catch_all") {
        None => {}
        Some(Value::Null) => table.catch_all = None,
        Some(Value::Object(map)) => {
            let result = match map.get("result") {
                Some(v) => result_from_json(table, v)?,
                None => ResultEntry::Nothing,
            };
            let mut outputs = std::collections::BTreeMap::new();
            if let Some(Value::Object(values)) = map.get("outputs") {
                for (name, v) in values {
                    let kind = table.schema.outputs.get(name).ok_or_else(|| {
                        invalid("unknown_input", format!("output `{name}` is not declared"))
                    })?;
                    outputs.insert(
                        name.clone(),
                        value_from_json(kind, v).map_err(|e| invalid("bad_value", e))?,
                    );
                }
            }
            table.catch_all = Some(CatchAll { result, outputs });
        }
        Some(_) => {
            return Err(invalid(
                "bad_value",
                "`catch_all` is { result, outputs? } or null",
            ));
        }
    }
    match params.get("near_best") {
        None => {}
        Some(Value::Null) => table.near_best = None,
        Some(v) => {
            table.near_best = Some(
                v.as_f64()
                    .ok_or_else(|| invalid("bad_value", "`near_best` is a number"))? as f32,
            );
        }
    }
    match params.get("recency") {
        None => {}
        Some(Value::Null) => table.recency = None,
        // `{ window?, penalty? }`: a present field of the wrong type or range is refused, not
        // coerced into another policy (ADR-1809). The table check then rejects a non-finite or
        // out-of-range penalty (`chooser_bad_number`).
        Some(Value::Object(map)) => {
            let window = match map.get("window") {
                None => 1,
                Some(w) => w.as_u64().and_then(|w| u32::try_from(w).ok()).ok_or_else(|| {
                    invalid(
                        "bad_value",
                        "`recency.window` must be an integer in 0..=4294967295",
                    )
                })?,
            };
            let penalty = match map.get("penalty") {
                None => 0.0,
                Some(p) => p
                    .as_f64()
                    .ok_or_else(|| invalid("bad_value", "`recency.penalty` must be a number"))?
                    as f32,
            };
            table.recency = Some(Recency { window, penalty });
        }
        Some(_) => {
            return Err(invalid("bad_value", "`recency` is { window, penalty } or null"));
        }
    }
    Ok(())
}

fn chooser_set_options(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path")?.to_owned();
    let params = params.clone();
    edit(world, &path, |table| set_options(table, &params))
}

fn chooser_validate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let slot = choosers
        .slot(path)
        .ok_or_else(|| missing("chooser_not_found", format!("no chooser at `{path}`")))?;
    let issues = if slot.generation == 0 {
        slot.last_issues.clone()
    } else {
        slot.table.validate_at(Some(path), choosers)
    };
    Ok(json!({ "issues": issues_json(&issues) }))
}

fn context_from_json(table: &ChooserTable, params: &Value) -> Result<ChooserContext, RpcError> {
    let mut ctx = ChooserContext::default();
    match params.get("context") {
        None | Some(Value::Null) => {}
        Some(Value::Object(map)) => {
            for (name, value) in map {
                let kind = table.schema.inputs.get(name).ok_or_else(|| {
                    invalid("unknown_input", format!("the table declares no input `{name}`"))
                })?;
                ctx.set(
                    name,
                    value_from_json(kind, value)
                        .map_err(|e| invalid("bad_context", format!("`{name}`: {e}")))?,
                );
            }
        }
        Some(_) => return Err(invalid("bad_context", "`context` must be an object")),
    }
    Ok(ctx)
}

fn chooser_evaluate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let table = lookup(choosers, path)?;
    let mut ctx = context_from_json(table, params)?;
    let seed = crate::params::opt_u64(params, "seed")?.unwrap_or(0);
    let mut rng = Rng::new(seed);
    if crate::params::opt_bool(params, "all")?.unwrap_or(false) {
        let results: Vec<Value> = table
            .evaluate_all(choosers, &mut ctx)
            .iter()
            .map(|o| outcome_json(Some(o)))
            .collect();
        return Ok(json!({ "results": results }));
    }
    let mut out = match trace_detail(params)? {
        Some(detail) => {
            let (outcome, mut trace) = table.evaluate_traced(choosers, &mut ctx, &mut rng, None, detail);
            trace.table = path.to_owned();
            let mut out = outcome_json(outcome.as_ref());
            if let Value::Object(map) = &mut out {
                map.insert("draw".into(), json!(trace.draw));
                map.insert("trace".into(), trace_json(&trace));
            }
            out
        }
        None => outcome_json(table.evaluate(choosers, &mut ctx, &mut rng, None).as_ref()),
    };
    if let Value::Object(map) = &mut out {
        map.insert(
            "context_outputs".into(),
            Value::Object(
                ctx.outputs()
                    .map(|(k, v)| (k.to_owned(), value_json(v)))
                    .collect(),
            ),
        );
    }
    Ok(out)
}

fn chooser_evaluate_binding(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<Choosers>().is_none() {
        return Ok(unavailable());
    }
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| invalid("bad_value", "`entity` (an entity id) is required"))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| invalid("bad_value", format!("{bits} is not an entity id")))?;
    let detail = trace_detail(params)?;
    let binding = world.get::<ChooserBinding>(entity).cloned().ok_or_else(|| {
        missing(
            "binding_not_found",
            format!("entity {bits} has no ChooserBinding"),
        )
    })?;
    let provider = world
        .get_resource::<ChooserProviders>()
        .and_then(|p| p.get(&binding.provider))
        .ok_or_else(|| {
            missing(
                "provider_not_found",
                format!("no provider `{}` is registered", binding.provider),
            )
        })?;
    let mut ctx = ChooserContext::default();
    provider(world, entity, &mut ctx);
    let context = Value::Object(ctx.inputs().map(|(k, v)| (k.to_owned(), value_json(v))).collect());
    let mut rng = binding.rng.clone().unwrap_or_else(|| {
        // The same stream key the fixed-step evaluation uses: an agent-side evaluation of a
        // never-yet-evaluated guid-carrying binding must not pin it to the entity-bits stream
        // (Muse read-only review of fix-framework2-determinism, medium finding 4).
        Rng::with_stream(binding.seed, ChooserBinding::stream_id(world, entity))
    });
    let mut recent = binding.recent.clone();
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let table = lookup(choosers, &binding.table)?;
    let (outcome, trace) = match detail {
        Some(detail) => {
            let (outcome, mut trace) =
                table.evaluate_traced(choosers, &mut ctx, &mut rng, Some(&mut recent), detail);
            trace.table.clone_from(&binding.table);
            if table.mode == Mode::Weighted
                && let Some(row) = outcome.as_ref().and_then(|o| o.row.as_deref())
            {
                recent.record(row);
            }
            (outcome, Some(trace))
        }
        None => (
            table.evaluate(choosers, &mut ctx, &mut rng, Some(&mut recent)),
            None,
        ),
    };
    if let Some(mut live) = world.get_mut::<ChooserBinding>(entity) {
        live.rng = Some(rng);
        live.recent = recent;
    }
    let mut out = outcome_json(outcome.as_ref());
    if let Value::Object(map) = &mut out {
        map.insert("context".into(), context);
        if let Some(trace) = trace {
            map.insert("trace".into(), trace_json(&trace));
        }
    }
    Ok(out)
}

fn chooser_trace(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(log) = world.get_resource::<ChooserTraceLog>() else {
        return Ok(unavailable());
    };
    let limit = crate::params::opt_u64(params, "limit")?.unwrap_or(32) as usize;
    let site = crate::params::opt_str(params, "site")?;
    let traces: Vec<Value> = log
        .traces(site, limit)
        .into_iter()
        .map(|t| {
            json!({ "site": t.site, "frame": t.frame, "outcome": outcome_json(t.outcome.as_ref()), "trace": trace_json(&t.trace) })
        })
        .collect();
    Ok(json!({ "enabled": log.enabled, "traces": traces }))
}

fn chooser_export(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(choosers) = world.get_resource::<Choosers>() else {
        return Ok(unavailable());
    };
    let path = str_param(params, "path")?;
    let slot = choosers
        .slot(path)
        .ok_or_else(|| missing("chooser_not_found", format!("no chooser at `{path}`")))?;
    let text = slot.table.to_ron_string();
    let mut out = Map::new();
    out.insert("text".into(), json!(text));
    if crate::params::opt_bool(params, "write")?.unwrap_or(false) {
        let file = slot.file.clone().ok_or_else(|| {
            invalid(
                "read_only_asset",
                format!("`{path}` has no source file (packaged build)"),
            )
        })?;
        crate::curve_tables::write_atomic(&file, &text)?;
        out.insert("file".into(), json!(file.display().to_string()));
    }
    Ok(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::chooser::ChooserPlugin;

    const TABLE: &str = r#"(format_version: 1, mode: Best, result: Asset("montage"),
        inputs: { "hit_angle": Float, "armed": Bool }, outputs: { "rate": Float },
        columns: [
            (id: "armed", input: "armed", kind: Bool),
            (id: "angle", input: "hit_angle", kind: FloatDistance(max_distance: 180.0, wrap: Some((-180.0, 180.0)))),
            (id: "rate", output: "rate", kind: SetFloat),
        ],
        rows: [
            (id: "front", cells: { "armed": True, "angle": 0.0, "rate": 1.0 }, result: Asset("anim/front.montage.ron")),
            (id: "back", cells: { "angle": 180.0, "rate": 1.2 }, result: Asset("anim/back.montage.ron")),
        ],
        catch_all: Some((result: Asset("anim/generic.montage.ron"))))"#;

    const PATH: &str = "anim/hit.chooser.ron";

    fn world() -> World {
        let mut world = World::new();
        let mut choosers = Choosers::default();
        choosers.insert(PATH, ChooserTable::from_ron_str(TABLE).expect("table"));
        world.insert_resource(choosers);
        world.insert_resource(ChooserProviders::default());
        world.insert_resource(ChooserTraceLog::default());
        install_chooser_methods(&mut world);
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
            ("chooser.list", "read"),
            ("chooser.describe", "read"),
            ("chooser.get", "read"),
            ("chooser.set_cell", "assets"),
            ("chooser.set_row", "assets"),
            ("chooser.remove_row", "assets"),
            ("chooser.set_column", "assets"),
            ("chooser.remove_column", "assets"),
            ("chooser.set_options", "assets"),
            ("chooser.validate", "read"),
            ("chooser.evaluate", "read"),
            ("chooser.evaluate_binding", "play"),
            ("chooser.trace", "read"),
            ("chooser.export", "assets"),
        ] {
            let (schema, _) = extension
                .method(name)
                .unwrap_or_else(|| panic!("{name} registered"));
            assert_eq!(schema.permission.name(), permission, "{name}");
        }
    }

    #[test]
    fn evaluate_with_a_trace_then_edit_a_cell_and_see_the_change() {
        let mut world = world();
        let context = json!({ "armed": true, "hit_angle": 10.0 });
        let r = call(
            &mut world,
            "chooser.evaluate",
            json!({ "path": PATH, "context": context, "trace": "full" }),
        )
        .expect("evaluate");
        assert_eq!(r["row"], "front");
        assert_eq!(r["outputs"]["rate"], 1.0);
        assert_eq!(r["trace"]["selected"], "front");
        assert_eq!(r["trace"]["rows"].as_array().map(Vec::len), Some(2));
        assert!(r["draw"].is_null());
        // Unarm the front row's gate: the back row wins.
        let edited = call(
            &mut world,
            "chooser.set_cell",
            json!({ "path": PATH, "row": "front", "column": "armed", "value": false }),
        )
        .expect("set_cell");
        assert_eq!(edited["generation"], 2);
        let r = call(
            &mut world,
            "chooser.evaluate",
            json!({ "path": PATH, "context": context }),
        )
        .expect("again");
        assert_eq!(r["row"], "back");
        let all = call(
            &mut world,
            "chooser.evaluate",
            json!({ "path": PATH, "context": context, "all": true }),
        )
        .expect("all");
        assert_eq!(all["results"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn rows_columns_and_options_edit_round_trip() {
        let mut world = world();
        call(
            &mut world,
            "chooser.set_column",
            json!({ "path": PATH, "column": { "id": "w", "kind": "Weight" }, "before": "rate" }),
        )
        .expect("column");
        call(
            &mut world,
            "chooser.set_row",
            json!({ "path": PATH, "row": "side", "cells": { "angle": 90.0, "w": 2.0 }, "result": { "asset": "anim/side.montage.ron" }, "before": "back" }),
        )
        .expect("row");
        call(
            &mut world,
            "chooser.set_options",
            json!({ "path": PATH, "mode": "weighted", "near_best": 0.5, "recency": { "window": 2, "penalty": 0.25 } }),
        )
        .expect("options");
        let got = call(&mut world, "chooser.get", json!({ "path": PATH })).expect("get");
        let ids: Vec<&str> = got["table"]["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .filter_map(|r| r["id"].as_str())
            .collect();
        assert_eq!(ids, ["front", "side", "back"]);
        assert_eq!(got["table"]["mode"], "weighted");
        let row = call(&mut world, "chooser.get", json!({ "path": PATH, "row": "side" })).expect("row");
        assert_eq!(row["row"]["cells"]["w"], 2.0);
        call(
            &mut world,
            "chooser.remove_column",
            json!({ "path": PATH, "column": "w" }),
        )
        .expect("remove column");
        call(
            &mut world,
            "chooser.remove_row",
            json!({ "path": PATH, "row": "side" }),
        )
        .expect("remove row");
        let exported = call(&mut world, "chooser.export", json!({ "path": PATH })).expect("export");
        let text = exported["text"].as_str().expect("text");
        assert!(ChooserTable::from_ron_str(text).is_ok(), "{text}");
        let validated = call(&mut world, "chooser.validate", json!({ "path": PATH })).expect("validate");
        assert!(validated["issues"].as_array().is_some());
    }

    /// ADR-1809: a malformed recency is refused, not coerced into another policy.
    #[test]
    fn set_options_refuses_a_malformed_recency() {
        let mut world = world();
        for recency in [
            json!(true),
            json!({ "window": 4_294_967_312_u64, "penalty": 0.25 }),
            json!({ "window": 2.5 }),
            json!({ "window": 2, "penalty": "0.25" }),
        ] {
            let err = call(
                &mut world,
                "chooser.set_options",
                json!({ "path": PATH, "recency": recency }),
            )
            .expect_err("refused");
            assert_eq!(reason(&err), "bad_value", "{recency}");
        }
        call(
            &mut world,
            "chooser.set_options",
            json!({ "path": PATH, "recency": { "window": 3, "penalty": 0.5 } }),
        )
        .expect("valid recency");
        let got = call(&mut world, "chooser.get", json!({ "path": PATH })).expect("get");
        assert_eq!(got["table"]["recency"], json!({ "window": 3, "penalty": 0.5 }));
    }

    #[test]
    fn errors_carry_reasons() {
        let mut world = world();
        let cases = [
            (
                "chooser.get",
                json!({ "path": "nope.chooser.ron" }),
                "chooser_not_found",
            ),
            (
                "chooser.get",
                json!({ "path": PATH, "row": "nope" }),
                "row_not_found",
            ),
            (
                "chooser.set_cell",
                json!({ "path": PATH, "row": "front", "column": "armed" }),
                "bad_value",
            ),
            (
                "chooser.set_cell",
                json!({ "path": PATH, "row": "front", "column": "nope", "value": 1 }),
                "column_not_found",
            ),
            (
                "chooser.set_cell",
                json!({ "path": PATH, "row": "front", "column": "armed", "value": 3.5 }),
                "bad_value",
            ),
            (
                "chooser.evaluate",
                json!({ "path": PATH, "context": { "speed": 1.0 } }),
                "unknown_input",
            ),
            (
                "chooser.evaluate",
                json!({ "path": PATH, "context": { "armed": 1.0 } }),
                "bad_context",
            ),
            (
                "chooser.set_cell",
                json!({ "path": PATH, "row": "front", "column": "angle", "value": { "ron": "NaN" } }),
                "chooser_bad_number",
            ),
            (
                "chooser.export",
                json!({ "path": PATH, "write": true }),
                "read_only_asset",
            ),
            (
                "chooser.evaluate_binding",
                json!({ "entity": 1 }),
                "binding_not_found",
            ),
        ];
        for (method, params, expected) in cases {
            let err = call(&mut world, method, params).expect_err(method);
            assert_eq!(reason(&err), expected, "{method}");
        }
    }

    #[test]
    fn evaluate_binding_uses_the_provider_and_advances_the_binding() {
        fn provider(_: &World, _: Entity, ctx: &mut ChooserContext) {
            ctx.set("armed", ChooserValue::Bool(true));
            ctx.set("hit_angle", ChooserValue::Float(5.0));
        }
        let mut world = world();
        world
            .resource_mut::<ChooserProviders>()
            .register("hits", provider);
        let entity = world.spawn(ChooserBinding::new(PATH, "hits", "hits")).id();
        let r = call(
            &mut world,
            "chooser.evaluate_binding",
            json!({ "entity": entity.to_bits(), "trace": "brief" }),
        )
        .expect("binding");
        assert_eq!(r["row"], "front");
        assert_eq!(r["context"]["armed"], true);
        assert!(
            world
                .get::<ChooserBinding>(entity)
                .and_then(|b| b.rng.as_ref())
                .is_some()
        );
        let traces = call(&mut world, "chooser.trace", json!({})).expect("trace");
        assert_eq!(traces["enabled"], false);
    }

    /// Spec 51 § 7 (ADR-4756): `chooser.evaluate_binding` starts a fresh binding on the same random
    /// stream as the fixed-step pass, `ChooserBinding::stream_id` (the entity's guid mixed to 64
    /// bits), not its entity bits. It used `entity.to_bits()` and wrote that stream back into the
    /// live binding.
    #[test]
    fn evaluate_binding_starts_the_guid_stream_like_the_fixed_step_pass() {
        use regia_scene::components::EntityGuid;
        fn provider(_: &World, _: Entity, ctx: &mut ChooserContext) {
            ctx.set("armed", ChooserValue::Bool(true));
            ctx.set("hit_angle", ChooserValue::Float(5.0));
        }
        let mut world = world();
        world
            .resource_mut::<ChooserProviders>()
            .register("hits", provider);
        world.spawn_empty();
        let guid = EntityGuid(regia_core::id::Guid::from_u128(
            0x0123_4567_89ab_cdef_fedc_ba98_7654_3210,
        ));
        let entity = world
            .spawn((guid, ChooserBinding::new(PATH, "hits", "hits").with_seed(9)))
            .id();
        // Each guid half through the splitmix64 finalizer, then XOR (integ's fold, which replaced
        // the bare `hi ^ lo` of ADR-4756).
        fn splitmix64(mut z: u64) -> u64 {
            z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }
        let folded = splitmix64(0xfedc_ba98_7654_3210_u64) ^ splitmix64(0x0123_4567_89ab_cdef_u64);
        assert_eq!(ChooserBinding::stream_id(&world, entity), folded);
        assert_ne!(folded, entity.to_bits());
        let r = call(
            &mut world,
            "chooser.evaluate_binding",
            json!({ "entity": entity.to_bits() }),
        )
        .expect("binding");
        assert_eq!(r["row"], "front");
        // `Best` takes no draw, so the stored stream is the fresh one.
        let stored = world
            .get::<ChooserBinding>(entity)
            .and_then(|b| b.rng.clone())
            .expect("the call stores the stream");
        assert_eq!(stored, Rng::with_stream(9, folded));
        assert_ne!(stored, Rng::with_stream(9, entity.to_bits()));
    }

    #[test]
    fn answers_without_the_plugin() {
        let mut world = World::new();
        install_chooser_methods(&mut world);
        for method in ["chooser.list", "chooser.get", "chooser.evaluate", "chooser.trace"] {
            assert_eq!(
                call(&mut world, method, json!({ "path": "x" })).expect(method),
                json!({ "available": false }),
                "{method}"
            );
        }
        let _ = ChooserPlugin;
    }

    /// Spec 51 § 3 / test 18: every `chooser.*` method degrades to `{ available: false }` without
    /// the plugin (given well-formed parameters).
    #[test]
    fn every_chooser_method_answers_without_the_plugin() {
        let mut world = World::new();
        install_chooser_methods(&mut world);
        let p = "x/none.chooser.ron";
        let calls = [
            ("chooser.list", json!({})),
            ("chooser.describe", json!({ "path": p })),
            ("chooser.get", json!({ "path": p, "row": "r" })),
            (
                "chooser.set_cell",
                json!({ "path": p, "row": "r", "column": "c", "value": true }),
            ),
            (
                "chooser.set_row",
                json!({ "path": p, "row": "r", "enabled": false }),
            ),
            ("chooser.remove_row", json!({ "path": p, "row": "r" })),
            (
                "chooser.set_column",
                json!({ "path": p, "column": { "id": "w", "kind": "Weight" } }),
            ),
            ("chooser.remove_column", json!({ "path": p, "column": "w" })),
            ("chooser.set_options", json!({ "path": p, "mode": "first" })),
            ("chooser.validate", json!({ "path": p })),
            ("chooser.evaluate", json!({ "path": p, "context": { "a": 1.0 } })),
            ("chooser.evaluate_binding", json!({ "entity": 1 })),
            ("chooser.trace", json!({ "limit": 4 })),
            ("chooser.export", json!({ "path": p })),
        ];
        for (method, params) in calls {
            let answer = call(&mut world, method, params.clone())
                .unwrap_or_else(|e| panic!("{method} {params} answered an error: {e:?}"));
            assert_eq!(answer, json!({ "available": false }), "{method} {params}");
        }
    }
}
