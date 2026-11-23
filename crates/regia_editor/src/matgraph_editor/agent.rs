// SPDX-License-Identifier: MPL-2.0
//! The material graph editor's agent-protocol surface: `editor.matgraph_*` methods that open,
//! inspect, edit, compile, preview and save the graph the panel shows
//! (`book/agents/material-graph-editor.md`).
//!
//! Every mutating method goes through [`super::edit`], so it lands on the editor's undo stack
//! exactly like a panel edit (`editor.undo` reverts it). Nodes travel as RON text
//! (`"Mul(\"a\", \"b\")"`) or as their JSON form (`{"Mul": ["a", "b"]}`, `"TexCoord"`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_matgraph::{Node, ParamDecl, Shading, SwitchDecl, TextureDecl};

use super::{DocMode, MatGraphDoc, MatGraphEditor, edit, project_root, resolve_path};

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

/// Every material-graph-editor method with its handler.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    let mut out = vec![
        (
            m(
                "editor.matgraph_open",
                "matgraph_open",
                "Open a .matgraph.ron in the material graph editor (with its layout sidecar), or start a new graph (create: true).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            false,
                            "Project-relative (assets/…) or absolute .matgraph.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start an empty graph (saved to `path` later).",
                        ),
                    ]
                },
                "{ path, nodes, ok, revision }",
                true,
            ),
            matgraph_open as Handler,
        ),
        (
            m(
                "editor.matgraph_state",
                "matgraph_state",
                "The open graph: nodes (name, kind, pos, input slots and what drives them, errors), surface outputs, declarations, compile status with errors mapped to nodes, permutation count.",
                &const {
                    [p(
                        "ron",
                        "boolean",
                        false,
                        "Include the .matgraph.ron text it would save.",
                    )]
                },
                "{ path, dirty, revision, ok, errors, warnings, key, permutations, nodes, outputs, params, textures, switches, functions, shading, ron? }",
                false,
            ),
            matgraph_state as Handler,
        ),
        (
            m(
                "editor.matgraph_edit",
                "matgraph_edit",
                "One undoable edit of the open graph. op: add {name?, node | kind, pos?}, set {name, node}, remove {name}, rename {name, to}, connect {from, to, slot}, disconnect {to, slot}, disconnect_all {name}, output {slot, from|null}, move {name, pos}, params {params}, textures {textures}, switches {switches}, shading {shading}.",
                &const {
                    [
                        p(
                            "op",
                            "string",
                            true,
                            "add | set | remove | rename | connect | disconnect | disconnect_all | output | move | params | textures | switches | shading.",
                        ),
                        p(
                            "name",
                            "string",
                            false,
                            "Node name (add: optional, a fresh name is picked).",
                        ),
                        p("node", "any", false, "A node as RON text or JSON (add, set)."),
                        p(
                            "kind",
                            "string",
                            false,
                            "add: a node kind with inputs unwired (Lerp, Sample, …).",
                        ),
                        p(
                            "to",
                            "string",
                            false,
                            "rename: new name; connect/disconnect: the node whose input changes.",
                        ),
                        p(
                            "from",
                            "string",
                            false,
                            "connect/output: the node that drives (output: null clears).",
                        ),
                        p(
                            "slot",
                            "string",
                            false,
                            "connect/disconnect: input slot (a, b, t, uv, …); output: surface slot.",
                        ),
                        p("pos", "array", false, "[x, y] canvas position (add, move)."),
                        p("params", "array", false, "params: the whole parameter list."),
                        p("textures", "array", false, "textures: the whole texture list."),
                        p("switches", "array", false, "switches: the whole switch list."),
                        p("shading", "object", false, "shading: the whole shading block."),
                    ]
                },
                "{ op, revision, ok, errors, name?, readers? }",
                true,
            ),
            matgraph_edit as Handler,
        ),
        (
            m(
                "editor.matgraph_compile",
                "matgraph_compile",
                "Compile the open graph with full validation (composed into the forward shader): pipeline key, parameter and texture slots, errors mapped to nodes, and the shipped permutation policy's per-profile pipeline count and cold compile estimate for this graph.",
                &const {
                    [
                        p("include_wgsl", "boolean", false, "Include the generated WGSL."),
                        p(
                            "switches",
                            "object",
                            false,
                            "Static switch overrides (name → bool) for one permutation.",
                        ),
                    ]
                },
                "{ ok, key, graph_hash, params, textures, uses_time, uses_uv1, vertex_offset, permutations, policy, errors, wgsl? }",
                false,
            ),
            matgraph_compile as Handler,
        ),
        (
            m(
                "editor.matgraph_preview",
                "matgraph_preview",
                "Render the live preview: the open graph's material on the preview shape (sphere, cube, plane or a project glTF mesh), from an orbit with a turned light.",
                &const {
                    [
                        p("width", "number", false, "Pixels, 64–1024 (default 256)."),
                        p("height", "number", false, "Pixels, 64–1024 (default 256)."),
                        p(
                            "shape",
                            "string",
                            false,
                            "sphere (default) | cube | plane | mesh.",
                        ),
                        p(
                            "mesh",
                            "string",
                            false,
                            "shape mesh: a project .glb/.gltf (its first part, at most 250000 vertices).",
                        ),
                        p(
                            "yaw",
                            "number",
                            false,
                            "Orbit around the vertical, radians (default 0).",
                        ),
                        p("pitch", "number", false, "Orbit up or down, radians (default 0)."),
                        p(
                            "light_yaw",
                            "number",
                            false,
                            "Turn the light around the vertical, radians (default 0).",
                        ),
                        p(
                            "profile",
                            "string",
                            false,
                            "Draw with this render profile's frame quality (the tier preview; a profile editor.matgraph_cost lists). Default: the editor's own.",
                        ),
                        p(
                            "node",
                            "string",
                            false,
                            "Draw this node's value instead of the material (the node menu's Preview this node).",
                        ),
                    ]
                },
                "{ width, height, png, mean_rgb, center_rgb, coverage, profile }",
                false,
            ),
            matgraph_preview as Handler,
        ),
        (
            m(
                "editor.matgraph_save",
                "matgraph_save",
                "Save the open graph and its layout sidecar (atomic writes). A graph with errors is saved too.",
                &const {
                    [p(
                        "path",
                        "string",
                        false,
                        "Save-as path (default: the open path).",
                    )]
                },
                "{ path, layout, ok }",
                true,
            ),
            matgraph_save as Handler,
        ),
    ];
    out.extend(function_methods());
    out.extend(super::agent_s2::methods());
    out.extend(super::agent_w12::methods());
    out.extend(super::code_editor::methods());
    out
}

/// The function, instance and tab methods (ADR-2490).
fn function_methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            m(
                "editor.matgraph_function_open",
                "matgraph_function_open",
                "Open a material function (.matfunc.ron) in its own canvas tab (the open document stays in its tab), or start one (create: true). Errors map to the function's nodes; Param, texture and switch names are checked against the calling graph (context, or the graph tab it is opened from).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            true,
                            "Project-relative (assets/…) or absolute .matfunc.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start a new function (input `x`, output `out`), saved to `path` later.",
                        ),
                        p(
                            "context",
                            "string",
                            false,
                            "A .matgraph.ron that calls it, whose declarations it reads (default: the open graph).",
                        ),
                    ]
                },
                "{ path, tab, nodes, ok, inputs, outputs, context, revision }",
                true,
            ),
            matgraph_function_open as Handler,
        ),
        (
            m(
                "editor.matgraph_function_signature",
                "matgraph_function_signature",
                "One undoable change of the open function's signature: its inputs (name, ty) in order and its output names. Kept outputs keep their wire; a new input gets an Input node of its name.",
                &const {
                    [
                        p(
                            "inputs",
                            "array",
                            true,
                            "[{ name, ty: Float | Vec2 | Vec3 | Vec4 }] in order.",
                        ),
                        p("outputs", "array", true, "Output names."),
                    ]
                },
                "{ revision, ok, errors, inputs, outputs }",
                true,
            ),
            matgraph_function_signature as Handler,
        ),
        (
            m(
                "editor.matgraph_function_collapse",
                "matgraph_function_collapse",
                "Collapse nodes of the open graph into a new material function (inline, or a new .matfunc.ron with `file`), replaced by one Call node per output named after the node it stands for. One undo step (undo also removes the file it wrote).",
                &const {
                    [
                        p("nodes", "array", true, "Node names to collapse."),
                        p("name", "string", true, "The new function's name."),
                        p(
                            "file",
                            "string",
                            false,
                            "Write the function to this project-relative .matfunc.ron and reference it.",
                        ),
                    ]
                },
                "{ function, inputs, outputs, calls, file, wgsl_identical, revision, ok, errors }",
                true,
            ),
            matgraph_function_collapse as Handler,
        ),
        (
            m(
                "editor.matgraph_function_expand",
                "matgraph_function_expand",
                "Expand a Call node of the open graph inline: the function's nodes take its place, the output node takes its name. One undo step.",
                &const { [p("node", "string", true, "The Call node.")] },
                "{ node, result, nodes, revision, ok, errors }",
                true,
            ),
            matgraph_function_expand as Handler,
        ),
        (
            m(
                "editor.matgraph_function_fix",
                "matgraph_function_fix",
                "Fix stale Call nodes of the open graph (their function's signature changed): renamed inputs keep their wire, gone ones are dropped, a gone output becomes the only (or first) one. One undo step.",
                &const {
                    [p(
                        "nodes",
                        "array",
                        false,
                        "Only these Call nodes (default: every stale one).",
                    )]
                },
                "{ fixed: [{ node, rewired, dropped, unwired, output }], stale, revision, ok, errors }",
                true,
            ),
            matgraph_function_fix as Handler,
        ),
        (
            m(
                "editor.matgraph_function_users",
                "matgraph_function_users",
                "The project's graphs that use a function file, with their Call nodes and the ones whose signature is stale (checked against the open function tab, or the file).",
                &const {
                    [p(
                        "path",
                        "string",
                        false,
                        "The .matfunc.ron (default: the open function).",
                    )]
                },
                "{ path, signature, users: [{ path, function, calls, stale }] }",
                false,
            ),
            matgraph_function_users as Handler,
        ),
        (
            m(
                "editor.matgraph_instance_open",
                "matgraph_instance_open",
                "Open a material instance (.matinst.ron) in its own tab on its parent's graph, read-only, with its overrides; or start one (create: true, parent).",
                &const {
                    [
                        p(
                            "path",
                            "string",
                            true,
                            "Project-relative (assets/…) or absolute .matinst.ron path.",
                        ),
                        p(
                            "create",
                            "boolean",
                            false,
                            "Start a new instance with no overrides, saved to `path` later.",
                        ),
                        p(
                            "parent",
                            "string",
                            false,
                            "create: the parent .matgraph.ron, project-relative.",
                        ),
                    ]
                },
                "{ path, tab, parent, overrides, ok, revision }",
                true,
            ),
            matgraph_instance_open as Handler,
        ),
        (
            m(
                "editor.matgraph_instance_set",
                "matgraph_instance_set",
                "One undoable override of the open instance: a parameter value, a texture path or a static switch, or clear it. Only the instance changes; editor.matgraph_save writes the .matinst.ron alone.",
                &const {
                    [
                        p("param", "string", false, "Parameter to override."),
                        p("texture", "string", false, "Texture to override."),
                        p("switch", "string", false, "Static switch to override."),
                        p(
                            "value",
                            "any",
                            false,
                            "param: { Float: 0.5 } | { Color: [r, g, b] } | RON; texture: an image path; switch: true | false.",
                        ),
                        p("clear", "boolean", false, "Remove the override instead."),
                    ]
                },
                "{ revision, ok, errors, overrides }",
                true,
            ),
            matgraph_instance_set as Handler,
        ),
        (
            m(
                "editor.matgraph_tabs",
                "matgraph_tabs",
                "The material graph editor's open documents (graphs, functions, instances) in tab order; select one, or close the active one.",
                &const {
                    [
                        p("select", "number", false, "Make this tab active."),
                        p(
                            "close",
                            "boolean",
                            false,
                            "Close the active tab (unsaved edits are dropped).",
                        ),
                    ]
                },
                "{ tabs: [{ kind, path, label, dirty, active }], active }",
                true,
            ),
            matgraph_tabs as Handler,
        ),
    ]
}

pub(super) fn str_param<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str)
}

pub(super) fn required<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    str_param(params, name).ok_or_else(|| RpcError::invalid_params(format!("`{name}` (string) is required")))
}

pub(super) fn refused(message: String) -> RpcError {
    if let Some(rest) = message.strip_prefix(super::model::READ_ONLY) {
        return RpcError::new(
            INVALID_PARAMS,
            "matgraph_read_only",
            rest.to_string(),
            "an instance edits its overrides only (editor.matgraph_instance_set); open the parent graph to change the graph",
        );
    }
    RpcError::new(
        INVALID_PARAMS,
        "matgraph_edit_refused",
        message,
        "editor.matgraph_state lists the nodes, their slots and the errors",
    )
}

fn no_graph() -> RpcError {
    RpcError::not_found(
        "no material graph is open in the editor",
        "call editor.matgraph_open { path } or { create: true } first",
    )
}

pub(super) fn open_editor(world: &mut World) -> Result<Mut<'_, MatGraphEditor>, RpcError> {
    if !world
        .get_resource::<MatGraphEditor>()
        .is_some_and(MatGraphEditor::is_open)
    {
        return Err(no_graph());
    }
    Ok(world.resource_mut::<MatGraphEditor>())
}

/// A node from RON text or its JSON form.
fn parse_node(value: &Value) -> Result<Node, RpcError> {
    match value {
        Value::String(text) => match ron::from_str::<Node>(text) {
            Ok(node) => Ok(node),
            // A bare JSON unit variant ("TexCoord") is also valid RON; anything else failed.
            Err(err) => Err(RpcError::invalid_params(format!("`node` is not a node: {err}"))),
        },
        other => serde_json::from_value::<Node>(other.clone())
            .map_err(|err| RpcError::invalid_params(format!("`node` is not a node: {err}"))),
    }
}

fn parse_block<T: serde::de::DeserializeOwned>(params: &Value, name: &str) -> Result<T, RpcError> {
    let value = params
        .get(name)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is required")))?;
    let parsed = match value {
        Value::String(text) => ron::from_str::<T>(text).map_err(|e| e.to_string()),
        other => serde_json::from_value::<T>(other.clone()).map_err(|e| e.to_string()),
    };
    parsed.map_err(|err| RpcError::invalid_params(format!("`{name}`: {err}")))
}

pub(super) fn pos_param(params: &Value) -> Result<Option<[f32; 2]>, RpcError> {
    let Some(value) = params.get("pos") else {
        return Ok(None);
    };
    let Some(array) = value.as_array() else {
        return Err(RpcError::invalid_params("`pos` must be [x, y], finite numbers"));
    };
    if array.len() != 2 {
        return Err(RpcError::invalid_params("`pos` must be [x, y], finite numbers"));
    }
    let x = array[0].as_f64().map(|v| v as f32);
    let y = array[1].as_f64().map(|v| v as f32);
    match (x, y) {
        (Some(x), Some(y)) if x.is_finite() && y.is_finite() => Ok(Some([x, y])),
        _ => Err(RpcError::invalid_params("`pos` must be [x, y], finite numbers")),
    }
}

pub(super) fn errors_json(editor: &mut MatGraphEditor) -> (bool, Value, Value) {
    let d = editor.diagnostics();
    let errors: Vec<Value> = d
        .errors
        .iter()
        .map(|e| json!({ "target": e.target.label(), "source": e.source, "message": e.message }))
        .collect();
    (d.ok, Value::Array(errors), json!(d.warnings))
}

/// A request `path` resolved as the panel resolves it (`assets/…` under the open project, else as
/// given) and confined by the editor's one document-path rule (ADR-7388): `..`, symlinks out and
/// paths outside the project and the temp dir are `path_outside_project`.
fn request_path(world: &World, path: &str) -> Result<std::path::PathBuf, RpcError> {
    request_path_ext(world, path, ".matgraph.ron")
}

/// [`request_path`] for a file ending in `ext` (`.matgraph.ron`, `.matfunc.ron`, `.matinst.ron`).
pub(super) fn request_path_ext(world: &World, path: &str, ext: &str) -> Result<std::path::PathBuf, RpcError> {
    if !path.ends_with(ext) {
        return Err(RpcError::invalid_params(format!(
            "`path` must name a `{ext}` file"
        )));
    }
    let resolved = resolve_path(world, path);
    crate::agent_ext::document_request_path(world, "path", &resolved.to_string_lossy())
}

/// `editor.matgraph_open`.
fn matgraph_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path = str_param(params, "path")
        .map(|p| request_path(world, p))
        .transpose()?;
    let root = project_root(world);
    let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
    editor.root = root;
    let unsaved = editor.has_unsaved_work();
    if create {
        editor.new_graph(path);
    } else {
        let path =
            path.ok_or_else(|| RpcError::invalid_params("`path` is required unless `create: true`"))?;
        if !path.is_file() {
            return Err(RpcError::not_found(
                format!("no file {}", path.display()),
                "check the path (project-relative assets/… or absolute)",
            ));
        }
        editor.open(&path).map_err(refused)?;
    }
    let nodes = editor.doc.graph.nodes.len();
    let ok = editor.diagnostics().ok;
    Ok(json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "nodes": nodes,
        "ok": ok,
        "revision": editor.revision,
        "discarded_unsaved": unsaved,
    }))
}

fn node_json(
    doc: &MatGraphDoc,
    positions: &BTreeMap<String, [f32; 2]>,
    diag: &super::Diagnostics,
    name: &str,
    node: &Node,
) -> Value {
    let mut inputs: Vec<Value> = node
        .input_slots()
        .into_iter()
        .map(|(slot, from)| json!({ "slot": slot, "from": from }))
        .collect();
    let mut extra = serde_json::Map::new();
    if let Node::Call { function, args, .. } = node
        && let Some(sig) = diag.signatures.get(function)
    {
        // The pins follow the signature; arguments it no longer has are listed as stale.
        inputs = sig
            .inputs
            .iter()
            .map(|i| json!({ "slot": i.name, "from": args.get(&i.name).filter(|a| !a.is_empty()), "type": format!("{:?}", i.ty) }))
            .chain(
                args.iter()
                    .filter(|(k, _)| !sig.inputs.iter().any(|i| &i.name == *k))
                    .map(|(k, v)| json!({ "slot": k, "from": v, "stale": true })),
            )
            .collect();
        extra.insert("outputs".into(), json!(sig.outputs));
        if let Some(stale) = diag.stale.iter().find(|st| st.node == name) {
            extra.insert("stale".into(), json!(stale.message()));
        }
    }
    if let (Node::Param(param), DocMode::Instance(instance)) = (node, &doc.mode) {
        extra.insert("override".into(), json!(instance.params.get(param)));
        extra.insert(
            "default".into(),
            json!(
                doc.graph
                    .params
                    .iter()
                    .find(|p| &p.name == param)
                    .map(|p| p.value)
            ),
        );
    }
    let target = super::Target::Node(name.to_string());
    let errors: Vec<&str> = diag.for_target(&target).collect();
    let drives: Vec<String> = doc
        .sink_slots()
        .into_iter()
        .filter(|slot| doc.sink_get(slot) == Some(name))
        .collect();
    let mut out = json!({
        "name": name,
        "kind": node.kind(),
        "pos": positions.get(name),
        "inputs": inputs,
        "texture": node.texture(),
        "drives": drives,
        "errors": errors,
        "ron": ron::to_string(node).ok(),
    });
    if let Some(map) = out.as_object_mut() {
        map.extend(extra);
    }
    out
}

/// `editor.matgraph_state`.
fn matgraph_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let undo = world
        .get_resource::<crate::command::UndoStack>()
        .and_then(crate::command::UndoStack::next_undo);
    let mut editor = open_editor(world)?;
    let editor = &mut *editor;
    let (ok, errors, warnings) = errors_json(editor);
    let diag = editor.diagnostics().clone();
    let doc = &editor.doc;
    let positions = doc.positions();
    let nodes: Vec<Value> = doc
        .graph
        .nodes
        .iter()
        .map(|(name, node)| node_json(doc, &positions, &diag, name, node))
        .collect();
    let outputs: serde_json::Map<String, Value> = doc
        .sink_slots()
        .into_iter()
        .map(|slot| {
            let from = doc.sink_get(&slot).map(str::to_string);
            (slot, json!(from))
        })
        .collect();
    let stale: Vec<Value> = diag
        .stale
        .iter()
        .map(|st| json!({ "node": st.node, "function": st.function, "unknown_args": st.unknown_args, "missing_args": st.missing_args, "bad_output": st.bad_output, "message": st.message() }))
        .collect();
    let mut out = json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "name": doc.graph.name,
        "dirty": editor.dirty(),
        "revision": editor.revision,
        "ok": ok,
        "errors": errors,
        "warnings": warnings,
        "key": diag.key.map(|k| format!("{k:016x}")),
        "graph_hash": diag.graph_hash.map(|k| format!("{k:016x}")),
        "permutations": doc.permutations(),
        "nodes": nodes,
        "outputs": outputs,
        "normal_space": doc.graph.output.normal_space,
        "params": doc.graph.params,
        "textures": doc.graph.textures,
        "switches": doc.graph.switches,
        "functions": doc.graph.functions.keys().collect::<Vec<_>>(),
        "shading": doc.graph.shading,
        "next_undo": undo,
        "kind": super::doc_kind(doc),
        "stale": stale,
        "function": doc.function_meta().map(|f| json!({ "name": f.name, "inputs": f.inputs, "outputs": f.outputs })),
        "context": editor.context.as_ref().map(|c| c.path.as_ref().map(|p| p.display().to_string())),
        "instance": doc.instance_meta().map(|i| instance_json(i, editor.parent_path.as_deref())),
        "tabs": editor.tabs().len(),
    });
    if params.get("ron").and_then(Value::as_bool).unwrap_or(false) {
        let text = match &doc.mode {
            DocMode::Graph => doc.graph.to_ron(),
            DocMode::Function(meta) => meta.to_function(&doc.graph.nodes).to_ron(),
            DocMode::Instance(instance) => instance.to_ron(),
        };
        out["ron"] = json!(text.map_err(|e| RpcError::internal(e.to_string()))?);
    }
    Ok(out)
}

/// `editor.matgraph_edit`.
fn matgraph_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_editor(world)?;
    let op = required(params, "op")?.to_string();
    let mut extra = json!({});
    let result: Result<(), String> = match op.as_str() {
        "add" => {
            let node = match (params.get("node"), str_param(params, "kind")) {
                (Some(node), _) => parse_node(node)?,
                (None, Some(kind)) => Node::default_of(kind).ok_or_else(|| {
                    RpcError::invalid_params(format!("unknown kind `{kind}` ({})", Node::KINDS.join(", ")))
                })?,
                (None, None) => return Err(RpcError::invalid_params("add needs `node` or `kind`")),
            };
            let name = str_param(params, "name").map(str::to_string);
            let pos = pos_param(params)?;
            edit(world, "add node", None, move |d| {
                d.add_node(name.as_deref(), node, pos)
            })
            .map(|name| {
                extra["name"] = json!(name);
            })
        }
        "set" => {
            let name = required(params, "name")?.to_string();
            let node = parse_node(
                params
                    .get("node")
                    .ok_or_else(|| RpcError::invalid_params("`node` is required"))?,
            )?;
            edit(world, "set node", None, move |d| d.set_node(&name, node))
        }
        "remove" => {
            let name = required(params, "name")?.to_string();
            edit(world, "remove node", None, move |d| d.remove_node(&name)).map(|readers| {
                extra["readers"] = json!(readers);
            })
        }
        "rename" => {
            let name = required(params, "name")?.to_string();
            let to = required(params, "to")?.to_string();
            edit(world, "rename node", None, move |d| d.rename_node(&name, &to))
        }
        "connect" => {
            let from = required(params, "from")?.to_string();
            let to = required(params, "to")?.to_string();
            let slot = required(params, "slot")?.to_string();
            edit(world, "connect", None, move |d| d.connect(&from, &to, &slot))
        }
        "disconnect" => {
            let to = required(params, "to")?.to_string();
            let slot = required(params, "slot")?.to_string();
            edit(world, "disconnect", None, move |d| d.disconnect(&to, &slot))
        }
        // w12 (ADR-7851): the node menu's Disconnect all.
        "disconnect_all" => {
            let name = required(params, "name")?.to_string();
            edit(world, "disconnect all", None, move |d| {
                d.disconnect_all(&name).map(|_| ())
            })
        }
        "output" => {
            let slot = required(params, "slot")?.to_string();
            // `from: null` clears the slot; a missing `from` is a mistake, not a clear.
            let from = match params.get("from") {
                Some(Value::Null) => None,
                Some(Value::String(s)) => Some(s.clone()),
                _ => {
                    return Err(RpcError::invalid_params(
                        "`from` is required: a node name, or null to clear the slot",
                    ));
                }
            };
            edit(world, "set output", None, move |d| {
                d.set_output(&slot, from.as_deref())
            })
        }
        "move" => {
            let name = required(params, "name")?.to_string();
            let pos = pos_param(params)?.ok_or_else(|| RpcError::invalid_params("`pos` is required"))?;
            let key = format!("move:{name}");
            let moved = edit(world, "move node", Some(key), move |d| d.move_node(&name, pos));
            // Each agent move is its own undo step (the canvas coalesces one drag).
            world
                .get_resource_or_insert_with(crate::command::UndoStack::new)
                .seal();
            moved
        }
        "params" => {
            let list: Vec<ParamDecl> = parse_block(params, "params")?;
            edit(world, "parameters", None, move |d| {
                d.graph.params = list;
                Ok(())
            })
        }
        "textures" => {
            let list: Vec<TextureDecl> = parse_block(params, "textures")?;
            edit(world, "textures", None, move |d| {
                d.graph.textures = list;
                Ok(())
            })
        }
        "switches" => {
            let list: Vec<SwitchDecl> = parse_block(params, "switches")?;
            edit(world, "switches", None, move |d| {
                d.graph.switches = list;
                Ok(())
            })
        }
        "shading" => {
            let shading: Shading = parse_block(params, "shading")?;
            edit(world, "shading", None, move |d| {
                d.graph.shading = shading;
                Ok(())
            })
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown op `{other}` (add, set, remove, rename, connect, disconnect, output, move, params, textures, switches, shading)"
            )));
        }
    };
    result.map_err(refused)?;
    let mut editor = world.resource_mut::<MatGraphEditor>();
    let (ok, errors, _) = errors_json(&mut editor);
    let mut out = json!({ "op": op, "revision": editor.revision, "ok": ok, "errors": errors });
    if let (Some(out), Value::Object(extra)) = (out.as_object_mut(), extra) {
        out.extend(extra);
    }
    Ok(out)
}

/// `editor.matgraph_compile`.
fn matgraph_compile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let switches: BTreeMap<String, bool> = match params.get("switches") {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| RpcError::invalid_params(format!("`switches` must map names to booleans: {e}")))?,
        None => BTreeMap::new(),
    };
    let include_wgsl = params
        .get("include_wgsl")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let editor = open_editor(world)?;
    let permutations = editor.doc.permutations();
    // A function compiles in its harness (the calling graph's declarations), an instance as its
    // parent with the overrides (`model::instance_view`).
    let harness_name = editor.doc.function_meta().map(|m| {
        if super::model::valid_node_name(&m.name) {
            m.name.clone()
        } else {
            "function".to_string()
        }
    });
    // Function tabs' definitions first: a graph calling an open function file compiles the tab
    // (w11, ADR-7730).
    let source = editor.compile_source();
    let graph = match source {
        Ok(g) => g,
        Err(message) => {
            return Ok(json!({
                "ok": false,
                "permutations": permutations,
                "errors": [{ "target": "graph", "source": null, "message": message }],
            }));
        }
    };
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_compile").entered();
    match regia_matgraph::compile_with(&graph, &switches) {
        Ok(c) => {
            let params: Vec<Value> = c
                .params
                .iter()
                .map(|p| json!({ "name": p.name, "slot": p.slot, "type": format!("{:?}", p.ty) }))
                .collect();
            let textures: Vec<Value> = c
                .textures
                .iter()
                .map(|t| json!({ "name": t.name, "slot": t.slot, "color_space": format!("{:?}", t.color_space) }))
                .collect();
            let mut out = json!({
                "ok": true,
                "key": format!("{:016x}", c.key),
                "graph_hash": format!("{:016x}", c.graph_hash),
                "switches": c.switches,
                "params": params,
                "textures": textures,
                "uses_time": c.uses_time,
                "uses_uv1": c.uses_uv1,
                "vertex_offset": c.vertex_wgsl.is_some(),
                "permutations": permutations,
                "policy": policy_json(&graph),
                "errors": [],
                "kind": super::doc_kind(&editor.doc),
            });
            if include_wgsl {
                out["wgsl"] = json!(&*c.wgsl);
                out["vertex_wgsl"] = json!(c.vertex_wgsl.as_deref());
            }
            Ok(out)
        }
        Err(errors) => {
            let errors: Vec<Value> = errors
                .0
                .iter()
                .map(|e| {
                    let (target, source) = match &harness_name {
                        Some(name) => super::function::map_harness_error(e.node.as_deref(), name),
                        None => (super::Target::of(e.node.as_deref()), e.node.clone()),
                    };
                    json!({
                        "target": target.label(),
                        "source": source,
                        "message": e.message,
                    })
                })
                .collect();
            Ok(json!({ "ok": false, "permutations": permutations, "errors": errors }))
        }
    }
}

/// `editor.matgraph_preview`.
fn matgraph_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let size = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .map_or(256.0, |v| v.clamp(64.0, 1024.0) as f32)
    };
    let (w, h) = (size("width"), size("height"));
    let (geometry, view) = super::agent_s2::preview_shape_params(world, params)?;
    // A function tab previews its calling graph with the tab's definition (w11, ADR-7730).
    let (source, root) = {
        let editor = open_editor(world)?;
        (editor.preview_source(), editor.root.clone())
    };
    let super::PreviewSource { graph, dir } = source.map_err(|e| {
        RpcError::new(
            INVALID_PARAMS,
            "matgraph_preview_failed",
            format!("the preview cannot draw this document: {e}"),
            "fix the errors editor.matgraph_state lists",
        )
    })?;
    // w12 (ADR-7851): the tier preview and a pinned node.
    let profile = str_param(params, "profile")
        .map(|name| {
            super::cost::parse_profile(name).map_err(|e| {
                code_error(
                    "matgraph_profile_unknown",
                    e,
                    "the profiles are the permutation policy's (editor.matgraph_cost lists them)",
                )
            })
        })
        .transpose()?;
    let graph = match str_param(params, "node") {
        Some(node) => super::node_preview::derived_graph(&graph, node).map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                "matgraph_preview_failed",
                format!("node `{node}` cannot be previewed: {e}"),
                "editor.matgraph_state lists the nodes",
            )
        })?,
        None => graph,
    };
    // The editor's preview scene (w13, P-083), the turntable at rest so the answer repeats.
    let scene = world
        .get_resource_or_init::<crate::preview_scene::PreviewScenes>()
        .context("matgraph");
    let device = crate::agent_ext::viewport_device(world)?;
    let image = super::preview::render_graph_preview_scene(
        &device,
        &graph,
        dir.as_deref(),
        root.as_deref(),
        [w, h],
        &geometry,
        &view,
        profile,
        &scene,
    )
    .map_err(|e| {
        RpcError::new(
            INVALID_PARAMS,
            "matgraph_preview_failed",
            format!("the preview cannot draw this graph: {e}"),
            "fix the errors editor.matgraph_state lists",
        )
    })?;
    let stats = super::preview::measure(&image);
    let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
    Ok(json!({
        "width": image.size[0],
        "height": image.size[1],
        "png": png,
        "mean_rgb": stats.mean_rgb,
        "center_rgb": stats.center_rgb,
        "coverage": stats.coverage,
        "profile": profile.map(regia_render::RenderProfilePreset::name),
    }))
}

/// `editor.matgraph_save`.
fn matgraph_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let ext = world
        .get_resource::<MatGraphEditor>()
        .map_or(".matgraph.ron", |e| doc_ext(&e.doc));
    let path = str_param(params, "path")
        .map(|p| request_path_ext(world, p, ext))
        .transpose()?;
    let mut editor = open_editor(world)?;
    let saved = editor.save(path.as_deref()).map_err(|e| {
        RpcError::new(
            regia_agent::protocol::INTERNAL_ERROR,
            "matgraph_save_failed",
            e,
            "check the path and the disk",
        )
    })?;
    let ok = editor.diagnostics().ok;
    let layout = (!matches!(editor.doc.mode, DocMode::Instance(_)))
        .then(|| super::layout_path(&saved).display().to_string());
    Ok(json!({
        "path": saved.display().to_string(),
        "layout": layout,
        "kind": super::doc_kind(&editor.doc),
        "ok": ok,
    }))
}

/// The shipped shader-permutation policy for the open graph alone (ADR-2026,
/// [`super::model::policy_cost`]): per profile, the pipelines and estimated cold compile time the
/// cook would list for it against that profile's default budget. `null` when a permutation does
/// not compile.
fn policy_json(graph: &regia_matgraph::MaterialGraph) -> Value {
    let Ok(cost) = super::model::policy_cost(graph) else {
        return Value::Null;
    };
    let profiles: Vec<Value> = cost
        .profiles
        .iter()
        .map(|p| {
            json!({
                "profile": p.profile,
                "sets": p.entries.len(),
                "pipelines": p.pipelines,
                "modules": p.modules,
                "estimated_compile_ms": p.estimated_compile_ms,
                "budget_pipelines": p.budget.max_pipelines,
                "budget_compile_ms": p.budget.max_compile_ms,
                "over_budget": p.over_budget,
            })
        })
        .collect();
    json!({
        "permutations": cost.permutations,
        "enumerated": cost.enumerated,
        "distinct_codes": cost.distinct_codes,
        "profiles": profiles,
    })
}

/// The file extension the open document saves to.
fn doc_ext(doc: &MatGraphDoc) -> &'static str {
    match doc.mode {
        DocMode::Graph => ".matgraph.ron",
        DocMode::Function(_) => ".matfunc.ron",
        DocMode::Instance(_) => ".matinst.ron",
    }
}

fn instance_json(instance: &regia_matgraph::MaterialInstance, parent: Option<&std::path::Path>) -> Value {
    json!({
        "parent": instance.parent,
        "parent_path": parent.map(|p| p.display().to_string()),
        "params": instance.params,
        "textures": instance.textures,
        "switches": instance.switches,
    })
}

pub(super) fn code_error(code: &'static str, message: String, hint: &'static str) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, hint)
}

/// The answer tail every edit shares: `{ revision, ok, errors }`.
pub(super) fn edit_answer(world: &mut World, mut out: Value) -> Value {
    let mut editor = world.resource_mut::<MatGraphEditor>();
    let (ok, errors, _) = errors_json(&mut editor);
    out["revision"] = json!(editor.revision);
    out["ok"] = json!(ok);
    out["errors"] = errors;
    out
}

pub(super) fn string_list(params: &Value, name: &str) -> Result<Vec<String>, RpcError> {
    params
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` (array of strings) is required")))?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must hold strings")))
        })
        .collect()
}

fn function_open_json(editor: &mut MatGraphEditor) -> Value {
    let ok = editor.diagnostics().ok;
    let meta = editor.doc.function_meta().cloned().unwrap_or_default();
    json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "tab": editor.tabs().iter().position(|t| t.active),
        "nodes": editor.doc.graph.nodes.len(),
        "ok": ok,
        "inputs": meta.inputs,
        "outputs": meta.outputs,
        "context": editor.context.as_ref().map(|c| c.path.as_ref().map(|p| p.display().to_string())),
        "revision": editor.revision,
    })
}

/// `editor.matgraph_function_open`.
fn matgraph_function_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path = request_path_ext(world, required(params, "path")?, ".matfunc.ron")?;
    let context_path = str_param(params, "context")
        .map(|p| request_path(world, p))
        .transpose()?;
    let root = project_root(world);
    if !create && !path.is_file() {
        return Err(RpcError::not_found(
            format!("no file {}", path.display()),
            "check the path, or pass create: true",
        ));
    }
    let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
    editor.root.clone_from(&root);
    let context = match context_path {
        Some(c) => Some(super::context_from_file(&c, root.as_deref()).map_err(refused)?),
        None => super::context_of_open_graph(&editor),
    };
    let previous = editor.tabs().len();
    editor.open_tab();
    let opened = if create {
        editor.new_function(Some(path), context);
        Ok(())
    } else {
        editor.open_function(&path, context)
    };
    if let Err(message) = opened {
        // Back to the document that was open.
        editor.close_tab();
        if previous > 0 {
            let _ = editor.select_tab(previous.saturating_sub(1));
        }
        return Err(refused(message));
    }
    Ok(function_open_json(&mut editor))
}

/// `editor.matgraph_function_signature`.
fn matgraph_function_signature(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_editor(world)?;
    let inputs: Vec<regia_matgraph::FunctionInput> = parse_block(params, "inputs")?;
    let outputs = string_list(params, "outputs")?;
    edit(world, "function signature", None, move |d| {
        d.set_signature(inputs, outputs)
    })
    .map_err(refused)?;
    let meta = world
        .resource::<MatGraphEditor>()
        .doc
        .function_meta()
        .cloned()
        .unwrap_or_default();
    Ok(edit_answer(
        world,
        json!({ "inputs": meta.inputs, "outputs": meta.outputs }),
    ))
}

/// `editor.matgraph_function_collapse`.
fn matgraph_function_collapse(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_editor(world)?;
    let nodes: std::collections::BTreeSet<String> = string_list(params, "nodes")?.into_iter().collect();
    let name = required(params, "name")?.to_string();
    let file = str_param(params, "file").map(str::to_string);
    let report = super::collapse_selection(world, &nodes, &name, file.as_deref()).map_err(|e| {
        code_error(
            "matgraph_collapse_invalid",
            e,
            "select nodes of the open graph that something outside the selection reads",
        )
    })?;
    Ok(edit_answer(
        world,
        json!({
            "function": report.function,
            "inputs": report.signature.inputs,
            "outputs": report.signature.outputs,
            "calls": report.calls,
            "file": report.file.map(|p| p.display().to_string()),
            "wgsl_identical": report.wgsl_identical,
        }),
    ))
}

/// `editor.matgraph_function_expand`.
fn matgraph_function_expand(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_editor(world)?;
    let node = required(params, "node")?.to_string();
    let out = super::expand_call(world, &node)
        .map_err(|e| code_error("matgraph_expand_invalid", e, "name a Call node of the open graph"))?;
    Ok(edit_answer(
        world,
        json!({ "node": node, "result": out.result, "nodes": out.nodes }),
    ))
}

/// `editor.matgraph_function_fix`.
fn matgraph_function_fix(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    open_editor(world)?;
    let nodes = match params.get("nodes") {
        Some(_) => Some(string_list(params, "nodes")?),
        None => None,
    };
    let fixes = super::fix_signatures(world, nodes.as_deref()).map_err(refused)?;
    let fixed: Vec<Value> = fixes
        .iter()
        .map(|f| {
            json!({
                "node": f.node,
                "rewired": f.rewired,
                "dropped": f.dropped,
                "unwired": f.unwired,
                "output": f.output,
            })
        })
        .collect();
    let stale = world
        .resource_mut::<MatGraphEditor>()
        .diagnostics()
        .stale
        .iter()
        .map(|s| s.node.clone())
        .collect::<Vec<_>>();
    Ok(edit_answer(world, json!({ "fixed": fixed, "stale": stale })))
}

/// `editor.matgraph_function_users`.
fn matgraph_function_users(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world);
    let (path, sig) = match str_param(params, "path") {
        Some(p) => {
            let path = request_path_ext(world, p, ".matfunc.ron")?;
            // The open function tab's (unsaved) signature wins over the file.
            let open = world.get_resource::<MatGraphEditor>().and_then(|e| {
                (e.path.as_deref() == Some(path.as_path()))
                    .then(|| e.doc.to_function())
                    .flatten()
            });
            let function = match open {
                Some(f) => f,
                None => {
                    let text = std::fs::read_to_string(&path).map_err(|e| {
                        RpcError::not_found(format!("cannot read {}: {e}", path.display()), "check the path")
                    })?;
                    regia_matgraph::MaterialFunction::from_ron(&text).map_err(|e| refused(e.to_string()))?
                }
            };
            (path, super::function::Signature::of(&function))
        }
        None => {
            let editor = open_editor(world)?;
            let (Some(function), Some(path)) = (editor.doc.to_function(), editor.path.clone()) else {
                return Err(RpcError::invalid_params(
                    "pass `path`, or open a saved function (editor.matgraph_function_open)",
                ));
            };
            (path, super::function::Signature::of(&function))
        }
    };
    let scan = root
        .clone()
        .or_else(|| path.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_default();
    let _span = tracing::info_span!(target: "regia_editor", "matgraph_function_users").entered();
    let users = super::function::function_users(&scan, root.as_deref(), &path, &sig);
    let users: Vec<Value> = users
        .iter()
        .map(|u| {
            json!({
                "path": u.path.display().to_string(),
                "function": u.function,
                "calls": u.calls,
                "stale": u.stale.iter().map(|s| json!({ "node": s.node, "message": s.message() })).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "path": path.display().to_string(),
        "signature": { "inputs": sig.inputs, "outputs": sig.outputs },
        "users": users,
    }))
}

/// `editor.matgraph_instance_open`.
fn matgraph_instance_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let path = request_path_ext(world, required(params, "path")?, ".matinst.ron")?;
    let root = project_root(world);
    let bad = |e: String| {
        code_error(
            "matgraph_instance_invalid",
            e,
            "check the instance's `parent` path",
        )
    };
    let (reference, parent) = if create {
        let reference = required(params, "parent")?.to_string();
        let parent = super::instance_parent_path(Some(&path), root.as_deref(), &reference).map_err(bad)?;
        (reference, parent)
    } else {
        if !path.is_file() {
            return Err(RpcError::not_found(
                format!("no file {}", path.display()),
                "check the path, or pass create: true with parent",
            ));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| bad(format!("{}: {e}", path.display())))?;
        let instance = regia_matgraph::MaterialInstance::from_ron(&text).map_err(|e| bad(e.to_string()))?;
        let parent =
            super::instance_parent_path(Some(&path), root.as_deref(), &instance.parent).map_err(bad)?;
        (instance.parent, parent)
    };
    let parent = crate::agent_ext::document_request_path(world, "parent", &parent.to_string_lossy())?;
    if !parent.is_file() {
        return Err(bad(format!(
            "the parent {} (`{reference}`) does not exist",
            parent.display()
        )));
    }
    let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
    editor.root.clone_from(&root);
    let previous = editor.tabs().len();
    editor.open_tab();
    let opened = if create {
        editor.new_instance(Some(path), &parent, &reference)
    } else {
        editor.open_instance(&path, &parent)
    };
    if let Err(message) = opened {
        editor.close_tab();
        if previous > 0 {
            let _ = editor.select_tab(previous.saturating_sub(1));
        }
        return Err(bad(message));
    }
    let ok = editor.diagnostics().ok;
    let instance = editor
        .doc
        .instance_meta()
        .map(|i| instance_json(i, Some(parent.as_path())));
    Ok(json!({
        "path": editor.path.as_ref().map(|p| p.display().to_string()),
        "tab": editor.tabs().iter().position(|t| t.active),
        "parent": parent.display().to_string(),
        "overrides": instance,
        "ok": ok,
        "revision": editor.revision,
    }))
}

/// A parameter value from JSON (`{ "Float": 0.5 }`, `{ "Color": [r, g, b] }`), RON text, or a
/// bare number / array shaped like `declared`.
pub(super) fn param_value(
    value: &Value,
    declared: Option<regia_matgraph::ParamValue>,
) -> Result<regia_matgraph::ParamValue, RpcError> {
    use regia_matgraph::ParamValue as P;
    let floats = |v: &Value| -> Option<Vec<f32>> {
        match v {
            Value::Number(n) => n.as_f64().map(|x| vec![x as f32]),
            Value::Array(a) => a.iter().map(|x| x.as_f64().map(|x| x as f32)).collect(),
            _ => None,
        }
    };
    if let Some(f) = floats(value) {
        let shaped = match (declared, f.as_slice()) {
            (Some(P::Float(_)), [x]) => Some(P::Float(*x)),
            (Some(P::Vec2(_)), [x, y]) => Some(P::Vec2([*x, *y])),
            (Some(P::Vec3(_)), [x, y, z]) => Some(P::Vec3([*x, *y, *z])),
            (Some(P::Color(_)), [x, y, z]) => Some(P::Color([*x, *y, *z])),
            (Some(P::Vec4(_)), [x, y, z, w]) => Some(P::Vec4([*x, *y, *z, *w])),
            _ => None,
        };
        return shaped.ok_or_else(|| {
            RpcError::invalid_params(
                "`value` does not have the parameter's width (or the parameter is not declared)",
            )
        });
    }
    let parsed = match value {
        Value::String(text) => ron::from_str::<P>(text).map_err(|e| e.to_string()),
        other => serde_json::from_value::<P>(other.clone()).map_err(|e| e.to_string()),
    };
    parsed.map_err(|e| RpcError::invalid_params(format!("`value` is not a parameter value: {e}")))
}

/// `editor.matgraph_instance_set`.
fn matgraph_instance_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let declared = {
        let editor = open_editor(world)?;
        if editor.doc.instance_meta().is_none() {
            return Err(code_error(
                "matgraph_instance_invalid",
                "the open document is not a material instance".into(),
                "open one with editor.matgraph_instance_open",
            ));
        }
        str_param(params, "param")
            .and_then(|n| editor.doc.graph.params.iter().find(|p| p.name == n))
            .map(|p| p.value)
    };
    let clear = params.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let value = params.get("value");
    let need = || RpcError::invalid_params("`value` is required unless `clear: true`");
    let result = match (
        str_param(params, "param"),
        str_param(params, "texture"),
        str_param(params, "switch"),
    ) {
        (Some(name), None, None) => {
            let v = if clear {
                None
            } else {
                Some(param_value(value.ok_or_else(need)?, declared)?)
            };
            let name = name.to_string();
            edit(world, "instance parameter", None, move |d| {
                d.set_param_override(&name, v)
            })
        }
        (None, Some(name), None) => {
            let v = if clear {
                None
            } else {
                Some(
                    value
                        .and_then(Value::as_str)
                        .ok_or_else(|| RpcError::invalid_params("texture `value` is an image path"))?
                        .to_string(),
                )
            };
            let name = name.to_string();
            edit(world, "instance texture", None, move |d| {
                d.set_texture_override(&name, v.as_deref())
            })
        }
        (None, None, Some(name)) => {
            let v = if clear {
                None
            } else {
                Some(
                    value
                        .and_then(Value::as_bool)
                        .ok_or_else(|| RpcError::invalid_params("switch `value` is true or false"))?,
                )
            };
            let name = name.to_string();
            edit(world, "instance switch", None, move |d| {
                d.set_switch_override(&name, v)
            })
        }
        _ => {
            return Err(RpcError::invalid_params(
                "pass exactly one of `param`, `texture`, `switch`",
            ));
        }
    };
    result.map_err(|e| {
        code_error(
            "matgraph_instance_invalid",
            e,
            "editor.matgraph_state lists the parent's declarations",
        )
    })?;
    let overrides = {
        let editor = world.resource::<MatGraphEditor>();
        editor
            .doc
            .instance_meta()
            .map(|i| instance_json(i, editor.parent_path.as_deref()))
    };
    Ok(edit_answer(world, json!({ "overrides": overrides })))
}

/// `editor.matgraph_tabs`.
fn matgraph_tabs(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut editor = world.get_resource_or_insert_with(MatGraphEditor::default);
    if let Some(index) = params.get("select").and_then(Value::as_u64) {
        editor
            .select_tab(usize::try_from(index).unwrap_or(usize::MAX))
            .map_err(RpcError::invalid_params)?;
    }
    if params.get("close").and_then(Value::as_bool).unwrap_or(false) {
        if !editor.is_open() {
            return Err(no_graph());
        }
        editor.close_tab();
    }
    let tabs = editor.tabs();
    let list: Vec<Value> = tabs
        .iter()
        .map(|t| {
            json!({
                "kind": t.kind,
                "path": t.path.as_ref().map(|p| p.display().to_string()),
                "label": t.label,
                "dirty": t.dirty,
                "active": t.active,
            })
        })
        .collect();
    Ok(json!({ "tabs": list, "active": tabs.iter().position(|t| t.active) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let (_, handler) = methods()
            .into_iter()
            .find(|(s, _)| s.name == method)
            .unwrap_or_else(|| panic!("no method {method}"));
        handler(world, &params)
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(crate::command::UndoStack::new());
        world
    }

    #[test]
    fn method_names_are_unique_and_namespaced() {
        let names: Vec<&str> = methods().iter().map(|(s, _)| s.name).collect();
        let unique: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len());
        assert!(names.iter().all(|n| n.starts_with("editor.matgraph_")));
    }

    #[test]
    fn calls_before_open_say_how_to_open() {
        let mut world = world();
        let err = call(&mut world, "editor.matgraph_state", json!({})).expect_err("error");
        assert!(err.message.contains("no material graph"), "{}", err.message);
    }

    #[test]
    fn build_a_graph_over_the_protocol_then_undo() {
        let mut world = world();
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "params", "params": [{ "name": "tint", "value": { "Color": [0.8, 0.2, 0.1] } }] }),
        )
        .expect("params");
        let r = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "tint", "node": "Param(\"tint\")" }),
        )
        .expect("add ron");
        assert_eq!(r["name"], "tint");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "vcol", "node": "VertexColor" }),
        )
        .expect("add json unit");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "rgb", "node": { "Swizzle": { "input": "vcol", "channels": "rgb" } } }),
        )
        .expect("add json struct");
        let r = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Mul", "pos": [10.0, 20.0] }),
        )
        .expect("add kind");
        let mul = r["name"].as_str().expect("name").to_string();
        assert_eq!(mul, "mul_1");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "connect", "from": "tint", "to": mul, "slot": "a" }),
        )
        .expect("connect a");
        let r = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color", "from": mul }),
        )
        .expect("output");
        assert_eq!(r["ok"], false, "b is still unwired");
        assert_eq!(r["errors"][0]["target"], format!("node:{mul}"));
        let r = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "connect", "from": "rgb", "to": mul, "slot": "b" }),
        )
        .expect("connect b");
        assert_eq!(r["ok"], true, "{r}");
        let state = call(&mut world, "editor.matgraph_state", json!({ "ron": true })).expect("state");
        assert_eq!(state["ok"], true);
        assert_eq!(state["outputs"]["base_color"], mul);
        let node = state["nodes"]
            .as_array()
            .and_then(|n| n.iter().find(|n| n["name"] == mul))
            .expect("mul listed");
        assert_eq!(node["inputs"][1], json!({ "slot": "b", "from": "rgb" }));
        assert_eq!(node["pos"], json!([10.0, 20.0]));
        assert_eq!(node["drives"], json!(["base_color"]));
        assert!(
            state["ron"]
                .as_str()
                .is_some_and(|r| r.contains("Mul(\"tint\", \"rgb\")")),
            "{}",
            state["ron"]
        );
        // Undo the last connect: `b` is unwired again and the error is back on the node.
        assert!(crate::command::undo_world(&mut world));
        let state = call(&mut world, "editor.matgraph_state", json!({})).expect("state");
        assert_eq!(state["ok"], false);
        // Full compile reports the layout.
        assert!(crate::command::redo_world(&mut world));
        let c = call(
            &mut world,
            "editor.matgraph_compile",
            json!({ "include_wgsl": true }),
        )
        .expect("compile");
        assert_eq!(c["ok"], true, "{c}");
        assert_eq!(
            c["params"][0],
            json!({ "name": "tint", "slot": 1, "type": "Vec3" })
        );
        assert!(
            c["wgsl"]
                .as_str()
                .is_some_and(|w| w.contains("fn material_surface"))
        );
        // The shipped permutation policy, per profile, for this graph alone.
        let policy = &c["policy"];
        assert_eq!(policy["permutations"], 1, "{policy}");
        assert_eq!(policy["distinct_codes"], 1, "{policy}");
        let profiles = policy["profiles"].as_array().expect("profiles");
        assert_eq!(profiles.len(), super::super::model::policy_presets().len());
        for p in profiles {
            assert_eq!(p["sets"], 1, "{p}");
            assert_eq!(p["over_budget"], false, "{p}");
            assert!(p["pipelines"].as_u64().is_some_and(|n| n > 0), "{p}");
        }
    }

    #[test]
    fn refusals_are_invalid_params_with_the_reason() {
        let mut world = world();
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        // A position that is not finite as f32, and an output without `from`, are refused.
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Time", "pos": [1e39, 0.0] }),
        )
        .expect_err("inf");
        assert_eq!(err.code, INVALID_PARAMS);
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color" }),
        )
        .expect_err("no from");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(err.message.contains("`from`"), "{}", err.message);
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "connect", "from": "x", "to": "y", "slot": "a" }),
        )
        .expect_err("refused");
        assert_eq!(err.code, INVALID_PARAMS);
        assert_eq!(
            err.data.as_ref().map(|d| &d["code"]),
            Some(&json!("matgraph_edit_refused"))
        );
        assert!(err.message.contains("no node `x`"), "{}", err.message);
        let err = call(&mut world, "editor.matgraph_edit", json!({ "op": "explode" })).expect_err("refused");
        assert!(err.message.contains("unknown op"));
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Nope" }),
        )
        .expect_err("refused");
        assert!(err.message.contains("unknown kind"));
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "node": "Mull(" }),
        )
        .expect_err("refused");
        assert!(err.message.contains("not a node"));
    }

    /// `pos` is exactly two finite numbers. A null or a string in the array is not a coordinate.
    #[test]
    fn pos_drops_nothing_and_rejects_a_non_number() {
        let mut world = world();
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        let added = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Time", "pos": [1.0, 2.0] }),
        )
        .expect("add");
        let name = added["name"].as_str().expect("name").to_string();
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "move", "name": name, "pos": [4.0, 5.0] }),
        )
        .expect("move");
        for pos in [json!([1.0, null, 2.0]), json!([10.0, "px", 20.0])] {
            let err = call(
                &mut world,
                "editor.matgraph_edit",
                json!({ "op": "move", "name": name, "pos": pos }),
            )
            .expect_err("non-number");
            assert_eq!(err.code, INVALID_PARAMS, "{pos}");
            assert_eq!(
                world.resource::<MatGraphEditor>().doc.layout.get(&name).copied(),
                Some([4.0, 5.0]),
                "{pos}"
            );
        }
    }

    #[test]
    fn remove_reports_readers_and_rename_follows() {
        let mut world = world();
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        for (name, node) in [("a", "Float(0.5)"), ("s", "Saturate(\"a\")")] {
            call(
                &mut world,
                "editor.matgraph_edit",
                json!({ "op": "add", "name": name, "node": node }),
            )
            .expect("add");
        }
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "roughness", "from": "s" }),
        )
        .expect("out");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "rename", "name": "a", "to": "half" }),
        )
        .expect("rename");
        let state = call(&mut world, "editor.matgraph_state", json!({})).expect("state");
        assert_eq!(state["ok"], true);
        let r = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "remove", "name": "half" }),
        )
        .expect("remove");
        assert_eq!(r["readers"], json!(["s"]));
        assert_eq!(r["ok"], false);
        assert_eq!(r["errors"][0]["message"], "an input is not connected");
    }

    #[test]
    fn save_writes_graph_and_layout_and_open_reads_them() {
        let dir = regia_test_utils::unique_temp_dir("matgraph-editor-agent");
        let path = dir.join("g.matgraph.ron");
        let mut world = world();
        call(
            &mut world,
            "editor.matgraph_open",
            json!({ "create": true, "path": path.display().to_string() }),
        )
        .expect("open");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "c", "node": "Vec3((0.1, 0.5, 0.9))", "pos": [3.0, 4.0] }),
        )
        .expect("add");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color", "from": "c" }),
        )
        .expect("out");
        let r = call(&mut world, "editor.matgraph_save", json!({})).expect("save");
        assert_eq!(r["ok"], true);
        assert!(path.exists() && super::super::layout_path(&path).exists());
        // Nothing is lost by opening the saved graph; an edit since the save is, and the answer says so.
        let open = json!({ "path": path.display().to_string() });
        let r = call(&mut world, "editor.matgraph_open", open.clone()).expect("reopen");
        assert_eq!(r["discarded_unsaved"], false);
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Time" }),
        )
        .expect("edit");
        let r = call(&mut world, "editor.matgraph_open", open).expect("reopen over an edit");
        assert_eq!(r["discarded_unsaved"], true);
        let mut other = self::world();
        let r = call(
            &mut other,
            "editor.matgraph_open",
            json!({ "path": path.display().to_string() }),
        )
        .expect("reopen");
        assert_eq!(r["nodes"], 1);
        let state = call(&mut other, "editor.matgraph_state", json!({})).expect("state");
        assert_eq!(state["dirty"], false);
        assert_eq!(state["nodes"][0]["pos"], json!([3.0, 4.0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ADR-1816: open and save refuse paths that lead outside the project and the allowed roots,
    /// and nothing is written.
    #[test]
    fn open_and_save_refuse_paths_outside_the_project() {
        let dir = regia_test_utils::unique_temp_dir("matgraph-editor-escape");
        let mut world = world();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        let bad = vec!["../escape.matgraph.ron".to_string()];
        // Rebound so the binding is not a needless `mut` on Windows.
        #[cfg(unix)]
        let bad = {
            let mut bad = bad;
            bad.push("/etc/regia-never.matgraph.ron".to_string());
            bad
        };
        for p in &bad {
            for method in ["editor.matgraph_save", "editor.matgraph_open"] {
                let err = call(&mut world, method, json!({ "path": p })).expect_err(p);
                let reason = err.data.as_ref().and_then(|d| d["reason"].as_str());
                assert_eq!(reason, Some("path_outside_project"), "{method} {p}: {err:?}");
            }
        }
        assert!(!dir.join("../escape.matgraph.ron").exists());
        // Only `.matgraph.ron` files: a save cannot overwrite another kind of file.
        let err = call(
            &mut world,
            "editor.matgraph_save",
            json!({ "path": "Cargo.toml" }),
        )
        .expect_err("not a graph file");
        assert_eq!(err.code, INVALID_PARAMS);
        assert!(!dir.join("Cargo.toml").exists());
        // A project path still saves (under `assets/`, folders created).
        let r = call(
            &mut world,
            "editor.matgraph_save",
            json!({ "path": "assets/materials/ok.matgraph.ron" }),
        )
        .expect("save under the project");
        assert!(
            r["path"].as_str().is_some_and(|p| p.ends_with("ok.matgraph.ron")),
            "{r}"
        );
        assert!(dir.join("assets/materials/ok.matgraph.ron").is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_renders_or_reports_no_gpu() {
        let mut world = world();
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "c", "node": "Vec3((0.9, 0.05, 0.03))" }),
        )
        .expect("add");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color", "from": "c" }),
        )
        .expect("out");
        match call(
            &mut world,
            "editor.matgraph_preview",
            json!({ "width": 96, "height": 96 }),
        ) {
            Ok(r) => {
                assert_eq!(r["width"], 96);
                assert!(r["png"].as_str().is_some_and(|p| !p.is_empty()));
                let c = &r["center_rgb"];
                let (red, green) = (c[0].as_u64().unwrap_or(0), c[1].as_u64().unwrap_or(255));
                assert!(red > 2 * green, "a red sphere: {r}");
            }
            Err(err) => {
                let code = err
                    .data
                    .as_ref()
                    .and_then(|d| d["code"].as_str())
                    .unwrap_or_default()
                    .to_string();
                assert_eq!(
                    code, "F0011",
                    "only a missing GPU may stop the preview: {}",
                    err.message
                );
            }
        }
    }
    fn data_code(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["code"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// Spec 80 § 4: the shared codes `F0002` (bad parameters, unknown op) and `F0003` (no graph
    /// open), and the method codes `matgraph_preview_failed` and `matgraph_save_failed`, reach
    /// the caller in `data.code`. None of these needs a GPU.
    #[test]
    fn error_codes_match_the_spec() {
        let mut world = world();
        for method in [
            "editor.matgraph_state",
            "editor.matgraph_edit",
            "editor.matgraph_compile",
            "editor.matgraph_preview",
            "editor.matgraph_save",
        ] {
            let err = call(&mut world, method, json!({ "op": "add", "kind": "Time" })).expect_err(method);
            assert_eq!(data_code(&err), "F0003", "{method} before open: {err:?}");
        }
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        for bad in [
            json!({ "op": "explode" }),
            json!({}),
            json!({ "op": "add", "kind": "Nope" }),
            json!({ "op": "move", "name": "x" }),
        ] {
            let err = call(&mut world, "editor.matgraph_edit", bad.clone()).expect_err("bad params");
            assert_eq!(err.code, INVALID_PARAMS, "{bad}");
            assert_eq!(data_code(&err), "F0002", "{bad}: {err:?}");
        }
        // A new graph has no path yet: save without one fails with the method code.
        let err = call(&mut world, "editor.matgraph_save", json!({})).expect_err("no path");
        assert_eq!(data_code(&err), "matgraph_save_failed", "{err:?}");
        // A folder that cannot be created (a file is in the way) fails the same way.
        let dir = regia_test_utils::unique_temp_dir("matgraph-editor-save-fail");
        std::fs::write(dir.join("blocker"), "not a folder").expect("blocker");
        let target = dir.join("blocker").join("g.matgraph.ron");
        let err = call(
            &mut world,
            "editor.matgraph_save",
            json!({ "path": target.display().to_string() }),
        )
        .expect_err("blocked");
        assert_eq!(data_code(&err), "matgraph_save_failed", "{err:?}");
        assert!(!target.exists());
        // w11 (ADR-7730): a material function previews on its own (here its first output: its
        // calling graph was never saved). It renders, or answers the no-GPU code; it is no longer
        // refused as `matgraph_preview_failed`.
        let func = dir.join("f.matfunc.ron");
        call(
            &mut world,
            "editor.matgraph_function_open",
            json!({ "path": func.display().to_string(), "create": true }),
        )
        .expect("function open");
        match call(&mut world, "editor.matgraph_preview", json!({})) {
            Ok(preview) => assert!(preview["png"].is_string(), "{preview}"),
            Err(err) => assert!(
                ["F0011", "editor_headless"].contains(&data_code(&err).as_str()),
                "a function preview needs only a GPU: {err:?}"
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Spec 80 § 6: `editor.matgraph_state` warns only past `PERMUTATION_WARN_SWITCHES` (4)
    /// switches, `permutations` is 2^switches and saturates, and `editor.matgraph_compile`
    /// refuses no graph for its switch count; its policy enumerates at most 64 permutations.
    #[test]
    fn switch_count_warns_past_four_and_permutations_saturate() {
        use super::super::model::{PERMUTATION_WARN_SWITCHES, POLICY_MAX_PERMUTATIONS};
        let mut world = world();
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("open");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "c", "node": "Vec3((0.5, 0.5, 0.5))" }),
        )
        .expect("add");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color", "from": "c" }),
        )
        .expect("out");
        let switches = |n: usize| -> Value {
            (0..n)
                .map(|i| json!({ "name": format!("s{i}"), "default": false }))
                .collect()
        };
        let set = |world: &mut World, n: usize| {
            call(
                world,
                "editor.matgraph_edit",
                json!({ "op": "switches", "switches": switches(n) }),
            )
            .expect("switches");
            call(world, "editor.matgraph_state", json!({})).expect("state")
        };
        assert_eq!(PERMUTATION_WARN_SWITCHES, 4);
        let state = set(&mut world, PERMUTATION_WARN_SWITCHES);
        assert_eq!(state["permutations"], 16);
        assert_eq!(state["warnings"], json!([]), "four switches do not warn");
        let state = set(&mut world, PERMUTATION_WARN_SWITCHES + 1);
        assert_eq!(state["permutations"], 32);
        assert_eq!(state["warnings"].as_array().map(Vec::len), Some(1), "{state}");
        let state = set(&mut world, 7);
        let c = call(&mut world, "editor.matgraph_compile", json!({})).expect("compile");
        assert_eq!(c["ok"], true, "{c}");
        assert_eq!(c["permutations"], 128);
        assert_eq!(c["policy"]["enumerated"], POLICY_MAX_PERMUTATIONS, "{c}");
        assert_eq!(state["permutations"], 128);
        let state = set(&mut world, 64);
        assert_eq!(state["permutations"], u64::MAX, "2^64 saturates");
    }

    /// Spec 80 § 4: the mutating methods (open, edit, save and the function and instance edits)
    /// need `Permission::EditScene`; the readers need only `Permission::Read`.
    #[test]
    fn mutating_methods_need_edit_scene() {
        let schemas: Vec<MethodSchema> = methods().into_iter().map(|(s, _)| s).collect();
        for s in &schemas {
            let want = if s.mutating {
                Permission::EditScene
            } else {
                Permission::Read
            };
            assert_eq!(s.permission, want, "{}", s.name);
        }
        let mutating = |name: &str| {
            schemas
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.mutating)
                .unwrap_or_else(|| panic!("no method {name}"))
        };
        for name in [
            "editor.matgraph_open",
            "editor.matgraph_edit",
            "editor.matgraph_save",
            "editor.matgraph_function_signature",
            "editor.matgraph_function_collapse",
            "editor.matgraph_function_expand",
            "editor.matgraph_function_fix",
            "editor.matgraph_instance_set",
        ] {
            assert!(mutating(name), "{name} edits the document");
        }
        for name in [
            "editor.matgraph_state",
            "editor.matgraph_compile",
            "editor.matgraph_preview",
            "editor.matgraph_function_users",
        ] {
            assert!(!mutating(name), "{name} only reads");
        }
    }

    // ---- Functions, instances, tabs (ADR-2490) ----

    /// A graph with a chain worth collapsing: tint × vertex color, lerped to white by uv.x.
    const CHAIN: &str = r#"(
        format_version: 4,
        params: [(name: "tint", value: Color((0.8, 0.2, 0.1))), (name: "rough", value: Float(0.6))],
        nodes: {
            "tint": Param("tint"),
            "vcol": VertexColor,
            "rgb": Swizzle(input: "vcol", channels: "rgb"),
            "tinted": Mul("tint", "rgb"),
            "uv": TexCoord,
            "u": Swizzle(input: "uv", channels: "x"),
            "white": Vec3((1.0, 1.0, 1.0)),
            "base": Lerp(a: "tinted", b: "white", t: "u"),
            "r": Param("rough"),
        },
        output: (base_color: Some("base"), roughness: Some("r")),
    )"#;

    fn project(label: &str) -> (std::path::PathBuf, World) {
        let dir = regia_test_utils::unique_temp_dir(label);
        let mut world = world();
        world.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        (dir, world)
    }

    fn write(path: &std::path::Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("dir");
        }
        std::fs::write(path, text).expect("write");
    }

    fn wgsl(world: &mut World) -> String {
        let c = call(world, "editor.matgraph_compile", json!({ "include_wgsl": true })).expect("compile");
        assert_eq!(c["ok"], true, "{c}");
        super::super::function::normalized_wgsl(c["wgsl"].as_str().expect("wgsl"))
    }

    /// Brief test 1: create a function from a selection (inline and as a file) and the compiled
    /// WGSL of the material is identical before and after (comments aside); expanding it back is
    /// identical too, and undo restores the file bytes.
    #[test]
    fn collapse_keeps_the_compiled_code_and_undo_restores_the_files() {
        let (dir, mut world) = project("matgraph-collapse");
        let graph_path = dir.join("assets/m/chain.matgraph.ron");
        write(&graph_path, CHAIN);
        call(
            &mut world,
            "editor.matgraph_open",
            json!({ "path": "assets/m/chain.matgraph.ron" }),
        )
        .expect("open");
        call(&mut world, "editor.matgraph_save", json!({})).expect("save as the editor writes it");
        let original = std::fs::read(&graph_path).expect("read");
        let before = wgsl(&mut world);

        let r = call(
            &mut world,
            "editor.matgraph_function_collapse",
            json!({ "nodes": ["rgb", "tinted", "white", "base"], "name": "tint_blend" }),
        )
        .expect("collapse inline");
        assert_eq!(r["calls"], json!(["base"]));
        assert_eq!(r["wgsl_identical"], true, "{r}");
        assert_eq!(r["inputs"][0], json!({ "name": "tint", "ty": "Vec3" }));
        assert_eq!(wgsl(&mut world), before);
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        assert_eq!(depth, 1, "a collapse is one undo step");
        let x = call(
            &mut world,
            "editor.matgraph_function_expand",
            json!({ "node": "base" }),
        )
        .expect("expand");
        assert_eq!(x["result"], "base");
        assert_eq!(wgsl(&mut world), before, "expand gives the same code back");
        assert!(crate::command::undo_world(&mut world));
        assert!(crate::command::undo_world(&mut world));
        call(&mut world, "editor.matgraph_save", json!({})).expect("save");
        assert_eq!(
            std::fs::read(&graph_path).expect("read"),
            original,
            "undo restores the graph's bytes"
        );

        // Into a file: the function file is written, the graph references it, the code is the same.
        let r = call(
            &mut world,
            "editor.matgraph_function_collapse",
            json!({ "nodes": ["uv", "u"], "name": "coord", "file": "coord.matfunc.ron" }),
        )
        .expect("collapse to a file");
        let file = dir.join("assets/m/coord.matfunc.ron");
        assert!(
            r["file"]
                .as_str()
                .is_some_and(|p| p.replace('\\', "/").ends_with("assets/m/coord.matfunc.ron")),
            "{r}"
        );
        assert!(file.is_file());
        assert_eq!(r["wgsl_identical"], true, "{r}");
        assert_eq!(wgsl(&mut world), before);
        let state = call(&mut world, "editor.matgraph_state", json!({ "ron": true })).expect("state");
        assert!(
            state["ron"]
                .as_str()
                .is_some_and(|t| t.contains("File(\"coord.matfunc.ron\")")),
            "{state}"
        );
        // Undo: the graph is back and the file the collapse created is gone; redo writes it again.
        assert!(crate::command::undo_world(&mut world));
        assert!(!file.exists(), "undo removes the function file it wrote");
        assert!(crate::command::redo_world(&mut world));
        assert!(file.is_file());
        assert!(crate::command::undo_world(&mut world));
        call(&mut world, "editor.matgraph_save", json!({})).expect("save");
        assert_eq!(std::fs::read(&graph_path).expect("read"), original);

        // Refusals carry their code.
        let err = call(
            &mut world,
            "editor.matgraph_function_collapse",
            json!({ "nodes": [], "name": "f" }),
        )
        .expect_err("empty");
        assert_eq!(
            err.data.as_ref().map(|d| &d["code"]),
            Some(&json!("matgraph_collapse_invalid"))
        );
        let err = call(
            &mut world,
            "editor.matgraph_function_collapse",
            json!({ "nodes": ["u"], "name": "f", "file": "../out.matfunc.ron" }),
        )
        .expect_err("outside");
        assert_eq!(
            err.data.as_ref().map(|d| &d["code"]),
            Some(&json!("matgraph_collapse_invalid"))
        );
        assert!(err.message.contains("project-relative"), "{}", err.message);
        let err = call(
            &mut world,
            "editor.matgraph_function_expand",
            json!({ "node": "u" }),
        )
        .expect_err("not a call");
        assert_eq!(
            err.data.as_ref().map(|d| &d["code"]),
            Some(&json!("matgraph_expand_invalid"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    const WEAR: &str = r#"(
        format_version: 3,
        name: "wear",
        inputs: [(name: "color", ty: Vec3), (name: "amount", ty: Float)],
        nodes: {
            "color": Input("color"),
            "amount": Input("amount"),
            "dark": Vec3((0.1, 0.1, 0.1)),
            "out": Lerp(a: "color", b: "dark", t: "amount"),
        },
        outputs: { "result": "out" },
    )"#;

    fn user_graph(input: &str) -> String {
        format!(
            r#"(
            format_version: 4,
            functions: {{ "wear": File("wear.matfunc.ron") }},
            nodes: {{
                "c": Vec3((0.8, 0.3, 0.2)),
                "k": Float(0.25),
                "w": Call(function: "wear", args: {{ "{input}": "c", "amount": "k" }}),
            }},
            output: (base_color: Some("w")),
        )"#
        )
    }

    /// Brief test 2: a function opens in its own tab with errors mapped to its nodes; a signature
    /// change marks its users stale, and the fix action rewires them.
    #[test]
    fn a_signature_change_marks_users_stale_and_fix_rewires_them() {
        let (dir, mut world) = project("matgraph-function-users");
        let m = dir.join("assets/m");
        write(&m.join("wear.matfunc.ron"), WEAR);
        write(&m.join("a.matgraph.ron"), &user_graph("color"));
        write(&m.join("b.matgraph.ron"), &user_graph("color"));
        write(&dir.join("assets/other.matgraph.ron"), CHAIN);

        let r = call(
            &mut world,
            "editor.matgraph_open",
            json!({ "path": "assets/m/a.matgraph.ron" }),
        )
        .expect("open a");
        assert_eq!(r["ok"], true, "{r}");
        let state = call(&mut world, "editor.matgraph_state", json!({})).expect("state");
        let w = state["nodes"]
            .as_array()
            .and_then(|n| n.iter().find(|n| n["name"] == "w"))
            .cloned()
            .expect("w");
        assert_eq!(
            w["inputs"].as_array().map(Vec::len),
            Some(2),
            "pins follow the file's signature: {w}"
        );
        assert_eq!(w["inputs"][0]["slot"], "color");
        assert_eq!(w["outputs"], json!(["result"]));

        // The function in its own tab, checked against the graph it was opened from.
        let f = call(
            &mut world,
            "editor.matgraph_function_open",
            json!({ "path": "assets/m/wear.matfunc.ron" }),
        )
        .expect("open f");
        assert_eq!(f["tab"], 1);
        assert_eq!(f["ok"], true, "{f}");
        let tabs = call(&mut world, "editor.matgraph_tabs", json!({})).expect("tabs");
        assert_eq!(tabs["tabs"].as_array().map(Vec::len), Some(2));
        // An error inside maps to the function's node.
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "bad", "node": "Normalize(\"amount\")" }),
        )
        .expect("add");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "connect", "from": "bad", "to": "out", "slot": "t" }),
        )
        .expect("wire");
        let s = call(&mut world, "editor.matgraph_state", json!({})).expect("state");
        assert_eq!(s["kind"], "function");
        assert!(
            s["errors"]
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e["target"] == "node:bad")),
            "{s}"
        );
        assert!(crate::command::undo_world(&mut world));
        assert!(crate::command::undo_world(&mut world));
        // Rename an input: color → tint_in.
        let sig = call(
            &mut world,
            "editor.matgraph_function_signature",
            json!({ "inputs": [{ "name": "tint_in", "ty": "Vec3" }, { "name": "amount", "ty": "Float" }], "outputs": ["result"] }),
        )
        .expect("signature");
        assert_eq!(sig["ok"], false, "the old Input node now names nothing: {sig}");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "set", "name": "color", "node": "Input(\"tint_in\")" }),
        )
        .expect("rewire input");
        let saved = call(&mut world, "editor.matgraph_save", json!({})).expect("save f");
        assert_eq!(saved["ok"], true, "{saved}");
        assert_eq!(saved["kind"], "function");

        let users = call(&mut world, "editor.matgraph_function_users", json!({})).expect("users");
        let list = users["users"].as_array().cloned().unwrap_or_default();
        assert_eq!(list.len(), 2, "a and b use it, other does not: {users}");
        for u in &list {
            assert_eq!(u["calls"], json!(["w"]));
            assert_eq!(u["stale"][0]["node"], "w", "{u}");
        }

        // Back to a: the call is stale; fix rewires it (one undo step) and it compiles.
        call(&mut world, "editor.matgraph_tabs", json!({ "select": 0 })).expect("select a");
        let s = call(&mut world, "editor.matgraph_state", json!({})).expect("state");
        assert_eq!(s["kind"], "graph");
        assert_eq!(s["stale"][0]["node"], "w", "{s}");
        assert_eq!(s["stale"][0]["unknown_args"], json!(["color"]));
        let depth = world.resource::<crate::command::UndoStack>().undo_depth();
        let fix = call(&mut world, "editor.matgraph_function_fix", json!({})).expect("fix");
        assert_eq!(fix["fixed"][0]["rewired"], json!([["color", "tint_in"]]), "{fix}");
        assert_eq!(fix["stale"], json!([]));
        assert_eq!(fix["ok"], true, "{fix}");
        assert_eq!(
            world.resource::<crate::command::UndoStack>().undo_depth(),
            depth + 1
        );
        call(&mut world, "editor.matgraph_save", json!({})).expect("save a");
        let users = call(
            &mut world,
            "editor.matgraph_function_users",
            json!({ "path": "assets/m/wear.matfunc.ron" }),
        )
        .expect("users");
        let stale: Vec<usize> = users["users"]
            .as_array()
            .map(|l| {
                l.iter()
                    .map(|u| u["stale"].as_array().map_or(0, Vec::len))
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(stale, [0, 1], "a is fixed, b still stale: {users}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Brief tests 3 and 4: an instance override written from the canvas equals the same
    /// override written by the instance API, the parent graph is never written, and undo
    /// restores the file bytes.
    #[test]
    fn instance_overrides_match_the_instance_api_and_undo_restores_bytes() {
        let (dir, mut world) = project("matgraph-instance");
        let parent = dir.join("assets/m/chain.matgraph.ron");
        write(&parent, CHAIN);
        let parent_bytes = std::fs::read(&parent).expect("read");
        let inst_path = dir.join("assets/m/red.matinst.ron");
        let mut api = regia_matgraph::MaterialInstance::new("chain.matgraph.ron");
        api.set("rough", regia_matgraph::ParamValue::Float(0.3));
        write(&inst_path, &api.to_ron().expect("ron"));
        let original = std::fs::read(&inst_path).expect("read");

        let r = call(
            &mut world,
            "editor.matgraph_instance_open",
            json!({ "path": "assets/m/red.matinst.ron" }),
        )
        .expect("open");
        assert_eq!(r["ok"], true, "{r}");
        let float = |v: &Value| v["Float"].as_f64().map(|x| x as f32);
        assert_eq!(float(&r["overrides"]["params"]["rough"]), Some(0.3));
        let s = call(&mut world, "editor.matgraph_state", json!({})).expect("state");
        assert_eq!(s["kind"], "instance");
        let r_node = s["nodes"]
            .as_array()
            .and_then(|n| n.iter().find(|n| n["name"] == "r"))
            .cloned()
            .expect("r");
        assert_eq!(
            float(&r_node["override"]),
            Some(0.3),
            "overridden params are marked: {r_node}"
        );
        // The graph is read-only.
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "kind": "Time" }),
        )
        .expect_err("read-only");
        assert_eq!(
            err.data.as_ref().map(|d| &d["code"]),
            Some(&json!("matgraph_read_only"))
        );
        // Override from the canvas.
        let set = call(
            &mut world,
            "editor.matgraph_instance_set",
            json!({ "param": "tint", "value": [0.9, 0.1, 0.1] }),
        )
        .expect("override");
        assert_eq!(set["ok"], true, "{set}");
        call(&mut world, "editor.matgraph_save", json!({})).expect("save");
        let mut expected = api.clone();
        expected.set("tint", regia_matgraph::ParamValue::Color([0.9, 0.1, 0.1]));
        assert_eq!(
            std::fs::read_to_string(&inst_path).expect("read"),
            expected.to_ron().expect("ron"),
            "the canvas writes what the instance API writes"
        );
        assert_eq!(
            std::fs::read(&parent).expect("read"),
            parent_bytes,
            "the parent is never written"
        );
        // The preview graph carries the override; a bad one is refused.
        let compiled = call(&mut world, "editor.matgraph_compile", json!({})).expect("compile");
        assert_eq!(compiled["ok"], true);
        let err = call(
            &mut world,
            "editor.matgraph_instance_set",
            json!({ "param": "nope", "value": 1.0 }),
        )
        .expect_err("unknown");
        assert_eq!(err.code, INVALID_PARAMS);
        let err = call(
            &mut world,
            "editor.matgraph_instance_set",
            json!({ "param": "rough", "value": { "Vec2": [1.0, 0.0] } }),
        )
        .expect_err("type");
        assert_eq!(
            err.data.as_ref().map(|d| &d["code"]),
            Some(&json!("matgraph_instance_invalid"))
        );
        // Undo, save: the original bytes.
        assert!(crate::command::undo_world(&mut world));
        call(&mut world, "editor.matgraph_save", json!({})).expect("save");
        assert_eq!(
            std::fs::read(&inst_path).expect("read"),
            original,
            "undo restores the file bytes"
        );
        // Clear an override, and a new instance from scratch.
        call(
            &mut world,
            "editor.matgraph_instance_set",
            json!({ "param": "rough", "clear": true }),
        )
        .expect("clear");
        let mut cleared = api.clone();
        cleared.params.remove("rough");
        call(&mut world, "editor.matgraph_save", json!({})).expect("save");
        assert_eq!(
            std::fs::read_to_string(&inst_path).expect("read"),
            cleared.to_ron().expect("ron")
        );
        let new = call(
            &mut world,
            "editor.matgraph_instance_open",
            json!({ "path": "assets/m/blue.matinst.ron", "create": true, "parent": "chain.matgraph.ron" }),
        )
        .expect("create");
        assert_eq!(new["tab"], 1);
        call(
            &mut world,
            "editor.matgraph_instance_set",
            json!({ "switch": "nope", "value": true }),
        )
        .expect_err("no switch");
        let err = call(
            &mut world,
            "editor.matgraph_save",
            json!({ "path": "assets/m/x.matgraph.ron" }),
        )
        .expect_err("ext");
        assert!(err.message.contains(".matinst.ron"), "{}", err.message);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Undo of an edit in a tab that is not active edits that tab, not the active one.
    #[test]
    fn undo_follows_its_tab() {
        let (dir, mut world) = project("matgraph-tabs");
        call(&mut world, "editor.matgraph_open", json!({ "create": true })).expect("graph");
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "add", "name": "t", "kind": "Time" }),
        )
        .expect("add");
        call(
            &mut world,
            "editor.matgraph_function_open",
            json!({ "path": "assets/f.matfunc.ron", "create": true }),
        )
        .expect("function");
        let f = world.resource::<MatGraphEditor>().doc.clone();
        // The graph's add is undone in its parked tab.
        assert!(crate::command::undo_world(&mut world));
        assert_eq!(
            world.resource::<MatGraphEditor>().doc,
            f,
            "the active function is untouched"
        );
        call(&mut world, "editor.matgraph_tabs", json!({ "select": 0 })).expect("select");
        assert!(world.resource::<MatGraphEditor>().doc.graph.nodes.is_empty());
        call(&mut world, "editor.matgraph_tabs", json!({ "close": true })).expect("close");
        let tabs = call(&mut world, "editor.matgraph_tabs", json!({})).expect("tabs");
        assert_eq!(tabs["tabs"].as_array().map(Vec::len), Some(1));
        assert_eq!(tabs["tabs"][0]["kind"], "function");
        // A function has no surface outputs or declarations (its preview: below).
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "output", "slot": "base_color", "from": "x" }),
        )
        .expect_err("no slot");
        assert!(err.message.contains("no output `base_color`"), "{}", err.message);
        call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "params", "params": [] }),
        )
        .expect("no change is fine");
        let err = call(
            &mut world,
            "editor.matgraph_edit",
            json!({ "op": "params", "params": [{ "name": "p", "value": { "Float": 1.0 } }] }),
        )
        .expect_err("declarations");
        assert!(err.message.contains("no declarations"), "{}", err.message);
        // w11 (ADR-7730): a function previews on its own (here its first output: its calling
        // graph was never saved and its tab is closed), so the preview renders or needs only a GPU.
        match call(&mut world, "editor.matgraph_preview", json!({})) {
            Ok(preview) => assert!(preview["png"].is_string(), "{preview}"),
            Err(err) => assert!(
                ["F0011", "editor_headless"].contains(&data_code(&err).as_str()),
                "a function preview needs only a GPU: {err:?}"
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
