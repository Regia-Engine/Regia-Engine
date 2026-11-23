// SPDX-License-Identifier: MPL-2.0
//! The w12 parity methods of the material graph editor (ADR-7851): the Parameters panel
//! (`editor.matgraph_params`), instance reparenting and the inheritance view
//! (`editor.matgraph_instance_reparent`, `editor.matgraph_hierarchy`), per-profile shader
//! statistics (`editor.matgraph_cost`) and Find usages (`editor.matgraph_find_usages`).
//! `editor.matgraph_preview` takes `profile` and `node` (`agent.rs`).

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use super::MatGraphEditor;
use super::agent::{code_error, edit_answer, open_editor, refused, str_param};
use super::params_panel::{DeclKind, DeclValue};

type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

const fn p(name: &'static str, kind: &'static str, required: bool, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind,
        required,
        description,
    }
}

const fn m(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
    mutating: bool,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating,
        permission: if mutating {
            Permission::EditScene
        } else {
            Permission::Read
        },
    }
}

/// The w12 methods with their handlers.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.matgraph_params",
                "matgraph_params",
                "The open graph's Parameters panel: list its parameters, textures and static switches (type, default, slot order, the nodes that read each, a parameter's category, tooltip and range, and the category groups in panel order); set a default or add one (clamped to its range); set a parameter's category, tooltip or range (op meta; a new range clamps the default); move one to another slot; remove one. Each change is one undo step.",
                &const {
                    [
                        p(
                            "op",
                            "string",
                            false,
                            "list (default) | set | meta | reorder | remove.",
                        ),
                        p(
                            "kind",
                            "string",
                            false,
                            "param | texture | switch (set: taken from `value` when omitted).",
                        ),
                        p(
                            "name",
                            "string",
                            false,
                            "set, reorder, remove: the declaration's name.",
                        ),
                        p(
                            "value",
                            "any",
                            false,
                            "set: a parameter value ({ Float: 0.5 } | { Color: [r, g, b] } | RON), an image path (texture) or true | false (switch).",
                        ),
                        p("to", "number", false, "reorder: the new slot (0-based, clamped)."),
                        p(
                            "category",
                            "string",
                            false,
                            "meta: the parameter's category (\"\" = ungrouped).",
                        ),
                        p(
                            "tooltip",
                            "string",
                            false,
                            "meta: the parameter's tooltip (\"\" = none).",
                        ),
                        p(
                            "range",
                            "object",
                            false,
                            "meta: { min, max, step? } (step 0 = continuous), or null to remove the range.",
                        ),
                    ]
                },
                "{ rows: [{ kind, index, name, type, default, used_by, category, tooltip, range }], groups: [{ category, params }], added?, value?, readers?, revision?, ok?, errors? }",
                true,
            ),
            matgraph_params as Handler,
        ),
        (
            m(
                "editor.matgraph_instance_reparent",
                "matgraph_instance_reparent",
                "Give the open material instance another parent graph (one undo step): overrides the new parent declares with the same type are kept, the others dropped and listed. Save with editor.matgraph_save.",
                &const {
                    [p(
                        "parent",
                        "string",
                        true,
                        "The new parent .matgraph.ron, project-relative (assets/…).",
                    )]
                },
                "{ parent, kept, dropped, revision, ok, errors }",
                true,
            ),
            matgraph_instance_reparent as Handler,
        ),
        (
            m(
                "editor.matgraph_hierarchy",
                "matgraph_hierarchy",
                "The project's material graphs with the instances whose parent resolves to each, and the instances whose parent is missing (the Hierarchy view).",
                &[],
                "{ graphs: [{ path, name, instances: [{ path, parent }] }], orphans }",
                false,
            ),
            matgraph_hierarchy as Handler,
        ),
        (
            m(
                "editor.matgraph_cost",
                "matgraph_cost",
                "Shader statistics of the open document per render profile the permutation policy enumerates: IR instructions of the colour pass's fragment entry and of the graph code, texture samples, bindings, samplers, passes; or one profile.",
                &const {
                    [p(
                        "profile",
                        "string",
                        false,
                        "One profile (DesktopHigh, mobile_low, …); default every policy profile.",
                    )]
                },
                "{ profiles: [{ profile, instanced, passes, instructions, graph_instructions, texture_samples, graph_texture_samples, bindings, samplers, error }] }",
                false,
            ),
            matgraph_cost as Handler,
        ),
        (
            m(
                "editor.matgraph_find_usages",
                "matgraph_find_usages",
                "Find the project's graphs, functions and instances that use a parameter, texture, switch, function or constant: by kind and name, or from a node of the open document (the node menu's Find usages).",
                &const {
                    [
                        p(
                            "kind",
                            "string",
                            false,
                            "param | texture | switch | function | constant.",
                        ),
                        p(
                            "name",
                            "string",
                            false,
                            "The name (constant: its node RON, e.g. Float(0.5)).",
                        ),
                        p(
                            "node",
                            "string",
                            false,
                            "A node of the open document instead of kind and name.",
                        ),
                    ]
                },
                "{ kind, name, usages: [{ path, doc, nodes, how }] }",
                false,
            ),
            matgraph_find_usages as Handler,
        ),
    ]
}

fn params_error(message: String) -> RpcError {
    code_error(
        "matgraph_params_invalid",
        message,
        "editor.matgraph_params lists the declarations (op list)",
    )
}

fn rows_json(world: &mut World) -> Result<Value, RpcError> {
    let editor = open_editor(world)?;
    let rows: Vec<Value> = super::params_panel::rows(&editor.doc.graph)
        .iter()
        .map(super::params_panel::row_json)
        .collect();
    Ok(Value::Array(rows))
}

/// `editor.matgraph_params`.
fn matgraph_params(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op").unwrap_or("list");
    let kind = str_param(params, "kind")
        .map(|k| {
            DeclKind::parse(k)
                .ok_or_else(|| params_error(format!("unknown kind `{k}`: param, texture or switch")))
        })
        .transpose()?;
    let name = || {
        str_param(params, "name")
            .map(str::to_string)
            .ok_or_else(|| params_error(format!("`name` is required for op {op}")))
    };
    {
        let editor = open_editor(world)?;
        if op != "list" && !matches!(editor.doc.mode, super::DocMode::Graph) {
            return Err(refused(format!(
                "{}the declarations belong to a graph: open it (an instance overrides them with editor.matgraph_instance_set)",
                super::model::READ_ONLY
            )));
        }
    }
    let mut extra = serde_json::Map::new();
    match op {
        "list" => {}
        "set" => {
            let name = name()?;
            let raw = params
                .get("value")
                .ok_or_else(|| params_error("`value` is required for op set".into()))?;
            let value = decl_value(world, kind, &name, raw)?;
            let added = super::edit(world, "declaration default", None, move |d| {
                super::params_panel::set(&mut d.graph, &name, value)
            })
            .map_err(params_error)?;
            extra.insert("added".into(), json!(added));
        }
        "meta" => {
            let name = name()?;
            let meta = param_meta(params)?;
            let value = super::edit(world, "parameter details", None, move |d| {
                super::params_panel::set_meta(&mut d.graph, &name, meta)
            })
            .map_err(params_error)?;
            extra.insert("value".into(), serde_json::to_value(value).unwrap_or(Value::Null));
        }
        "reorder" => {
            let name = name()?;
            let kind = kind.ok_or_else(|| params_error("`kind` is required for op reorder".into()))?;
            let to = params
                .get("to")
                .and_then(Value::as_u64)
                .ok_or_else(|| params_error("`to` (a slot number) is required for op reorder".into()))?;
            let to = usize::try_from(to).unwrap_or(usize::MAX);
            let at = super::edit(world, "reorder declaration", None, move |d| {
                super::params_panel::reorder(&mut d.graph, kind, &name, to)
            })
            .map_err(params_error)?;
            extra.insert("index".into(), json!(at));
        }
        "remove" => {
            let name = name()?;
            let kind = kind.ok_or_else(|| params_error("`kind` is required for op remove".into()))?;
            let readers = super::edit(world, "remove declaration", None, move |d| {
                super::params_panel::remove(&mut d.graph, kind, &name)
            })
            .map_err(params_error)?;
            extra.insert("readers".into(), json!(readers));
        }
        other => {
            return Err(params_error(format!(
                "unknown op `{other}`: list, set, meta, reorder or remove"
            )));
        }
    }
    let rows = rows_json(world)?;
    let groups = super::params_panel::groups_json(&open_editor(world)?.doc.graph);
    let mut out = json!({ "rows": rows, "groups": groups });
    if let Value::Object(map) = &mut out {
        map.extend(extra);
    }
    Ok(if op == "list" {
        out
    } else {
        edit_answer(world, out)
    })
}

/// The `meta` op's change: `category`, `tooltip` (strings) and `range` (`{ min, max, step? }` or
/// null), each optional but at least one present.
fn param_meta(params: &Value) -> Result<super::params_panel::ParamMeta, RpcError> {
    let text = |key: &str| -> Result<Option<String>, RpcError> {
        match params.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(params_error(format!("`{key}` is a string"))),
        }
    };
    let range = match params.get("range") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(raw) => {
            let num = |key: &str, required: bool| -> Result<f32, RpcError> {
                match raw.get(key).and_then(Value::as_f64) {
                    Some(x) => Ok(x as f32),
                    None if !required && raw.get(key).is_none() => Ok(0.0),
                    None => Err(params_error(format!("`range.{key}` is a number"))),
                }
            };
            Some(Some(regia_matgraph::ParamRange {
                min: num("min", true)?,
                max: num("max", true)?,
                step: num("step", false)?,
            }))
        }
    };
    let meta = super::params_panel::ParamMeta {
        category: text("category")?,
        tooltip: text("tooltip")?,
        range,
    };
    if meta == super::params_panel::ParamMeta::default() {
        return Err(params_error(
            "op meta needs `category`, `tooltip` or `range`".into(),
        ));
    }
    Ok(meta)
}

/// The new default of `name` from JSON: a parameter value shaped like the declaration (or any
/// tagged value), an image path, or a boolean.
fn decl_value(
    world: &mut World,
    kind: Option<DeclKind>,
    name: &str,
    raw: &Value,
) -> Result<DeclValue, RpcError> {
    let declared = {
        let editor = open_editor(world)?;
        editor
            .doc
            .graph
            .params
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value)
    };
    let kind = kind.unwrap_or(match raw {
        Value::Bool(_) => DeclKind::Switch,
        Value::String(s) if s.contains('.') && !s.contains('(') => DeclKind::Texture,
        _ => DeclKind::Param,
    });
    match kind {
        DeclKind::Switch => raw
            .as_bool()
            .map(DeclValue::Switch)
            .ok_or_else(|| params_error("a switch default is true or false".into())),
        DeclKind::Texture => raw
            .as_str()
            .map(|s| DeclValue::Texture(s.to_string()))
            .ok_or_else(|| params_error("a texture default is an image path".into())),
        DeclKind::Param => super::agent::param_value(raw, declared).map(DeclValue::Param),
    }
}

/// `editor.matgraph_instance_reparent`.
fn matgraph_instance_reparent(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let raw = super::agent::required(params, "parent")?;
    let path = super::agent::request_path_ext(world, raw, ".matgraph.ron")?;
    let bad = |e: String| {
        code_error(
            "matgraph_reparent_invalid",
            e,
            "open an instance with editor.matgraph_instance_open; editor.matgraph_hierarchy lists the graphs",
        )
    };
    {
        let editor = open_editor(world)?;
        if editor.doc.instance_meta().is_none() {
            return Err(bad("the open document is not a material instance".into()));
        }
    }
    if !path.is_file() {
        return Err(bad(format!("no graph {}", path.display())));
    }
    let root = crate::agent_ext::agent_root(world);
    {
        let mut editor = open_editor(world)?;
        if editor.root.is_none() {
            editor.root = Some(root);
        }
    }
    let report = super::hierarchy::reparent_open(world, &path).map_err(bad)?;
    Ok(edit_answer(world, report.to_json()))
}

/// `editor.matgraph_hierarchy`.
fn matgraph_hierarchy(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = crate::agent_ext::agent_root(world);
    let tree = super::hierarchy::scan(&root);
    let out = tree.to_json();
    world
        .get_resource_or_insert_with(MatGraphEditor::default)
        .ui
        .parity
        .hierarchy = Some(Ok(tree));
    Ok(out)
}

/// `editor.matgraph_cost`.
fn matgraph_cost(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let profiles = match str_param(params, "profile") {
        Some(name) => vec![super::cost::parse_profile(name).map_err(|e| {
            code_error(
                "matgraph_profile_unknown",
                e,
                "the profiles are the permutation policy's (editor.matgraph_compile lists them)",
            )
        })?],
        None => super::cost::profiles(),
    };
    let graph = {
        let editor = open_editor(world)?;
        super::model::resolved_live(
            &editor.doc.view_graph(),
            editor.graph_dir().as_deref(),
            editor.root.as_deref(),
            &editor.live_functions(),
        )
    }
    .map_err(|e| cost_error(&e))?;
    let rows = profiles
        .into_iter()
        .map(|p| super::cost::profile_stats(&graph, p).map(|s| super::cost::stats_json(&s)))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| cost_error(&e))?;
    Ok(json!({ "profiles": rows }))
}

fn cost_error(e: &str) -> RpcError {
    code_error(
        "matgraph_cost_failed",
        format!("the document does not compile: {e}"),
        "fix the errors editor.matgraph_state lists",
    )
}

/// `editor.matgraph_find_usages`.
fn matgraph_find_usages(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bad = |e: String| {
        code_error(
            "matgraph_usage_invalid",
            e,
            "pass kind (param, texture, switch, function, constant) and name, or node",
        )
    };
    let root = crate::agent_ext::agent_root(world);
    let query = match str_param(params, "node") {
        Some(node) => {
            let editor = open_editor(world)?;
            let graph = editor.doc.view_graph().into_owned();
            super::usages::query_for_node(&graph, node, editor.graph_dir().as_deref(), Some(&root))
                .ok_or_else(|| bad(format!("node `{node}` is not a parameter, texture, switch, call or constant node (or does not exist)")))?
        }
        None => {
            let kind = str_param(params, "kind").ok_or_else(|| bad("`kind` or `node` is required".into()))?;
            let kind =
                super::usages::UsageKind::parse(kind).ok_or_else(|| bad(format!("unknown kind `{kind}`")))?;
            let name =
                str_param(params, "name").ok_or_else(|| bad("`name` is required with `kind`".into()))?;
            super::usages::UsageQuery {
                kind,
                name: name.to_string(),
                file: None,
            }
        }
    };
    let found = super::usages::find_usages(&root, Some(&root), &query);
    let out = json!({
        "kind": query.kind.name(),
        "name": &query.name,
        "usages": found.iter().map(super::usages::Usage::to_json).collect::<Vec<_>>(),
    });
    if let Some(mut editor) = world.get_resource_mut::<MatGraphEditor>() {
        editor.ui.parity.usages = Some((query, Ok(found)));
    }
    Ok(out)
}
