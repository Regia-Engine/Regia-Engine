// SPDX-License-Identifier: MPL-2.0
//! The Stage 2 agent surface of the material graph editor (ADR-9700,
//! `book/agents/material-graph-editor.md`): typed node rows, the node search, the clipboard,
//! align / distribute, comments and reroutes, per-node previews, Apply to selection, and the
//! Material panel's shape preview (`editor.material_preview`).
//!
//! Every mutating method is one undo step through [`super::edit`] (Apply to selection through
//! the level's `SetProperty` group), exactly as the panel's buttons.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_matgraph::{FunctionSource, Node, ParamValue};

use super::agent::{
    code_error, edit_answer, open_editor, pos_param, refused, required, str_param, string_list,
};
use super::inspector::{self, FieldKind, FieldValue};
use super::{DocMode, MatGraphEditor, edit};
use crate::material_editor::{PreviewGeometry, PreviewShape, PreviewView};

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

/// The Stage 2 `editor.matgraph_*` methods with their handlers.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.matgraph_node_fields",
                "matgraph_node_fields",
                "The selected-node inspector's typed rows of a node: each editable value with its kind (float, vec2..4, int, param, texture, switch, input, function, call_output, channel, normal_blend, channels) and, for pickers, the names it offers. Wires are not rows.",
                &const { [p("name", "string", true, "Node name.")] },
                "{ name, kind, fields: [{ name, kind, value, options? }] }",
                false,
            ),
            node_fields as Handler,
        ),
        (
            m(
                "editor.matgraph_node_set",
                "matgraph_node_set",
                "Set one typed row of a node (one undo step; repeated sets of the same row coalesce like a drag). A Param node also takes field `default` (its declared default value). Noise octaves clamp to 1..8.",
                &const {
                    [
                        p("name", "string", true, "Node name."),
                        p(
                            "field",
                            "string",
                            true,
                            "Row name from editor.matgraph_node_fields (or `default` on a Param).",
                        ),
                        p(
                            "value",
                            "any",
                            true,
                            "Number, array of numbers, whole number, name, or null.",
                        ),
                    ]
                },
                "{ name, field, node, revision, ok, errors }",
                true,
            ),
            node_set as Handler,
        ),
        (
            m(
                "editor.matgraph_search",
                "matgraph_search",
                "The canvas's fuzzy node search: kinds matching `query`, best first, filtered to the kinds that can take a wire from node `from` (its output) or into `to`.`slot` / surface `output`. With `add`, adds that kind at `pos` wired to the pin (one undo step).",
                &const {
                    [
                        p("query", "string", false, "Letters to match (empty: every kind)."),
                        p("from", "string", false, "A wire from this node's output."),
                        p("to", "string", false, "A wire into this node's input `slot`."),
                        p("slot", "string", false, "The input slot of `to`."),
                        p("output", "string", false, "A wire into this surface output slot."),
                        p("add", "string", false, "Add a node of this kind, wired."),
                        p("pos", "array", false, "[x, y] canvas position for `add`."),
                    ]
                },
                "{ kinds, added?, revision?, ok?, errors? }",
                true,
            ),
            search as Handler,
        ),
        (
            m(
                "editor.matgraph_find",
                "matgraph_find",
                "The read-only twin of editor.matgraph_search: the same fuzzy node-search listing \
                (kinds matching `query`, best first, filtered to the kinds that can take a wire from \
                node `from` or into `to`.`slot` / surface `output`) without adding anything. Answers \
                for a read-only agent; to add a node, call editor.matgraph_search with `add`.",
                &const {
                    [
                        p("query", "string", false, "Letters to match (empty: every kind)."),
                        p("from", "string", false, "A wire from this node's output."),
                        p("to", "string", false, "A wire into this node's input `slot`."),
                        p("slot", "string", false, "The input slot of `to`."),
                        p("output", "string", false, "A wire into this surface output slot."),
                    ]
                },
                "{ kinds }",
                false,
            ),
            find as Handler,
        ),
        (
            m(
                "editor.matgraph_clipboard",
                "matgraph_clipboard",
                "Copy, cut or paste nodes. The clipboard is RON of the nodes and their relative positions, shared by every tab; paste renames clashing names (references follow) and leaves references outside the copied set unwired. Cut and paste are one undo step each.",
                &const {
                    [
                        p("op", "string", true, "copy | cut | paste."),
                        p("nodes", "array", false, "copy/cut: node names."),
                        p(
                            "pos",
                            "array",
                            false,
                            "paste: [x, y] of the selection's top-left corner (default [0, 0]).",
                        ),
                        p(
                            "text",
                            "string",
                            false,
                            "paste: clipboard text (default: the editor's clipboard).",
                        ),
                    ]
                },
                "{ op, text?, count?, renamed?, revision?, ok?, errors? }",
                true,
            ),
            clipboard as Handler,
        ),
        (
            m(
                "editor.matgraph_arrange",
                "matgraph_arrange",
                "Align or distribute nodes (one undo step): align_left | align_right | align_top | align_bottom | align_center_x | align_center_y (two or more nodes), distribute_x | distribute_y (three or more).",
                &const {
                    [
                        p("op", "string", true, "The arrange operation."),
                        p("nodes", "array", true, "Node names."),
                    ]
                },
                "{ op, revision, ok, errors }",
                true,
            ),
            arrange as Handler,
        ),
        (
            m(
                "editor.matgraph_annotate",
                "matgraph_annotate",
                "Comment frames and edge reroute points (saved to the layout sidecar v2 only; one undo step each). op: comment_add {text, min, max}, comment_set {index, text?, min?, max? or rect?, move_nodes?} (move_nodes: the nodes inside the old frame move with its top-left corner, as a canvas header drag), comment_remove {index}, reroute_set {to, slot, points} (empty points clear; the canvas draws the wire through the points).",
                &const {
                    [
                        p(
                            "op",
                            "string",
                            true,
                            "comment_add | comment_set | comment_remove | reroute_set.",
                        ),
                        p(
                            "index",
                            "number",
                            false,
                            "comment_set/comment_remove: the comment's index.",
                        ),
                        p("text", "string", false, "The comment's text."),
                        p("min", "array", false, "[x, y] corner."),
                        p("max", "array", false, "[x, y] opposite corner."),
                        p(
                            "rect",
                            "array",
                            false,
                            "comment_set: [[x, y], [x, y]], the two corners (instead of min and max).",
                        ),
                        p(
                            "move_nodes",
                            "boolean",
                            false,
                            "comment_set: move the nodes inside the frame with it (default false).",
                        ),
                        p(
                            "to",
                            "string",
                            false,
                            "reroute_set: the node whose input the wire feeds.",
                        ),
                        p("slot", "string", false, "reroute_set: that input."),
                        p("points", "array", false, "reroute_set: [[x, y], …] in order."),
                    ]
                },
                "{ op, comments, reroutes, index?, moved?, revision }",
                true,
            ),
            annotate as Handler,
        ),
        (
            m(
                "editor.matgraph_node_preview",
                "matgraph_node_preview",
                "Render one node's value as a swatch: the node routed to an unlit base color in a derived graph (fast compile path), on the preview sphere. Live debugging of graph values in a running game is out of scope (spec 80 § 10).",
                &const {
                    [
                        p("node", "string", true, "Node name."),
                        p("size", "number", false, "Pixels, 64–512 (default 96)."),
                    ]
                },
                "{ node, size, png, mean_rgb, gray_levels, key }",
                false,
            ),
            node_preview as Handler,
        ),
        (
            m(
                "editor.matgraph_apply_to_selection",
                "matgraph_apply_to_selection",
                "Assign the open graph or instance (its saved file, project-relative) to the selected mesh owners' MeshSource.material, as one undo step (the content browser's material drop edit).",
                &[],
                "{ path, owners }",
                true,
            ),
            apply_to_selection as Handler,
        ),
    ]
}

/// The Material panel's agent methods (`editor.material_*`).
#[must_use]
pub fn material_methods() -> Vec<(MethodSchema, Handler)> {
    vec![(
        m(
            "editor.material_preview",
            "material_preview",
            "Render a .material.ron on the Material panel's preview shape (sphere, cube, plane or a project glTF mesh) from an orbit with a turned light: the panel's preview, read from the file.",
            &const {
                [
                    p(
                        "path",
                        "string",
                        true,
                        "Project-relative (assets/…) or absolute .material.ron.",
                    ),
                    p(
                        "shape",
                        "string",
                        false,
                        "sphere (default) | cube | plane | mesh.",
                    ),
                    p("mesh", "string", false, "shape mesh: a project .glb/.gltf."),
                    p("yaw", "number", false, "Orbit around the vertical, radians."),
                    p("pitch", "number", false, "Orbit up or down, radians."),
                    p("light_yaw", "number", false, "Turn the light, radians."),
                    p("width", "number", false, "Pixels, 64–1024 (default 256)."),
                    p("height", "number", false, "Pixels, 64–1024 (default 256)."),
                ]
            },
            "{ path, shape, width, height, png, mean_rgb, center_rgb, coverage }",
            false,
        ),
        material_preview as Handler,
    )]
}

fn number(params: &Value, name: &str) -> Result<f32, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(0.0),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

/// The preview shape and view of a request (`shape`, `mesh`, `yaw`, `pitch`, `light_yaw`); the
/// mesh is loaded here, so a bad one is `matgraph_preview_mesh_failed` before any render.
pub(super) fn preview_shape_params(
    world: &World,
    params: &Value,
) -> Result<(std::sync::Arc<PreviewGeometry>, PreviewView), RpcError> {
    let shape = PreviewShape::parse(
        str_param(params, "shape").unwrap_or("sphere"),
        str_param(params, "mesh"),
    )
    .map_err(RpcError::invalid_params)?;
    // A mesh path is confined to the project like any agent path (ADR-1816).
    let shape = match shape {
        PreviewShape::Mesh(path) => {
            let full = crate::mesh_source::agent_asset_path(world, "mesh", &path).map_err(|e| {
                code_error(
                    "matgraph_preview_mesh_failed",
                    e.message,
                    "pass a project .glb or .gltf under assets/",
                )
            })?;
            PreviewShape::Mesh(full.display().to_string())
        }
        other => other,
    };
    let geometry = crate::material_editor::preview_geometry(&shape, None).map_err(|e| {
        code_error(
            "matgraph_preview_mesh_failed",
            format!("the preview mesh cannot load: {e}"),
            "pass a project .glb or .gltf with at most 250000 vertices",
        )
    })?;
    let view = PreviewView {
        yaw: number(params, "yaw")?,
        pitch: number(params, "pitch")?,
        light_yaw: number(params, "light_yaw")?,
    };
    Ok((geometry, view))
}

fn node_missing(name: &str) -> RpcError {
    code_error(
        "matgraph_node_missing",
        format!("no node `{name}`"),
        "editor.matgraph_state lists the nodes",
    )
}

/// The names a picker row offers, from the open document.
pub(super) fn options(editor: &MatGraphEditor, node: &Node, kind: FieldKind) -> Option<Vec<String>> {
    let graph = &editor.doc.graph;
    Some(match kind {
        FieldKind::Param => graph.params.iter().map(|d| d.name.clone()).collect(),
        FieldKind::Texture => graph.textures.iter().map(|d| d.name.clone()).collect(),
        FieldKind::Switch => graph.switches.iter().map(|d| d.name.clone()).collect(),
        FieldKind::Function => graph.functions.keys().cloned().collect(),
        FieldKind::Input => editor
            .doc
            .function_meta()
            .map(|m| m.inputs.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default(),
        FieldKind::CallOutput => {
            let Node::Call { function, .. } = node else {
                return None;
            };
            match graph.functions.get(function) {
                Some(FunctionSource::Inline(f)) => f.outputs.keys().cloned().collect(),
                _ => super::function_definition(editor, function)
                    .map(|(f, _)| f.outputs.keys().cloned().collect())
                    .unwrap_or_default(),
            }
        }
        FieldKind::Channel => inspector::CHANNELS.iter().map(|s| (*s).to_string()).collect(),
        FieldKind::NormalBlend => inspector::NORMAL_BLENDS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        _ => return None,
    })
}

/// `editor.matgraph_node_fields`.
fn node_fields(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required(params, "name")?;
    let editor = open_editor(world)?;
    let node = editor
        .doc
        .graph
        .nodes
        .get(name)
        .ok_or_else(|| node_missing(name))?;
    let rows: Vec<Value> = inspector::fields(node)
        .into_iter()
        .map(|f| {
            let mut row = json!({
                "name": f.name,
                "kind": inspector::kind_name(f.kind),
                "value": f.value.to_json(),
            });
            if let Some(names) = options(&editor, node, f.kind) {
                row["options"] = json!(names);
            }
            row
        })
        .collect();
    let mut out = json!({ "name": name, "kind": node.kind(), "fields": rows });
    if let Node::Param(param) = node
        && let Some(decl) = editor.doc.graph.params.iter().find(|d| &d.name == param)
    {
        out["default"] = serde_json::to_value(decl.value).unwrap_or(Value::Null);
    }
    Ok(out)
}

/// `editor.matgraph_node_set`.
fn node_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required(params, "name")?.to_string();
    let field = required(params, "field")?.to_string();
    let value = params
        .get("value")
        .ok_or_else(|| RpcError::invalid_params("`value` is required"))?;
    let node = {
        let editor = open_editor(world)?;
        editor
            .doc
            .graph
            .nodes
            .get(&name)
            .cloned()
            .ok_or_else(|| node_missing(&name))?
    };
    if let (Node::Param(param), "default") = (&node, field.as_str()) {
        let declared = world
            .resource::<MatGraphEditor>()
            .doc
            .graph
            .params
            .iter()
            .find(|p| p.name == *param)
            .map(|p| p.value)
            .ok_or_else(|| refused(format!("the graph declares no parameter `{param}`")))?;
        let parsed = param_from_json(declared, value).map_err(RpcError::invalid_params)?;
        let param = param.clone();
        edit(
            world,
            "parameter default",
            Some(format!("param_default:{param}")),
            move |d| {
                let decl = d
                    .graph
                    .params
                    .iter_mut()
                    .find(|p| p.name == param)
                    .ok_or_else(|| format!("no parameter `{param}`"))?;
                decl.value = parsed;
                Ok(())
            },
        )
        .map_err(refused)?;
    } else {
        let kind = inspector::fields(&node)
            .into_iter()
            .find(|f| f.name == field)
            .map(|f| f.kind)
            .ok_or_else(|| refused(format!("`{}` has no typed field `{field}`", node.kind())))?;
        let parsed = FieldValue::from_json(kind, value)
            .map_err(|e| RpcError::invalid_params(format!("`value` for `{field}`: {e}")))?;
        super::set_node_field(world, &name, &field, parsed).map_err(refused)?;
    }
    let node = world
        .resource::<MatGraphEditor>()
        .doc
        .graph
        .nodes
        .get(&name)
        .cloned();
    let text = node
        .as_ref()
        .and_then(|n| ron::ser::to_string(n).ok())
        .unwrap_or_default();
    Ok(edit_answer(
        world,
        json!({ "name": name, "field": field, "node": text }),
    ))
}

/// A parameter value of the declared type from JSON (a number or an array).
fn param_from_json(declared: ParamValue, value: &Value) -> Result<ParamValue, String> {
    let list: Vec<f32> = match value {
        Value::Number(n) => vec![n.as_f64().unwrap_or(f64::NAN) as f32],
        Value::Array(a) => a.iter().map(|v| v.as_f64().unwrap_or(f64::NAN) as f32).collect(),
        _ => return Err("expected a number or an array of numbers".into()),
    };
    if !list.iter().all(|v| v.is_finite()) {
        return Err("the value must be finite numbers".into());
    }
    let take = |n: usize| -> Result<Vec<f32>, String> {
        if list.len() == n {
            Ok(list.clone())
        } else {
            Err(format!("expected {n} number(s)"))
        }
    };
    Ok(match declared {
        ParamValue::Float(_) => ParamValue::Float(take(1)?[0]),
        ParamValue::Vec2(_) => {
            let v = take(2)?;
            ParamValue::Vec2([v[0], v[1]])
        }
        ParamValue::Vec3(_) => {
            let v = take(3)?;
            ParamValue::Vec3([v[0], v[1], v[2]])
        }
        ParamValue::Color(_) => {
            let v = take(3)?;
            ParamValue::Color([v[0], v[1], v[2]])
        }
        ParamValue::Vec4(_) => {
            let v = take(4)?;
            ParamValue::Vec4([v[0], v[1], v[2], v[3]])
        }
    })
}

/// The wire a search request names.
fn wire_from(params: &Value) -> Result<Option<super::search::WireFrom>, RpcError> {
    use super::search::WireFrom;
    Ok(
        match (
            str_param(params, "from"),
            str_param(params, "to"),
            str_param(params, "output"),
        ) {
            (Some(from), None, None) => Some(WireFrom::Output(from.to_string())),
            (None, Some(to), None) => Some(WireFrom::Input {
                node: to.to_string(),
                slot: required(params, "slot")?.to_string(),
            }),
            (None, None, Some(slot)) => Some(WireFrom::Surface(slot.to_string())),
            (None, None, None) => None,
            _ => {
                return Err(RpcError::invalid_params(
                    "give at most one of `from`, `to` (with `slot`) and `output`",
                ));
            }
        },
    )
}

/// `editor.matgraph_search`.
fn search(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let query = str_param(params, "query").unwrap_or("");
    let from = wire_from(params)?;
    let function_doc = {
        let editor = open_editor(world)?;
        matches!(editor.doc.mode, DocMode::Function(_))
    };
    let kinds = super::search::search_kinds(query, from.as_ref(), function_doc);
    let Some(kind) = str_param(params, "add") else {
        return Ok(json!({ "kinds": kinds }));
    };
    let pos = pos_param(params)?.unwrap_or([0.0, 0.0]);
    let added = super::search_add(world, kind, pos, from).map_err(refused)?;
    {
        let mut editor = world.resource_mut::<MatGraphEditor>();
        editor.ui.selected = Some(added.clone());
    }
    Ok(edit_answer(world, json!({ "kinds": kinds, "added": added })))
}

/// `editor.matgraph_find`: the same listing as [`search`] with no `add` (a read-only agent's
/// node search, rev-claude-47-k finding 3: the mutating flag is per method, so the listing gets
/// its own).
fn find(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(add) = params.get("add").filter(|v| !v.is_null()) {
        return Err(RpcError::invalid_params(format!(
            "editor.matgraph_find does not add nodes (`add` is {add}); call editor.matgraph_search \
             with `add` for that"
        )));
    }
    let query = str_param(params, "query").unwrap_or("");
    let from = wire_from(params)?;
    let function_doc = {
        let editor = open_editor(world)?;
        matches!(editor.doc.mode, DocMode::Function(_))
    };
    let kinds = super::search::search_kinds(query, from.as_ref(), function_doc);
    Ok(json!({ "kinds": kinds }))
}

/// `editor.matgraph_clipboard`.
fn clipboard(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = required(params, "op")?.to_string();
    open_editor(world)?;
    match op.as_str() {
        "copy" | "cut" => {
            let nodes: BTreeSet<String> = string_list(params, "nodes")?.into_iter().collect();
            let text = if op == "copy" {
                super::copy_selection(world, &nodes)
            } else {
                super::cut_selection(world, &nodes)
            }
            .map_err(refused)?;
            Ok(edit_answer(
                world,
                json!({ "op": op, "text": text, "count": nodes.len() }),
            ))
        }
        "paste" => {
            let at = pos_param(params)?.unwrap_or([0.0, 0.0]);
            let renamed = super::paste(world, str_param(params, "text"), at).map_err(refused)?;
            Ok(edit_answer(world, json!({ "op": op, "renamed": renamed })))
        }
        other => Err(RpcError::invalid_params(format!(
            "unknown clipboard op `{other}` (copy, cut, paste)"
        ))),
    }
}

/// `editor.matgraph_arrange`.
fn arrange(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = super::clipboard::Arrange::parse(required(params, "op")?).map_err(RpcError::invalid_params)?;
    let nodes: BTreeSet<String> = string_list(params, "nodes")?.into_iter().collect();
    open_editor(world)?;
    super::arrange_nodes(world, &nodes, op).map_err(refused)?;
    Ok(edit_answer(world, json!({ "op": op.name() })))
}

fn point(value: Option<&Value>, name: &str) -> Result<Option<[f32; 2]>, RpcError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params(format!("`{name}` must be [x, y], finite numbers"));
    let a = value.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
    match (a[0].as_f64(), a[1].as_f64()) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok(Some([x as f32, y as f32])),
        _ => Err(bad()),
    }
}

/// Two corners of a comment frame.
type Corners = ([f32; 2], [f32; 2]);

/// The optional `rect` parameter: `[[x, y], [x, y]]`.
fn rect_param(params: &Value) -> Result<Option<Corners>, RpcError> {
    let Some(value) = params.get("rect") else {
        return Ok(None);
    };
    let corners = value
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| RpcError::invalid_params("`rect` must be [[x, y], [x, y]]"))?;
    match (point(corners.first(), "rect")?, point(corners.get(1), "rect")?) {
        (Some(a), Some(b)) => Ok(Some((a, b))),
        _ => Err(RpcError::invalid_params("`rect` must be [[x, y], [x, y]]")),
    }
}

/// `editor.matgraph_annotate`.
fn annotate(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = required(params, "op")?.to_string();
    open_editor(world)?;
    let index = params
        .get("index")
        .and_then(Value::as_u64)
        .map(|i| usize::try_from(i).unwrap_or(usize::MAX));
    let text = str_param(params, "text").map(str::to_string);
    let min = point(params.get("min"), "min")?;
    let max = point(params.get("max"), "max")?;
    let mut new_index = None;
    let mut moved: Option<Vec<String>> = None;
    match op.as_str() {
        "comment_add" => {
            let (Some(min), Some(max)) = (min, max) else {
                return Err(RpcError::invalid_params("comment_add needs `min` and `max`"));
            };
            let text = text.unwrap_or_default();
            new_index = Some(
                edit(world, "add comment", None, move |d| {
                    d.add_comment(&text, min, max)
                })
                .map_err(refused)?,
            );
        }
        "comment_set" => {
            let index = index.ok_or_else(|| RpcError::invalid_params("comment_set needs `index`"))?;
            let rect = match (min, max, rect_param(params)?) {
                (Some(a), Some(b), None) => Some((a, b)),
                (None, None, rect) => rect,
                (_, _, Some(_)) => {
                    return Err(RpcError::invalid_params(
                        "give `rect` or `min` and `max`, not both",
                    ));
                }
                _ => return Err(RpcError::invalid_params("give both `min` and `max`, or neither")),
            };
            let move_nodes = params.get("move_nodes").and_then(Value::as_bool).unwrap_or(false);
            match (move_nodes, rect) {
                (true, Some(rect)) => {
                    let names = edit(world, "move comment", None, move |d| {
                        d.set_comment_moving_nodes(index, text.as_deref(), rect)
                    })
                    .map_err(refused)?;
                    moved = Some(names);
                }
                (true, None) => {
                    return Err(RpcError::invalid_params(
                        "`move_nodes` needs `rect` (or `min` and `max`)",
                    ));
                }
                (false, rect) => {
                    edit(
                        world,
                        "edit comment",
                        Some(format!("comment:{index}")),
                        move |d| d.set_comment(index, text.as_deref(), rect),
                    )
                    .map_err(refused)?;
                }
            }
        }
        "comment_remove" => {
            let index = index.ok_or_else(|| RpcError::invalid_params("comment_remove needs `index`"))?;
            edit(world, "remove comment", None, move |d| d.remove_comment(index)).map_err(refused)?;
        }
        "reroute_set" => {
            let to = required(params, "to")?.to_string();
            let slot = required(params, "slot")?.to_string();
            let list = params
                .get("points")
                .and_then(Value::as_array)
                .ok_or_else(|| RpcError::invalid_params("`points` (array of [x, y]) is required"))?;
            let points = list
                .iter()
                .map(|v| point(Some(v), "points").map(Option::unwrap_or_default))
                .collect::<Result<Vec<_>, _>>()?;
            edit(world, "reroute", None, move |d| d.set_reroute(&to, &slot, points)).map_err(refused)?;
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown annotate op `{other}` (comment_add, comment_set, comment_remove, reroute_set)"
            )));
        }
    }
    let editor = world.resource::<MatGraphEditor>();
    let mut out = json!({
        "op": op,
        "comments": editor.doc.notes.comments.len(),
        "reroutes": editor.doc.notes.reroutes.len(),
        "revision": editor.revision,
    });
    if let Some(index) = new_index {
        out["index"] = json!(index);
    }
    if let Some(moved) = moved {
        out["moved"] = json!(moved);
    }
    Ok(out)
}

/// `editor.matgraph_node_preview`.
fn node_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = required(params, "node")?.to_string();
    let size = params
        .get("size")
        .and_then(Value::as_f64)
        .map_or(96, |v| v.clamp(64.0, 512.0) as u32);
    let (graph, dir, root) = {
        let editor = open_editor(world)?;
        if matches!(editor.doc.mode, DocMode::Function(_)) {
            return Err(code_error(
                "matgraph_preview_failed",
                "a material function's nodes preview in a graph that calls it".into(),
                "open the calling graph's tab (editor.matgraph_tabs)",
            ));
        }
        if !editor.doc.graph.nodes.contains_key(&name) {
            return Err(node_missing(&name));
        }
        // Function tabs' definitions first (w11): the swatch follows an edit in a function tab.
        let source = editor.preview_source().map_err(|e| {
            code_error(
                "matgraph_preview_failed",
                format!("node `{name}` cannot preview: {e}"),
                "fix the errors editor.matgraph_state lists",
            )
        })?;
        (source.graph, source.dir, editor.root.clone())
    };
    let device = crate::agent_ext::viewport_device(world)?;
    let swatch = super::node_preview::render_node_preview(
        &device,
        &graph,
        &name,
        dir.as_deref(),
        root.as_deref(),
        size,
    )
    .map_err(|e| {
        code_error(
            "matgraph_preview_failed",
            format!("node `{name}` cannot preview: {e}"),
            "fix the errors editor.matgraph_state lists on that node",
        )
    })?;
    let png = crate::render::viewport_png_base64(&swatch.image).map_err(RpcError::internal)?;
    Ok(json!({
        "node": name,
        "size": swatch.image.size[0],
        "png": png,
        "mean_rgb": swatch.mean_rgb,
        "gray_levels": super::node_preview::gray_levels(&swatch.image),
        "key": format!("{:016x}", swatch.key),
    }))
}

/// `editor.matgraph_apply_to_selection`.
fn apply_to_selection(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let report = super::apply_to_selection(world).map_err(|e| {
        RpcError::new(
            INVALID_PARAMS,
            "matgraph_apply_refused",
            e,
            "save the graph under assets/, select meshes (editor.select), then apply",
        )
    })?;
    Ok(json!({ "path": report.path, "owners": report.owners }))
}

/// `editor.material_preview`.
fn material_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let rel = required(params, "path")?;
    if !rel.ends_with(".material.ron") {
        return Err(RpcError::invalid_params(
            "`path` must name a `.material.ron` file",
        ));
    }
    let resolved = super::resolve_path(world, rel);
    let path = crate::agent_ext::allowed_request_path(world, "path", &resolved.to_string_lossy())?;
    let bytes = std::fs::read(&path)
        .map_err(|e| RpcError::not_found(format!("cannot read {}: {e}", path.display()), "check the path"))?;
    let material = regia_asset::MaterialAssetRon::from_ron(&bytes)
        .map_err(|e| RpcError::invalid_params(format!("{} is not a material: {e}", path.display())))?;
    let (geometry, view) = preview_shape_params(world, params)?;
    let size = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .map_or(256.0, |v| v.clamp(64.0, 1024.0) as f32)
    };
    // The Material editor's preview scene (w13, P-083), the turntable at rest.
    let scene = world
        .get_resource_or_init::<crate::preview_scene::PreviewScenes>()
        .context("material");
    let device = crate::agent_ext::viewport_device(world)?;
    let image = crate::material_editor::render_preview_scene(
        &device,
        &crate::material_editor::preview_material(&material),
        &geometry,
        &view,
        [size("width"), size("height")],
        0,
        &scene,
    )
    .ok_or_else(|| {
        code_error(
            "material_preview_failed",
            "the material preview render failed (see the log)".into(),
            "retry; the log names the render error",
        )
    })?;
    let stats = super::preview::measure(&image);
    let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
    Ok(json!({
        "path": path.display().to_string(),
        "shape": str_param(params, "shape").unwrap_or("sphere"),
        "width": image.size[0],
        "height": image.size[1],
        "png": png,
        "mean_rgb": stats.mean_rgb,
        "center_rgb": stats.center_rgb,
        "coverage": stats.coverage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> MethodSchema {
        methods()
            .into_iter()
            .find(|(s, _)| s.name == name)
            .unwrap_or_else(|| panic!("no method {name}"))
            .0
    }

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(s, _)| s.name == method)
            .unwrap_or_else(|| panic!("no method {method}"));
        handler(world, &params)
    }

    fn open_graph() -> World {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        let agent_methods = crate::matgraph_editor::agent::methods();
        let (_, open) = agent_methods
            .iter()
            .find(|(s, _)| s.name == "editor.matgraph_open")
            .expect("matgraph_open");
        open(&mut world, &json!({ "create": true })).expect("open");
        let (_, edit) = agent_methods
            .iter()
            .find(|(s, _)| s.name == "editor.matgraph_edit")
            .expect("matgraph_edit");
        edit(
            &mut world,
            &json!({ "op": "params", "params": [{ "name": "tint", "value": { "Color": [0.8, 0.2, 0.1] } }] }),
        )
        .expect("params");
        edit(
            &mut world,
            &json!({ "op": "add", "name": "tint", "node": "Param(\"tint\")" }),
        )
        .expect("param node");
        world
    }

    /// `editor.matgraph_find` (w8-editor-smalls-z): the read-only twin of
    /// `editor.matgraph_search` — same listing for the same query, no `add`, and an `add` on it
    /// is refused so a read-only agent cannot mutate through it.
    #[test]
    fn matgraph_find_answers_the_search_listing_without_adding() {
        let find = schema("editor.matgraph_find");
        assert!(!find.mutating, "find is read-only");
        assert_eq!(find.permission, Permission::Read);
        let search = schema("editor.matgraph_search");
        assert!(search.mutating, "search still mutates");
        assert_eq!(search.permission, Permission::EditScene);
        assert!(
            find.params.iter().all(|p| !matches!(p.name, "add" | "pos")),
            "find has no add or pos parameter: {:?}",
            find.params.iter().map(|p| p.name).collect::<Vec<_>>()
        );

        let mut world = open_graph();
        for query in [
            json!({}),
            json!({ "query": "para" }),
            json!({ "query": "swiz" }),
            json!({ "from": "tint" }),
            json!({ "output": "out" }),
            json!({ "query": "", "from": "tint" }),
        ] {
            let listing = call(&mut world, "editor.matgraph_find", query.clone()).expect("find");
            let searched = call(&mut world, "editor.matgraph_search", query.clone()).expect("search");
            assert_eq!(listing, searched, "the same listing for {query}");
            assert!(
                listing["kinds"].as_array().is_some_and(|k| !k.is_empty()),
                "the fixture's search has results: {listing}"
            );
        }

        // `add` belongs to the mutating method alone.
        let err = call(
            &mut world,
            "editor.matgraph_find",
            json!({ "query": "mul", "add": "Mul", "pos": [0.0, 0.0] }),
        )
        .expect_err("find does not add");
        assert!(
            err.message.contains("matgraph_search"),
            "the error points at the mutating twin: {err:?}"
        );
        let added = call(
            &mut world,
            "editor.matgraph_search",
            json!({ "query": "mul", "add": "Mul", "pos": [0.0, 0.0] }),
        )
        .expect("search adds");
        assert_eq!(added["added"], json!("mul_1"), "{added}");
    }
}
