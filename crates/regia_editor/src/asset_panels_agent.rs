// SPDX-License-Identifier: MPL-2.0
//! The agent surface of the asset audit panels (`crate::asset_panels`,
//! `book/agents/asset-validators.md`): what a human sees and does in Problems, Reference graph and
//! Size map. The queries themselves are `editor.asset_validate`, `editor.asset_graph` and
//! `editor.asset_size_map`; these methods read and drive the panels' one model.
//!
//! The asset set comes from `set` (RON) or, by default here, from the editor's loaded registry.

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::asset_audit::{AuditError, GraphQuery, GroupBy, Side, SizeMapQuery};
use regia_asset_pipeline::asset_graph::{AssetKind, AssetSet};
use regia_asset_pipeline::validation_rules::ValidationFile;
use regia_asset_pipeline::validators::{Scope, Severity};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::asset_panels::{AssetPanels, PanelSource, SEVERITIES, registry_set, severity_name};
use crate::viewport_agent::Handler;

const P_SET: ParamSchema = ParamSchema {
    name: "set",
    kind: "string",
    required: false,
    description: "An asset set as RON. Default: the editor's loaded registry (registry_not_loaded without one).",
};

fn audit_error(e: &AuditError) -> RpcError {
    let code = if matches!(e, AuditError::UnknownAsset(_)) {
        NOT_FOUND
    } else {
        INVALID_PARAMS
    };
    RpcError::new(
        code,
        e.code(),
        e.to_string(),
        "take paths from the set; depth is 0..=8, width 1..=500",
    )
}

/// The request's asset set and where it came from.
fn set_of(world: &World, params: &Value) -> Result<(AssetSet, PanelSource), RpcError> {
    if let Some(text) = params.get("set").and_then(Value::as_str) {
        let set = AssetSet::from_ron(text).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                e.code(),
                e.to_string(),
                "`set` is the RON of an asset set",
            )
        })?;
        return Ok((set, PanelSource::Request));
    }
    registry_set(world)
        .map(|set| (set, PanelSource::Registry))
        .ok_or_else(|| {
            RpcError::new(
                NOT_FOUND,
                "registry_not_loaded",
                "no asset registry is loaded in this editor",
                "cook the project first, or pass `set`",
            )
        })
}

fn panels_mut(world: &mut World) -> Mut<'_, AssetPanels> {
    world.get_resource_or_insert_with(AssetPanels::default)
}

fn severity_param(text: &str) -> Result<Severity, RpcError> {
    SEVERITIES
        .into_iter()
        .find(|s| severity_name(*s) == text)
        .ok_or_else(|| RpcError::invalid_params(format!("`{text}` is not error, warning or info")))
}

fn str_list(value: &Value, key: &str) -> Result<Vec<String>, RpcError> {
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be an array of strings")))
}

/// The JSON of the Problems panel: `limit` visible rows.
fn problems_json(panels: &AssetPanels, limit: usize) -> Value {
    let problems = &panels.problems;
    let visible = problems.visible();
    // Identical consecutive rows are one row with a `count` while the panel collapses them
    // (ADR-7789).
    let rows: Vec<Value> = problems
        .visible_runs()
        .iter()
        .take(limit)
        .map(|(row, index, count)| {
            json!({
                "severity": severity_name(row.severity),
                "code": row.code,
                "validator": row.validator,
                "asset": row.asset,
                "text": row.text,
                "hint": row.hint,
                "index": index,
                "count": count,
                "entities": crate::asset_panels::problem_entities(row),
            })
        })
        .collect();
    let groups: Vec<Value> = problems
        .items()
        .iter()
        .filter_map(|item| match item {
            crate::asset_panels::ProblemItem::Header {
                source,
                rows,
                worst,
                open,
            } => Some(json!({
                "source": source,
                "rows": rows,
                "worst": severity_name(*worst),
                "open": open,
            })),
            crate::asset_panels::ProblemItem::Row(..) => None,
        })
        .collect();
    let hidden: Vec<&str> = problems
        .hidden_severities
        .iter()
        .map(|s| severity_name(*s))
        .collect();
    json!({
        "ran": problems.report.is_some(),
        "summary": problems.report.as_ref().map(|r| &r.summary),
        "rows": rows,
        "rows_total": problems.rows.len(),
        "visible": visible.len(),
        "hidden_severities": hidden,
        "validator": problems.validator,
        "validators": problems.validators(),
        "grouped": problems.grouped,
        "collapse_duplicates": problems.collapse_duplicates,
        "groups": groups,
        "selected_row": problems.selected_row,
    })
}

fn graph_json(panels: &AssetPanels) -> Value {
    let graph = &panels.graph;
    json!({
        "query": graph.query,
        "result": graph.result,
        "can_back": graph.can_back(),
        "can_forward": graph.can_forward(),
    })
}

fn size_map_json(panels: &AssetPanels) -> Value {
    let state = &panels.size_map;
    let cells: Vec<Value> = state
        .cells
        .iter()
        .map(|c| json!({ "row": c.index, "x": c.x, "y": c.y, "w": c.w, "h": c.h }))
        .collect();
    json!({ "query": state.query, "result": state.result, "cells": cells })
}

/// `editor.asset_panels_state`.
fn panels_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = params.get("limit").and_then(Value::as_u64).unwrap_or(500) as usize;
    let panels = world.get_resource::<AssetPanels>().cloned().unwrap_or_default();
    Ok(json!({
        "source": panels.source.as_str(),
        "error": panels.error,
        "problems": problems_json(&panels, limit),
        "graph": graph_json(&panels),
        "size_map": size_map_json(&panels),
    }))
}

/// `editor.problems_panel`: run, filter or pick.
///
/// `hide` and `validator` are parsed before any write. A refusal leaves the panel as it was.
fn problems_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // A material graph error's link (w13) is checked before any write, like the other params.
    let open_graph_error = match params.get("open_graph_error") {
        None | Some(Value::Null) => None,
        Some(raw) => {
            let index = raw
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`open_graph_error` is an error's `index`"))?;
            let listed = crate::matgraph_editor::problems::world_problems(world).len();
            if index >= listed {
                return Err(graph_error_missing(format!(
                    "there is no material graph error {index} ({listed} listed)"
                )));
            }
            Some(index)
        }
    };
    let hide = match params.get("hide") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            str_list(value, "hide")?
                .iter()
                .map(|name| severity_param(name))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };
    let validator = match params.get("validator") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => {
            let name = value
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`validator` is a name or null"))?;
            Some(Some(name.to_owned()))
        }
    };
    if params.get("run").and_then(Value::as_bool).unwrap_or(false) {
        let (set, source) = set_of(world, params)?;
        let rules = crate::asset_panels::project_rules(world)
            .map_err(|e| RpcError::new(INVALID_PARAMS, "invalid_config", e, "fix config/validation.ron"))?;
        let scope = match (params.get("assets"), folder_param(params)?) {
            (Some(value), _) if !value.is_null() => Some(Scope::Assets(str_list(value, "assets")?)),
            // A Content Browser folder (ADR-7725): what is under it, recursively.
            (_, Some(folder)) => Some(Scope::Folder {
                path: crate::asset_panels::folder_in_set(&set, &folder),
                recursive: true,
            }),
            _ => None,
        };
        run_validation(world, &set, source, &rules, scope);
    }
    let mut panels = panels_mut(world);
    let select_row = params
        .get("select_row")
        .and_then(Value::as_u64)
        .map(|n| n as usize);
    if let Some(index) = select_row
        && index >= panels.problems.rows.len()
    {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "problem_row_missing",
            format!("there is no row {index}"),
            "take `index` from a row of this answer",
        ));
    }
    if let Some(hide) = hide {
        panels.problems.hidden_severities = hide;
    }
    if let Some(validator) = validator {
        panels.problems.validator = validator;
    }
    if let Some(asset) = params.get("select").and_then(Value::as_str) {
        panels.select(asset);
    }
    if let Some(grouped) = params.get("group").and_then(Value::as_bool) {
        panels.problems.grouped = grouped;
    }
    if let Some(collapse) = params.get("collapse").and_then(Value::as_bool) {
        panels.problems.collapse_duplicates = collapse;
    }
    if let Some(source) = params.get("toggle_group").and_then(Value::as_str) {
        panels.problems.toggle_group(source);
    }
    if let Some(index) = select_row
        && let Some(asset) = panels.problems.rows.get(index).map(|row| row.asset.clone())
    {
        panels.problems.selected_row = Some(index);
        panels.select(asset);
    }
    let mut answer = problems_json(&panels, 500);
    if params.get("copy").and_then(Value::as_str) == Some("all") {
        answer["copied"] = json!(panels.problems.copy_all_text());
    } else if params.get("copy").and_then(Value::as_str) == Some("selected") {
        answer["copied"] = json!(panels.problems.selected_text());
    }
    // A finding's entity link: select and frame it, as the panel's link does.
    if let Some(guid) = params.get("select_entity").and_then(Value::as_str) {
        let entity = crate::log_links::select_entity_by_guid(world, guid)?;
        answer["selected_entity"] = json!(entity.to_bits());
    }
    // The open material graphs' compile errors (w13, P-185) and their Open link.
    if let Some(index) = open_graph_error {
        let opened = crate::matgraph_editor::problems::open_world_problem(world, index)
            .map_err(graph_error_missing)?;
        answer["opened_graph_error"] = opened.to_json(index);
    }
    let graph_errors: Vec<Value> = crate::matgraph_editor::problems::world_problems(world)
        .iter()
        .enumerate()
        .map(|(i, p)| p.to_json(i))
        .collect();
    answer["graph_errors"] = Value::Array(graph_errors);
    // The open logic graph's issues with their nodes (ADR-8021; `editor.logic_goto_node` follows one).
    answer["logic_graph"] = crate::logic_graph_editor::problems::rows_json(world);
    Ok(answer)
}

fn graph_error_missing(message: String) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "graph_error_missing",
        message,
        "take `index` from graph_errors of this answer",
    )
}

fn run_validation(
    world: &mut World,
    set: &AssetSet,
    source: PanelSource,
    rules: &ValidationFile,
    scope: Option<Scope>,
) {
    let mut panels = panels_mut(world);
    let scope = scope
        .or_else(|| panels.problems.last_scope.clone())
        .unwrap_or(Scope::Project);
    panels.source = source;
    panels.error = None;
    panels.validate(set, rules, scope);
}

fn side_param(params: &Value) -> Result<Side, RpcError> {
    match params.get("side").and_then(Value::as_str) {
        Some("referrers") => Ok(Side::Referrers),
        Some("dependencies") => Ok(Side::Dependencies),
        _ => Err(RpcError::invalid_params("`side` is referrers or dependencies")),
    }
}

fn u32_param(params: &Value, key: &str) -> Result<u32, RpcError> {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a whole number")))
}

/// One graph action, parsed before the panel is borrowed.
enum GraphStep {
    Open(String),
    Back,
    Forward,
    Depth {
        side: Side,
        depth: u32,
    },
    Width(u32),
    Kind(AssetKind),
    More {
        side: Side,
        hop: u32,
    },
    OpenSizeMap,
    /// Open on every asset under a folder (ADR-7725).
    OpenFolder(String),
}

/// The `folder` param (a Content Browser folder, `assets/props`), when given (ADR-7725).
fn folder_param(params: &Value) -> Result<Option<String>, RpcError> {
    match params.get("folder") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(folder)) if !folder.trim_matches('/').is_empty() => {
            Ok(Some(folder.trim_end_matches('/').to_owned()))
        }
        Some(_) => Err(RpcError::invalid_params(
            "`folder` is a non-empty folder path (assets/…)",
        )),
    }
}

/// The assets under `folder`; `asset_folder_empty` when there are none.
fn folder_roots(set: &AssetSet, folder: &str) -> Result<Vec<String>, RpcError> {
    let roots = crate::asset_panels::folder_assets(set, folder);
    if roots.is_empty() {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "asset_folder_empty",
            format!("no asset of the set is under {folder}"),
            "pass a folder that holds cooked assets (see editor.list_assets)",
        ));
    }
    Ok(roots)
}

/// `editor.reference_graph_panel`: one panel action.
///
/// The action and its parameters are checked before any write. `source` is assigned only after
/// the action succeeds, so a refusal leaves the panel as it was.
fn reference_graph_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let action = params
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`action` is required"))?;
    let step = match action {
        "open" if params.get("asset").is_none() && params.get("folder").is_some() => {
            GraphStep::OpenFolder(folder_param(params)?.unwrap_or_default())
        }
        "open" | "recenter" => {
            let asset = params
                .get("asset")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("`asset` is required"))?;
            GraphStep::Open(asset.to_owned())
        }
        "back" => GraphStep::Back,
        "forward" => GraphStep::Forward,
        "depth" => GraphStep::Depth {
            side: side_param(params)?,
            depth: u32_param(params, "depth")?,
        },
        "width" => GraphStep::Width(u32_param(params, "width")?),
        "kind" => {
            let name = params
                .get("kind")
                .ok_or_else(|| RpcError::invalid_params("`kind` is required"))?;
            let kind: AssetKind = serde_json::from_value(name.clone()).map_err(|_| {
                RpcError::invalid_params("`kind` is mesh, texture, material, scene, audio or other")
            })?;
            GraphStep::Kind(kind)
        }
        "more" => GraphStep::More {
            side: side_param(params)?,
            hop: u32_param(params, "hop")?,
        },
        "open_size_map" => GraphStep::OpenSizeMap,
        other => {
            return Err(RpcError::invalid_params(format!(
                "`{other}` is not open, recenter, back, forward, depth, width, kind, more or open_size_map"
            )));
        }
    };
    let (set, source) = set_of(world, params)?;
    let mut panels = panels_mut(world);
    let moved = match step {
        GraphStep::OpenFolder(folder) => {
            let roots = folder_roots(&set, &folder)?;
            let query = GraphQuery {
                roots,
                folder_expanded: true,
                pages: Vec::new(),
                ..panels.graph.query.clone()
            };
            panels.graph.show(&set, query).map(|()| true)
        }
        GraphStep::Open(asset) => {
            let query = GraphQuery {
                roots: vec![asset],
                pages: Vec::new(),
                ..panels.graph.query.clone()
            };
            panels.graph.show(&set, query).map(|()| true)
        }
        GraphStep::Back => panels.graph.step(&set, -1),
        GraphStep::Forward => panels.graph.step(&set, 1),
        GraphStep::Depth { side, depth } => {
            let mut query = panels.graph.query.clone();
            match side {
                Side::Referrers => query.referrer_depth = depth,
                Side::Dependencies => query.dependency_depth = depth,
            }
            panels.graph.show(&set, query).map(|()| true)
        }
        GraphStep::Width(width) => {
            let mut query = panels.graph.query.clone();
            query.max_per_column = width;
            query.pages.clear();
            panels.graph.show(&set, query).map(|()| true)
        }
        GraphStep::Kind(kind) => panels.graph.toggle_kind(&set, kind).map(|()| true),
        GraphStep::More { side, hop } => panels.graph.more(&set, side, hop).map(|()| true),
        GraphStep::OpenSizeMap => panels.open_size_map(&set).map(|()| true),
    }
    .map_err(|e| audit_error(&e))?;
    panels.source = source;
    let mut answer = graph_json(&panels);
    if let Some(object) = answer.as_object_mut() {
        object.insert("moved".into(), json!(moved));
    }
    Ok(answer)
}

/// `editor.size_map_panel`: show a closure, lay it out and read a point of it.
fn size_map_panel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let assets = params.get("assets").filter(|v| !v.is_null());
    let folder = folder_param(params)?;
    let source = match (assets, folder) {
        (None, None) => None,
        (assets, folder) => {
            let (set, source) = set_of(world, params)?;
            let roots = match (assets, folder) {
                (Some(value), _) => str_list(value, "assets")?,
                (None, Some(folder)) => folder_roots(&set, &folder)?,
                (None, None) => Vec::new(),
            };
            let group_by = match params.get("group_by").and_then(Value::as_str) {
                None => GroupBy::Asset,
                Some(text) => GroupBy::parse(text).map_err(|e| audit_error(&e))?,
            };
            let query = SizeMapQuery {
                roots,
                group_by,
                top: params
                    .get("top")
                    .and_then(Value::as_u64)
                    .and_then(|n| u32::try_from(n).ok())
                    .unwrap_or(200),
            };
            panels_mut(world)
                .size_map
                .show(&set, query)
                .map_err(|e| audit_error(&e))?;
            Some(source)
        }
    };
    let mut panels = panels_mut(world);
    if let Some(source) = source {
        panels.source = source;
    }
    let area = params
        .get("area")
        .and_then(Value::as_array)
        .and_then(|a| Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32)));
    if let Some((w, h)) = area {
        panels.size_map.layout(w, h);
    }
    let mut answer = size_map_json(&panels);
    let point = params
        .get("hover")
        .and_then(Value::as_array)
        .and_then(|a| Some((a.first()?.as_f64()? as f32, a.get(1)?.as_f64()? as f32)));
    if let (Some((x, y)), Some(object)) = (point, answer.as_object_mut()) {
        let hover = panels.size_map.hover(x, y).map(|hover| {
            json!({
                "name": hover.name,
                "bytes": hover.bytes,
                "share_bp": hover.share_bp,
                "exclusive_bytes": hover.exclusive_bytes,
            })
        });
        object.insert("hover".into(), json!(hover));
    }
    Ok(answer)
}

/// The methods of this module.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.asset_panels_state",
                tool: "asset_panels_state",
                description: "What the Problems, Reference graph and Size map panels show: rows and filters, the query and its columns with the history flags, the treemap rows and cells, and the set's source.",
                params: &[ParamSchema {
                    name: "limit",
                    kind: "integer",
                    required: false,
                    description: "Most problem rows returned (default 500); rows_total has the full count.",
                }],
                result: "{ source, error, problems: { ran, summary, rows, rows_total, visible, hidden_severities, validator, validators }, graph: { query, result, can_back, can_forward }, size_map: { query, result, cells } }",
                mutating: false,
                permission: Permission::Read,
            },
            panels_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.problems_panel",
                tool: "problems_panel",
                description: "Drive the Problems panel: run the validators over the registry or `set` (re-runs the last scope), filter by severity and validator, and pick an asset (a selection event, nothing else).",
                params: &[
                    P_SET,
                    ParamSchema {
                        name: "run",
                        kind: "boolean",
                        required: false,
                        description: "Run the validators first.",
                    },
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: false,
                        description: "With `run`: validate these paths (default: the last scope, else the project).",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "With `run` and no `assets`: validate what is under this folder (assets/…), recursively: the Content Browser folder's Validate.",
                    },
                    ParamSchema {
                        name: "hide",
                        kind: "array",
                        required: false,
                        description: "Severities to hide: error, warning, info.",
                    },
                    ParamSchema {
                        name: "validator",
                        kind: "string",
                        required: false,
                        description: "Show one validator; null shows all.",
                    },
                    ParamSchema {
                        name: "select",
                        kind: "string",
                        required: false,
                        description: "Pick this asset, as a click on a row does.",
                    },
                    ParamSchema {
                        name: "group",
                        kind: "boolean",
                        required: false,
                        description: "Group by source (validator or system), as the Group by source checkbox.",
                    },
                    ParamSchema {
                        name: "collapse",
                        kind: "boolean",
                        required: false,
                        description: "Show identical consecutive rows once with a count (default true).",
                    },
                    ParamSchema {
                        name: "toggle_group",
                        kind: "string",
                        required: false,
                        description: "Open or collapse this source's group, as a click on its header.",
                    },
                    ParamSchema {
                        name: "select_row",
                        kind: "number",
                        required: false,
                        description: "Click this row (its `index`): picks its asset and selects it for copy.",
                    },
                    ParamSchema {
                        name: "select_entity",
                        kind: "string",
                        required: false,
                        description: "Select and frame the entity with this GUID, as a finding's entity link.",
                    },
                    ParamSchema {
                        name: "copy",
                        kind: "string",
                        required: false,
                        description: "all or selected: answer `copied`, the text Copy all or Copy selected copies.",
                    },
                    ParamSchema {
                        name: "open_graph_error",
                        kind: "number",
                        required: false,
                        description: "Follow this material graph error's Open link (its `index` in `graph_errors`): activate its tab, select its node, bring the Material Graph panel forward.",
                    },
                ],
                result: "{ ran, summary, rows: [{ severity, code, validator, asset, text, hint, index, count, entities }], rows_total, visible, hidden_severities, validator, validators, grouped, collapse_duplicates, groups: [{ source, rows, worst, open }], selected_row, graph_errors: [{ index, tab, label, path, kind, target, node, message }], copied?, selected_entity?, opened_graph_error?, logic_graph }",
                // `select_entity` replaces the selection and frames the camera, as
                // `editor.log_open_link` does (review sprint-w12-rev-g5 #18).
                mutating: true,
                permission: Permission::Play,
            },
            problems_panel as Handler,
        ),
        (
            MethodSchema {
                name: "editor.reference_graph_panel",
                tool: "reference_graph_panel",
                description: "One action of the Reference graph panel: open or recenter on an asset, back, forward, depth, width, toggle a kind of the type filter, page a column (more), open_size_map.",
                params: &[
                    P_SET,
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: true,
                        description: "open, recenter, back, forward, depth, width, kind, more or open_size_map.",
                    },
                    ParamSchema {
                        name: "asset",
                        kind: "string",
                        required: false,
                        description: "open, recenter: the asset path or id.",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "open without `asset`: every asset under this folder (assets/…) as the roots, the Content Browser folder's Reference graph (asset_folder_empty when none).",
                    },
                    ParamSchema {
                        name: "side",
                        kind: "string",
                        required: false,
                        description: "depth, more: referrers or dependencies.",
                    },
                    ParamSchema {
                        name: "depth",
                        kind: "integer",
                        required: false,
                        description: "depth: hops, 0..=8.",
                    },
                    ParamSchema {
                        name: "width",
                        kind: "integer",
                        required: false,
                        description: "width: nodes per column, 1..=500.",
                    },
                    ParamSchema {
                        name: "kind",
                        kind: "string",
                        required: false,
                        description: "kind: mesh, texture, material, scene, audio or other.",
                    },
                    ParamSchema {
                        name: "hop",
                        kind: "integer",
                        required: false,
                        description: "more: the column's hop.",
                    },
                ],
                result: "{ query, result, can_back, can_forward, moved }",
                mutating: false,
                permission: Permission::Read,
            },
            reference_graph_panel as Handler,
        ),
        (
            MethodSchema {
                name: "editor.size_map_panel",
                tool: "size_map_panel",
                description: "Show a closure in the Size map panel, lay the treemap out for a rectangle and read what a point shows.",
                params: &[
                    P_SET,
                    ParamSchema {
                        name: "assets",
                        kind: "array",
                        required: false,
                        description: "The closure's roots; omit to keep what the panel shows.",
                    },
                    ParamSchema {
                        name: "folder",
                        kind: "string",
                        required: false,
                        description: "Without `assets`: every asset under this folder (assets/…) as the roots, the Content Browser folder's Size map (asset_folder_empty when none).",
                    },
                    ParamSchema {
                        name: "group_by",
                        kind: "string",
                        required: false,
                        description: "asset (default), kind, folder:N or tag:KEY.",
                    },
                    ParamSchema {
                        name: "top",
                        kind: "integer",
                        required: false,
                        description: "Rows before `(other)` in asset grouping (default 200).",
                    },
                    ParamSchema {
                        name: "area",
                        kind: "array",
                        required: false,
                        description: "[w, h] of the treemap: fills `cells`.",
                    },
                    ParamSchema {
                        name: "hover",
                        kind: "array",
                        required: false,
                        description: "[x, y] in `area`: fills `hover` with bytes, share (parts per 10,000) and exclusive bytes.",
                    },
                ],
                result: "{ query, result, cells: [{ row, x, y, w, h }], hover? }",
                mutating: false,
                permission: Permission::Read,
            },
            size_map_panel as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review sprint-w12-rev-g5 #18: the problems panel's `select_entity` moves the selection and
    /// the camera, so a read-scoped token may not call it.
    #[test]
    fn the_problems_panel_method_needs_the_play_permission() {
        let (schema, _) = methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.problems_panel")
            .expect("registered");
        assert!(schema.mutating);
        assert_eq!(schema.permission, Permission::Play);
    }

    const SET: &str = r##"#![enable(implicit_some)]
    (
        format_version: 2,
        assets: [
            (path: "scenes/main.scene", kind: scene, references: ["meshes/rock.mesh", "textures/gone.png"], cooked_bytes: 10),
            (path: "meshes/rock.mesh", kind: mesh, references: ["textures/rock.png"], cooked_bytes: 100),
            (path: "textures/rock.png", kind: texture, cooked_bytes: 1000),
        ],
    )"##;

    fn call(world: &mut World, method: &str, params: &Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(m, _)| m.name == method)
            .expect("method registered");
        handler(world, params)
    }

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn every_panel_answers_without_a_run() {
        let mut world = World::new();
        let state = call(&mut world, "editor.asset_panels_state", &json!({})).expect("state");
        assert_eq!(state["source"], "none");
        assert_eq!(state["problems"]["ran"], false);
        assert_eq!(state["graph"]["can_back"], false);
    }

    #[test]
    fn without_a_registry_the_panels_name_the_code() {
        let mut world = World::new();
        let err = call(&mut world, "editor.problems_panel", &json!({ "run": true })).expect_err("no set");
        assert_eq!(code(&err), "registry_not_loaded");
        let err = call(
            &mut world,
            "editor.reference_graph_panel",
            &json!({ "action": "open", "asset": "a" }),
        )
        .expect_err("no set");
        assert_eq!(code(&err), "registry_not_loaded");
    }

    #[test]
    fn problems_run_filter_and_pick() {
        let mut world = World::new();
        let out = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "run": true, "set": SET }),
        )
        .expect("run");
        let total = out["rows_total"].as_u64().expect("rows");
        assert!(total > 0);
        assert!(
            out["validators"]
                .as_array()
                .expect("names")
                .iter()
                .any(|v| v == "missing_references")
        );

        let out = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "hide": ["error"], "validator": "missing_references" }),
        )
        .expect("filter");
        assert_eq!(out["visible"], 0);
        assert_eq!(out["rows_total"], total);
        let out = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "hide": [], "validator": null }),
        )
        .expect("clear");
        assert_eq!(out["visible"], total);

        call(
            &mut world,
            "editor.problems_panel",
            &json!({ "select": "meshes/rock.mesh" }),
        )
        .expect("pick");
        let picked = world.resource_mut::<AssetPanels>().take_selected();
        assert_eq!(picked.as_deref(), Some("meshes/rock.mesh"));

        let err = call(&mut world, "editor.problems_panel", &json!({ "hide": ["fatal"] })).expect_err("bad");
        assert!(err.message.contains("fatal"));
    }

    #[test]
    fn the_graph_panel_walks_its_actions_and_two_runs_match() {
        let walk = || {
            let mut world = World::new();
            let params = |action: Value| {
                let mut action = action;
                action["set"] = json!(SET);
                action
            };
            let open = call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "open", "asset": "scenes/main.scene" })),
            )
            .expect("open");
            assert_eq!(open["result"]["roots"][0], "scenes/main.scene");
            let deep = call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "depth", "side": "dependencies", "depth": 2 })),
            )
            .expect("depth");
            assert_eq!(deep["can_back"], true);
            let recentered = call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "recenter", "asset": "meshes/rock.mesh" })),
            )
            .expect("recenter");
            let back = call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "back" })),
            )
            .expect("back");
            assert_eq!(back["query"]["roots"][0], "scenes/main.scene");
            assert_eq!(back["can_forward"], true);
            call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "kind", "kind": "mesh" })),
            )
            .expect("kind");
            call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "more", "side": "dependencies", "hop": 1 })),
            )
            .expect("more");
            let sized = call(
                &mut world,
                "editor.reference_graph_panel",
                &params(json!({ "action": "open_size_map" })),
            )
            .expect("size map");
            (recentered, sized)
        };
        assert_eq!(walk(), walk());
    }

    #[test]
    fn graph_errors_carry_the_audit_codes() {
        let mut world = World::new();
        let err = call(
            &mut world,
            "editor.reference_graph_panel",
            &json!({ "action": "open", "asset": "nope", "set": SET }),
        )
        .expect_err("unknown");
        assert_eq!(code(&err), "unknown_asset");
        call(
            &mut world,
            "editor.reference_graph_panel",
            &json!({ "action": "open", "asset": "meshes/rock.mesh", "set": SET }),
        )
        .expect("open");
        let err = call(
            &mut world,
            "editor.reference_graph_panel",
            &json!({ "action": "depth", "side": "referrers", "depth": 9, "set": SET }),
        )
        .expect_err("too deep");
        assert_eq!(code(&err), "graph_bad_depth");
        let err = call(
            &mut world,
            "editor.reference_graph_panel",
            &json!({ "action": "sideways", "set": SET }),
        )
        .expect_err("bad action");
        assert!(err.message.contains("sideways"));
    }

    #[test]
    fn the_size_map_lays_out_and_reads_a_point() {
        let mut world = World::new();
        let out = call(
            &mut world,
            "editor.size_map_panel",
            &json!({ "assets": ["scenes/main.scene"], "set": SET, "area": [100, 100], "hover": [1, 1] }),
        )
        .expect("map");
        assert_eq!(out["result"]["total_bytes"], 1110);
        assert_eq!(out["cells"].as_array().expect("cells").len(), 3);
        assert_eq!(out["hover"]["name"], "textures/rock.png");

        let out = call(&mut world, "editor.size_map_panel", &json!({ "hover": [99, 99] })).expect("keep");
        assert!(out["hover"].is_object(), "{out}");

        let err = call(
            &mut world,
            "editor.size_map_panel",
            &json!({ "assets": ["scenes/main.scene"], "set": SET, "group_by": "color" }),
        )
        .expect_err("bad group");
        assert_eq!(code(&err), "size_map_bad_group");
    }

    #[test]
    fn a_failed_action_leaves_the_panel_as_it_was() {
        let mut world = World::new();
        call(
            &mut world,
            "editor.problems_panel",
            &json!({ "run": true, "set": SET }),
        )
        .expect("seed");
        let before = world.resource::<AssetPanels>().clone();
        let err = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "run": true, "hide": ["fatal"], "set": SET }),
        )
        .expect_err("bad severity");
        assert!(err.message.contains("fatal"), "{err:?}");
        assert_eq!(world.resource::<AssetPanels>(), &before);
        let err = call(
            &mut world,
            "editor.reference_graph_panel",
            &json!({ "action": "nope", "set": SET }),
        )
        .expect_err("bad action");
        assert!(err.message.contains("nope"), "{err:?}");
        assert_eq!(world.resource::<AssetPanels>(), &before);
    }

    /// P-154 (ADR-7789): grouping, duplicate collapse, a row click for Copy selected and an entity
    /// link through `editor.problems_panel`; a missing row is refused before any write.
    #[test]
    fn problems_group_collapse_copy_and_entity_links() {
        use regia_core::id::Guid;
        use regia_scene::prelude::{EntityGuid, Transform};
        let mut world = World::new();
        call(
            &mut world,
            "editor.problems_panel",
            &json!({ "run": true, "set": SET }),
        )
        .expect("run");
        let out = call(&mut world, "editor.problems_panel", &json!({ "group": true })).expect("group");
        assert_eq!(out["grouped"], true);
        let groups = out["groups"].as_array().expect("groups");
        assert!(
            groups
                .iter()
                .any(|g| g["source"] == "missing_references" && g["open"] == true),
            "{out}"
        );
        assert_eq!(out["groups"][0]["worst"], "error", "worst group first: {out}");
        let out = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "toggle_group": "missing_references" }),
        )
        .expect("toggle");
        assert!(
            out["groups"]
                .as_array()
                .expect("groups")
                .iter()
                .any(|g| g["source"] == "missing_references" && g["open"] == false),
            "{out}"
        );
        assert_eq!(out["rows"][0]["count"], 1);
        let err = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "select_row": 9999, "group": false }),
        )
        .expect_err("no such row");
        assert_eq!(code(&err), "problem_row_missing");
        assert!(
            world.resource::<AssetPanels>().problems.grouped,
            "a refusal writes nothing"
        );
        let out = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "select_row": 0, "copy": "selected" }),
        )
        .expect("select");
        assert_eq!(out["selected_row"], 0);
        assert!(
            out["copied"].as_str().is_some_and(|t| t.starts_with("error ")),
            "{out}"
        );
        let out = call(&mut world, "editor.problems_panel", &json!({ "copy": "all" })).expect("copy");
        assert!(
            out["copied"]
                .as_str()
                .is_some_and(|t| t.starts_with("[missing_references]")),
            "{out}"
        );

        let entity = world
            .spawn((EntityGuid(Guid::from_u128(7)), Transform::default()))
            .id();
        let guid = Guid::from_u128(7).to_string();
        let out = call(
            &mut world,
            "editor.problems_panel",
            &json!({ "select_entity": guid }),
        )
        .expect("entity");
        assert_eq!(out["selected_entity"], entity.to_bits());
        assert_eq!(
            world.resource::<crate::selection::Selection>().entities(),
            [entity]
        );
    }
}
