// SPDX-License-Identifier: MPL-2.0
//! The UI designer's agent-protocol surface (`book/agents/ui-designer.md`, ADR-1811): open or
//! start a `.ui.ron` document, read its tree, edit it (add, remove, move, duplicate, set, repeat,
//! converter, image, mask, select, undo, redo), list its pictures, lint it, preview its layout at a window size and safe area, and save it. The
//! model is [`UiDesigner`]; the egui panel shares it.

use std::path::PathBuf;

use regia_agent::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use regia_ui::authoring::{AnimationBindDef, RepeatRowHeight, ScreenBackDef, UiNodeDef};
use regia_ui::{BindConverter, ViewModels};
use serde_json::{Value, json};

use crate::ui_designer::{
    BindSourceEdit, DesignOp, DesignerError, LintFinding, NodePath, PRESETS, UiDesign, UiDesigner,
    bind_source_op, converter_op, enclosing_repeats, lint, node_at, node_json, node_kind, path_string,
    preset, repeat_op_with, resolve_node, walk,
};
use crate::ui_designer_canvas::{
    AlignKind, Axis, Guide, SnapSettings, TransformMode, TransformRequest, plan_group_move, plan_transform,
};
use crate::ui_designer_gpu::{ThemeChoice, preview_device, project_theme_files, themed_preview};
use crate::ui_designer_multi::{copy_text, paste_target, shared_fields};
use crate::ui_designer_preview::{
    MAX_REPEAT_COUNT, PreviewFrame, SIZE_PRESETS, preview, project_localization,
};
use crate::viewport_agent::Handler;

/// Every error code the methods return (as `data.code`).
pub const ERROR_CODES: [&str; 10] = [
    "ui_designer_no_document",
    "ui_designer_bad_node",
    "ui_designer_bad_op",
    "ui_designer_bad_value",
    "ui_designer_bad_move",
    "ui_designer_nothing_to_undo",
    "ui_designer_nothing_to_redo",
    "ui_designer_parse",
    "ui_designer_io",
    "ui_designer_bad_size",
];

/// Error codes only `editor.ui_designer_render` returns (they need a GPU, or its absence, to
/// show up), besides the shared ones above.
pub const RENDER_ERROR_CODES: [&str; 3] = [
    "ui_designer_unknown_theme",
    "ui_designer_no_gpu",
    "ui_designer_render_failed",
];

fn rpc(e: DesignerError) -> RpcError {
    let (code, hint) = match e.code {
        "no_document" => (NOT_FOUND, "call editor.ui_designer_open first"),
        "bad_node" => (
            NOT_FOUND,
            "editor.ui_designer_state lists node paths (\"/0/1\") and names (\"#name\")",
        ),
        "io" => (INTERNAL_ERROR, "check the path and its permissions"),
        "parse" => (
            INVALID_PARAMS,
            "the file is not a loadable .ui.ron; nothing was changed",
        ),
        "nothing_to_undo" | "nothing_to_redo" => (
            INVALID_PARAMS,
            "editor.ui_designer_state reports undo and redo counts",
        ),
        "bad_size" => (
            INVALID_PARAMS,
            "size is [w, h] in 1..=8192 or a preset name; insets leave some area",
        ),
        "unknown_theme" => (
            NOT_FOUND,
            "theme names come from ui.theme_list and the project's assets/ui/themes",
        ),
        "no_gpu" => (
            INTERNAL_ERROR,
            "install lavapipe (Vulkan software) or a GPU driver; editor.ui_designer_preview needs none",
        ),
        "render_failed" => (INTERNAL_ERROR, "retry; report it if it repeats"),
        _ => (INVALID_PARAMS, "the message names the field; nothing was changed"),
    };
    RpcError::new(code, &format!("ui_designer_{}", e.code), e.message, hint)
}

/// A path from the caller, through the editor's one document-path rule (ADR-7388, audit
/// rc1-sec-paths-z finding 1): relative under the project, absolute only under the project or the
/// temp directory; `..`, drive letters, UNC forms, NUL and symlinks leading out are
/// `path_outside_project`.
pub(crate) fn resolve_path(world: &World, text: &str) -> Result<PathBuf, RpcError> {
    crate::agent_ext::document_request_path(world, "path", text)
}

/// [`resolve_path`] for a document the caller saves or creates: it must also end in `suffix`, so a
/// save cannot overwrite a project file of another kind (`ui_designer_bad_value`).
pub(crate) fn resolve_doc_path(world: &World, text: &str, suffix: &str) -> Result<PathBuf, RpcError> {
    if !text.to_ascii_lowercase().ends_with(suffix) {
        return Err(rpc(DesignerError::new(
            "bad_value",
            format!("`path` must name a {suffix} file, not {text:?}"),
        )));
    }
    resolve_path(world, text)
}

pub(crate) fn project_root(world: &World) -> Option<PathBuf> {
    world
        .get_resource::<crate::agent_ext::EditorProject>()
        .and_then(|p| p.root.clone())
}

fn doc(world: &mut World) -> Result<&mut UiDesign, RpcError> {
    world
        .get_resource_or_insert_with(UiDesigner::default)
        .into_inner()
        .doc_mut()
        .map_err(rpc)
}

fn node_param(doc: &UiDesign, params: &Value, key: &str) -> Result<NodePath, RpcError> {
    let text = params.get(key).and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(format!("`{key}` (a node path \"/0/1\" or \"#name\") is required"))
    })?;
    resolve_node(&doc.file().root, text).map_err(rpc)
}

fn summary(doc: &UiDesign) -> Value {
    json!({
        "open": true,
        "path": doc.path.as_ref().map(|p| p.display().to_string()),
        "version": doc.file().version,
        "opened_version": doc.opened_version,
        "dirty": doc.dirty(),
        "revision": doc.revision(),
        "undo": doc.undo_len(),
        "redo": doc.redo_len(),
        "had_comments": doc.had_comments,
        "last_save_canonical": doc.last_save_canonical,
        "comments_lost": doc.comments_lost,
        "selection": doc.selection.as_deref().map(path_string),
        "selected": doc.selected_nodes().iter().map(|p| path_string(p)).collect::<Vec<_>>(),
    })
}

/// The nodes an op works on: `nodes` (a list of paths or names), else `node`, else the selection.
/// Both `nodes` and `node` (neither `null`) is refused: the op cannot tell which one was meant.
fn nodes_param(doc: &UiDesign, params: &Value) -> Result<Vec<NodePath>, RpcError> {
    let given = |key: &str| params.get(key).is_some_and(|v| !v.is_null());
    if given("nodes") && given("node") {
        return Err(RpcError::invalid_params("give `nodes` or `node`, not both"));
    }
    match params.get("nodes") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                let text = v.as_str().ok_or_else(|| {
                    RpcError::invalid_params("`nodes` is a list of node paths (\"/0/1\") or \"#name\"s")
                })?;
                resolve_node(&doc.file().root, text).map_err(rpc)
            })
            .collect(),
        Some(Value::Null) | None => match params.get("node") {
            Some(_) => Ok(vec![node_param(doc, params, "node")?]),
            None => Ok(doc.selected_nodes()),
        },
        Some(_) => Err(RpcError::invalid_params(
            "`nodes` is a list of node paths (\"/0/1\") or \"#name\"s",
        )),
    }
}

/// `snap`: absent or `false` (an agent's rect is taken as given), `true` (edges, siblings and an
/// 8 px grid within 6 px), or `{edges?, siblings?, grid?, threshold?}`.
fn snap_param(params: &Value) -> Result<SnapSettings, RpcError> {
    match params.get("snap") {
        Some(Value::Bool(true)) => Ok(SnapSettings::ON),
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(SnapSettings::OFF),
        Some(Value::Object(map)) => {
            let mut snap = SnapSettings::ON;
            let flag = |k: &str, d: bool| map.get(k).and_then(Value::as_bool).unwrap_or(d);
            snap.edges = flag("edges", true);
            snap.siblings = flag("siblings", true);
            match map.get("grid") {
                Some(Value::Null) => snap.grid = None,
                Some(v) => {
                    snap.grid = Some(v.as_f64().map(|g| g as f32).filter(|g| *g > 0.0).ok_or_else(|| {
                        rpc(DesignerError::new(
                            "bad_value",
                            "snap.grid is a spacing > 0 in UI px (null: no grid)",
                        ))
                    })?)
                }
                None => {}
            }
            if let Some(v) = map.get("threshold") {
                snap.threshold = v
                    .as_f64()
                    .map(|t| t as f32)
                    .filter(|t| *t >= 0.0)
                    .ok_or_else(|| {
                        rpc(DesignerError::new(
                            "bad_value",
                            "snap.threshold is a distance >= 0 in frame px",
                        ))
                    })?;
            }
            Ok(snap)
        }
        Some(_) => Err(rpc(DesignerError::new(
            "bad_value",
            "snap is true, false or {edges, siblings, grid, threshold}",
        ))),
    }
}

/// `rect`: `[x, y, width, height]` in the preview frame's px.
/// The guide lines of a snapped transform as the protocol reports them.
fn guides_json(guides: &[Guide]) -> Value {
    json!(
        guides
            .iter()
            .map(|g| json!({
                "axis": if g.axis == Axis::X { "x" } else { "y" },
                "at": g.at,
                "span": g.span,
                "source": g.source,
            }))
            .collect::<Vec<_>>()
    )
}

/// `delta`: `[dx, dy]` in the preview frame's px (a group move, ADR-9980).
fn delta_param(params: &Value) -> Result<[f32; 2], RpcError> {
    let bad = || {
        rpc(DesignerError::new(
            "bad_value",
            "delta is [dx, dy] in the preview frame's px",
        ))
    };
    let items = params.get("delta").and_then(Value::as_array).ok_or_else(bad)?;
    let values: Vec<f32> = items.iter().filter_map(Value::as_f64).map(|v| v as f32).collect();
    match values.as_slice() {
        &[dx, dy] if items.len() == 2 && values.iter().all(|v| v.is_finite()) => Ok([dx, dy]),
        _ => Err(bad()),
    }
}

fn rect_param(params: &Value) -> Result<[f32; 4], RpcError> {
    let bad = || {
        rpc(DesignerError::new(
            "bad_value",
            "rect is [x, y, width, height] in the preview frame's px (editor.ui_designer_preview reports rects)",
        ))
    };
    let items = params.get("rect").and_then(Value::as_array).ok_or_else(bad)?;
    let values: Vec<f32> = items.iter().filter_map(Value::as_f64).map(|v| v as f32).collect();
    match values.as_slice() {
        &[x, y, w, h] if values.iter().all(|v| v.is_finite()) => Ok([x, y, w, h]),
        _ => Err(bad()),
    }
}

fn tree(root: &UiNodeDef) -> Vec<Value> {
    let mut out = Vec::new();
    walk(root, &mut Vec::new(), &mut |node, path| {
        out.push(json!({
            "path": path_string(path),
            "depth": path.len(),
            "name": node.name,
            "kind": node_kind(node),
            "classes": node.classes,
            "bindings": node.bind.iter().count() + node.bindings.len(),
            "command": node.command,
            "children": node.children.len(),
            "repeat": node.repeat.as_ref().map(|r| r.source.as_str()),
            "in_template": path
                .split_last()
                .is_some_and(|(_, parent)| !enclosing_repeats(root, parent).is_empty()),
        }));
    });
    out
}

fn finding_json(f: &LintFinding) -> Value {
    json!({ "code": f.code, "severity": f.severity, "node": f.node, "message": f.message })
}

/// `editor.ui_designer_open`.
fn ui_designer_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let new = params.get("new").and_then(Value::as_bool).unwrap_or(false);
    let path = params.get("path").and_then(Value::as_str);
    let path = match path {
        Some(p) if new => Some(resolve_doc_path(world, p, ".ui.ron")?),
        Some(p) => Some(resolve_path(world, p)?),
        None => None,
    };
    let mut designer = world.get_resource_or_insert_with(UiDesigner::default);
    match (new, path) {
        (true, path) => {
            designer.new_document();
            if let Some(path) = path
                && let Ok(doc) = designer.doc_mut()
            {
                doc.path = Some(path);
            }
        }
        (false, Some(path)) => designer.open(&path).map_err(rpc)?,
        (false, None) => return Err(RpcError::invalid_params("pass `path` or `new: true`")),
    }
    let doc = designer
        .doc()
        .ok_or_else(|| rpc(DesignerError::new("no_document", "open failed")))?;
    let mut out = summary(doc);
    out["tree"] = json!(tree(&doc.file().root));
    if let Some(mut runtime) = world.get_non_send_mut::<crate::ui_designer_interactive::DesignerRuntime>() {
        runtime.stop();
    }
    Ok(out)
}

/// `editor.ui_designer_state`.
fn ui_designer_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let designer = world.get_resource_or_insert_with(UiDesigner::default);
    let Some(doc) = designer.doc() else {
        return Ok(json!({ "open": false, "presets": PRESETS, "sizes": size_names() }));
    };
    let mut out = summary(doc);
    out["tree"] = json!(tree(&doc.file().root));
    out["presets"] = json!(PRESETS);
    out["sizes"] = json!(size_names());
    let focus = match params.get("node").and_then(Value::as_str) {
        Some(text) => Some(resolve_node(&doc.file().root, text).map_err(rpc)?),
        None => doc.selection.clone(),
    };
    if let Some(path) = focus
        && let Some(node) = node_at(&doc.file().root, &path)
    {
        let mut full = node_json(node);
        if let Value::Object(map) = &mut full {
            map.remove("children");
        }
        out["node"] = json!({ "path": path_string(&path), "fields": full });
    }
    let selected = doc.selected_nodes();
    if selected.len() > 1 {
        // Several selected: the fields they share and the ones that differ (mixed).
        let (shared, mixed) = shared_fields(&doc.file().root, &selected);
        out["shared"] = json!(shared);
        out["mixed"] = json!(mixed);
    }
    Ok(out)
}

fn size_names() -> Vec<&'static str> {
    SIZE_PRESETS.iter().map(|(n, ..)| *n).collect()
}

/// `editor.ui_designer_edit`.
fn ui_designer_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // The clipboard is the designer's, not the document's: it survives opening another document.
    let mut clipboard = world
        .get_resource_or_insert_with(UiDesigner::default)
        .clipboard
        .take();
    let result = edit_document(world, params, &mut clipboard);
    world.resource_mut::<UiDesigner>().clipboard = clipboard;
    result
}

fn edit_document(
    world: &mut World,
    params: &Value,
    clipboard: &mut Option<String>,
) -> Result<Value, RpcError> {
    let op = params
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`op` is required"))?;
    let project = project_root(world);
    let doc = doc(world)?;
    let frame = |doc: &UiDesign| -> Result<PreviewFrame, RpcError> {
        let localization = project.as_deref().and_then(project_localization);
        preview(doc.file(), &doc.preview, localization).map_err(rpc)
    };
    let mut extra = serde_json::Map::new();
    let index = |key: &str| -> Result<Option<usize>, RpcError> {
        match params.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("`{key}` is a child index"))),
        }
    };
    let touched = match op {
        "anchor_preset" => {
            let node = node_param(doc, params, "node")?;
            let preset = params.get("preset").and_then(Value::as_str)
                .and_then(regia_ui::anchors::AnchorPreset::from_name)
                .ok_or_else(|| rpc(DesignerError::new("bad_value", "unknown anchor preset")))?;
            let keep = match params.get("keep_position") {
                None => true,
                Some(Value::Bool(v)) => *v,
                _ => return Err(rpc(DesignerError::new("bad_value", "keep_position must be a boolean"))),
            };
            let op = crate::ui_designer_canvas::anchor_preset_edit(
                doc.file(), &frame(doc)?, &node, preset, keep,
                |file| preview(file, &doc.preview, project.as_deref().and_then(project_localization)),
            ).map_err(rpc)?;
            doc.apply(op)
        }
        "add" => {
            // `parent: null` is the default parent, like an absent key.
            let parent = match params.get("parent") {
                Some(Value::Null) | None => doc.selection.clone().unwrap_or_default(),
                Some(_) => node_param(doc, params, "parent")?,
            };
            let node = match (params.get("preset").and_then(Value::as_str), params.get("node")) {
                (Some(name), None) => preset(name).ok_or_else(|| {
                    rpc(DesignerError::new(
                        "bad_value",
                        format!("unknown preset {name:?}; presets: {}", PRESETS.join(", ")),
                    ))
                })?,
                (None, Some(json)) => serde_json::from_value::<UiNodeDef>(json.clone())
                    .map_err(|e| rpc(DesignerError::new("bad_value", format!("node: {e}"))))?,
                _ => {
                    return Err(RpcError::invalid_params(
                        "add takes `preset` or `node` (one of them)",
                    ));
                }
            };
            doc.apply(DesignOp::Add {
                parent,
                index: index("index")?,
                node: Box::new(node),
            })
        }
        "remove" if params.get("nodes").is_some() => {
            let nodes = nodes_param(doc, params)?;
            doc.remove_many(&nodes)
        }
        "remove" => {
            let node = node_param(doc, params, "node")?;
            doc.apply(DesignOp::Remove(node))
        }
        "duplicate" if params.get("nodes").is_some() => {
            let nodes = nodes_param(doc, params)?;
            doc.duplicate_many(&nodes).map(|c| c.first().cloned().unwrap_or_default())
        }
        "duplicate" => {
            let node = node_param(doc, params, "node")?;
            doc.apply(DesignOp::Duplicate(node))
        }
        "move" => {
            let node = node_param(doc, params, "node")?;
            let parent = node_param(doc, params, "parent")?;
            doc.apply(DesignOp::Move {
                node,
                parent,
                index: index("index")?,
            })
        }
        "set" => {
            let key = params
                .get("key")
                .and_then(Value::as_str)
                .ok_or_else(|| RpcError::invalid_params("set needs `key` (e.g. \"style.size\")"))?;
            let value = params
                .get("value")
                .cloned()
                .ok_or_else(|| RpcError::invalid_params("set needs `value` (null resets the field)"))?;
            if key == "document.back" {
                let back = match &value {
                    Value::Null => None,
                    Value::String(text) => match text.as_str() {
                        "pop" | "Pop" => Some(ScreenBackDef::Pop),
                        "ignore" | "Ignore" => Some(ScreenBackDef::Ignore),
                        _ => {
                            return Err(rpc(DesignerError::new(
                                "bad_value",
                                "document.back is null, \"pop\" or \"ignore\"",
                            )));
                        }
                    },
                    _ => {
                        return Err(rpc(DesignerError::new(
                            "bad_value",
                            "document.back is null, \"pop\" or \"ignore\"",
                        )));
                    }
                };
                let mut file = doc.file().clone();
                file.back = back;
                let touched = doc.selection.clone().unwrap_or_default();
                doc.replace_as_step(file, touched.clone());
                Ok(touched)
            } else {
                let nodes = nodes_param(doc, params)?;
                if nodes.len() > 1 || params.get("nodes").is_some() {
                    // Several nodes: one undo step.
                    doc.set_many(&nodes, key, &value, false)
                        .map(|t| t.first().cloned().unwrap_or_default())
                } else {
                    let node = match nodes.into_iter().next() {
                        Some(node) => node,
                        None => node_param(doc, params, "node")?,
                    };
                    doc.apply(DesignOp::Set {
                        node,
                        key: key.to_owned(),
                        value,
                    })
                }
            }
        }
        "select_many" => {
            if !params.get("nodes").is_some_and(Value::is_array) {
                return Err(RpcError::invalid_params("select_many needs `nodes` (a list; the first is the primary)"));
            }
            let nodes = nodes_param(doc, params)?;
            doc.select_many(nodes.clone())
                .map(|()| nodes.first().cloned().unwrap_or_default())
        }
        // A group move (ADR-9980): every top-level node of `nodes` (or the selection) by `delta`.
        "transform" if params.get("delta").is_some() => {
            if params.get("rect").is_some_and(|v| !v.is_null()) {
                return Err(RpcError::invalid_params(
                    "transform takes `rect` (one node) or `delta` (a group move), not both",
                ));
            }
            let delta = delta_param(params)?;
            let nodes = nodes_param(doc, params)?;
            let laid_out = frame(&*doc)?;
            // In a layout variant, moves start from that variant's values (ADR-7858).
            let edited = crate::ui_designer_variants::edited_document(doc.file(), &doc.preview).into_owned();
            let plan = plan_group_move(&edited.root, &laid_out, &nodes, delta, &snap_param(params)?)
                .map_err(rpc)?;
            extra.insert("delta".into(), json!(plan.delta));
            extra.insert("rect".into(), json!(plan.rect));
            extra.insert("guides".into(), guides_json(&plan.guides));
            extra.insert("changed".into(), json!(!plan.ops.is_empty()));
            extra.insert(
                "moved".into(),
                json!(plan.nodes.iter().map(|p| path_string(p)).collect::<Vec<_>>()),
            );
            doc.move_many(plan, &nodes, None)
                .map(|()| nodes.first().cloned().unwrap_or_default())
        }
        "transform" => {
            // The rect form moves one `node`; a `nodes` list beside it would be ignored.
            if params.get("nodes").is_some_and(|v| !v.is_null()) {
                return Err(RpcError::invalid_params(
                    "transform with `rect` moves one `node`; `nodes` takes `delta`",
                ));
            }
            let node = node_param(doc, params, "node")?;
            let mode = match params.get("mode").and_then(Value::as_str).unwrap_or("move") {
                "move" => TransformMode::Move,
                "resize" => TransformMode::Resize,
                other => {
                    return Err(rpc(DesignerError::new(
                        "bad_value",
                        format!("mode {other:?}: move or resize"),
                    )));
                }
            };
            let request = TransformRequest {
                node: &node,
                rect: rect_param(params)?,
                mode,
                snap: snap_param(params)?,
                keep_aspect: params.get("keep_aspect").and_then(Value::as_bool).unwrap_or(false),
                reorder: params.get("reorder").and_then(Value::as_bool).unwrap_or(true),
            };
            let laid_out = frame(&*doc)?;
            let edited = crate::ui_designer_variants::edited_document(doc.file(), &doc.preview).into_owned();
            let plan = plan_transform(&edited.root, &laid_out, &request).map_err(rpc)?;
            extra.insert("rect".into(), json!(plan.rect));
            extra.insert("writes".into(), json!(plan.writes));
            extra.insert("guides".into(), guides_json(&plan.guides));
            extra.insert("changed".into(), json!(plan.op.is_some()));
            match plan.op {
                Some(op) => doc.apply(op),
                None => Ok(node),
            }
        }
        "align" => {
            let name = params.get("align").and_then(Value::as_str).unwrap_or_default();
            let kind = AlignKind::parse(name).ok_or_else(|| {
                rpc(DesignerError::new(
                    "bad_value",
                    format!(
                        "align {name:?}: one of {}",
                        AlignKind::ALL.map(AlignKind::name).join(", ")
                    ),
                ))
            })?;
            let nodes = nodes_param(doc, params)?;
            let laid_out = frame(&*doc)?;
            doc.align(&laid_out, &nodes, kind)
                .map(|()| nodes.first().cloned().unwrap_or_default())
        }
        "group" => {
            let nodes = nodes_param(doc, params)?;
            doc.group(&nodes, params.get("name").and_then(Value::as_str))
        }
        "copy" | "cut" => {
            let nodes = nodes_param(doc, params)?;
            let text = copy_text(&doc.file().root, &nodes).map_err(rpc)?;
            extra.insert("clipboard".into(), json!(text));
            *clipboard = Some(text);
            if op == "cut" {
                doc.remove_many(&nodes)
            } else {
                Ok(doc.selection.clone().unwrap_or_default())
            }
        }
        "paste" => {
            let text = match params.get("text").and_then(Value::as_str) {
                Some(text) => text.to_owned(),
                None => clipboard.clone().ok_or_else(|| {
                    rpc(DesignerError::new(
                        "bad_value",
                        "nothing to paste: copy first, or pass `text` (node JSON)",
                    ))
                })?,
            };
            let (parent, index) = match params.get("parent") {
                Some(Value::Null) | None => {
                    let (parent, at) = paste_target(doc);
                    (parent, index("index")?.or(at))
                }
                Some(_) => (node_param(doc, params, "parent")?, index("index")?),
            };
            doc.paste(&text, &parent, index)
                .map(|p| p.first().cloned().unwrap_or_default())
        }
        "repeat" => {
            let node = node_param(doc, params, "node")?;
            let text = |key: &str| params.get(key).and_then(Value::as_str);
            // `row_height` (ADR-7751): `list` or `template`; absent keeps the node's.
            let row_height = match params.get("row_height") {
                None | Some(Value::Null) => node_at(&doc.file().root, &node)
                    .and_then(|n| n.repeat.as_ref())
                    .map_or(RepeatRowHeight::List, |r| r.row_height),
                Some(value) => serde_json::from_value::<RepeatRowHeight>(value.clone()).map_err(|_| {
                    rpc(DesignerError::new("bad_value", "`row_height` is list or template"))
                })?,
            };
            doc.apply(repeat_op_with(node, text("source"), text("key"), row_height))
        }
        // Where a binding reads from (ADR-7751): exactly one of `source`, `theme`, `animation`.
        "bind_source" => {
            let node = node_param(doc, params, "node")?;
            let given: Vec<&str> = ["source", "theme", "animation"]
                .into_iter()
                .filter(|k| params.get(*k).is_some_and(|v| !v.is_null()))
                .collect();
            let edit = match given.as_slice() {
                ["source"] => params
                    .get("source")
                    .and_then(Value::as_str)
                    .map(|s| BindSourceEdit::Field(s.trim().to_owned())),
                ["theme"] => params
                    .get("theme")
                    .and_then(Value::as_str)
                    .map(|s| BindSourceEdit::Theme(s.trim().to_owned())),
                ["animation"] => params
                    .get("animation")
                    .and_then(|v| serde_json::from_value::<AnimationBindDef>(v.clone()).ok())
                    .map(BindSourceEdit::Animation),
                _ => None,
            };
            let Some(edit) = edit else {
                return Err(rpc(DesignerError::new(
                    "bad_value",
                    "bind_source takes exactly one of `source` (\"Model.field\" or \".field\"), `theme` (a token such as \"color.accent\") or `animation` ({\"from\": 0, \"to\": 1, \"duration\": 0.8, \"delay\": 0, \"ease\": \"quad_in_out\", \"repeat\": \"once\" | \"loop\" | \"ping_pong\"})",
                )));
            };
            bind_source_op(&doc.file().root, &node, index("binding")?.unwrap_or(0), edit).and_then(|op| doc.apply(op))
        }
        "converter" => {
            let node = node_param(doc, params, "node")?;
            let converter = match params.get("converter") {
                None => {
                    return Err(RpcError::invalid_params(
                        "converter needs `converter` (null clears it)",
                    ));
                }
                Some(Value::Null) => None,
                Some(value) => Some(serde_json::from_value::<BindConverter>(value.clone()).map_err(|e| {
                    rpc(DesignerError::new(
                        "bad_value",
                        format!(
                            "converter: {e}; one of {} (e.g. {{\"clamp\": {{\"min\": 0, \"max\": 1}}}})",
                            BindConverter::NAMES.join(", ")
                        ),
                    ))
                })?),
            };
            converter_op(&doc.file().root, &node, index("binding")?.unwrap_or(0), converter)
                .and_then(|op| doc.apply(op))
        }
        "select" => {
            let node = match params.get("node") {
                None | Some(Value::Null) => None,
                Some(_) => Some(node_param(doc, params, "node")?),
            };
            doc.select(node.clone()).map(|()| node.unwrap_or_default())
        }
        "undo" => doc.undo().map(|()| Vec::new()),
        "redo" => doc.redo().map(|()| Vec::new()),
        // A picture by asset path, icon or id (ADR-3148): one `set` of the whole field.
        "image" | "mask" => {
            let node = node_param(doc, params, "node")?;
            let current = node_at(&doc.file().root, &node)
                .ok_or_else(|| rpc(DesignerError::new("bad_node", "no such node")))?;
            let value = if op == "image" {
                crate::ui_designer_images::edit_image(current.image.as_ref(), params)
                    .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
            } else {
                crate::ui_designer_images::edit_mask(current.mask.as_ref(), params)
                    .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
            }
            .map_err(rpc)?;
            doc.apply(DesignOp::Set {
                node,
                key: op.to_owned(),
                value,
            })
        }
        other => {
            return Err(rpc(DesignerError::new(
                "bad_op",
                format!(
                    "unknown op {other:?}: anchor_preset, add, remove, move, duplicate, set, repeat, converter, bind_source, image, mask, select, select_many, transform, align, group, copy, cut, paste, undo, redo"
                ),
            )));
        }
    }
    .map_err(rpc)?;
    let mut out = summary(doc);
    out["node"] = json!(path_string(&touched));
    for (k, v) in extra {
        out[k] = v;
    }
    Ok(out)
}

/// `editor.ui_designer_images` (ADR-3148).
fn ui_designer_images(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world);
    let icons: Option<Vec<String>> = world
        .get_resource::<regia_ui::icons::IconAtlas>()
        .map(|a| a.names().into_iter().map(str::to_owned).collect());
    let doc = doc(world)?;
    let mut out = crate::ui_designer_images::image_report(doc.file(), root.as_deref(), icons.as_deref());
    out["revision"] = json!(doc.revision());
    out["project"] = json!(root.is_some());
    Ok(out)
}

/// `editor.ui_designer_lint`.
fn ui_designer_lint(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let models = world.get_resource::<ViewModels>().cloned();
    let doc = doc(world)?;
    let findings = lint(doc.file(), models.as_ref());
    Ok(json!({
        "schema": models.is_some(),
        "errors": findings.iter().filter(|f| f.severity == "error").count(),
        "findings": findings.iter().map(finding_json).collect::<Vec<_>>(),
    }))
}

/// Reads `size`, `safe_area`, `locale`, `pseudo`, `samples`, `repeat_count` and `theme` into the
/// document's settings (its view state: nothing here is written to the file).
fn apply_preview_params(doc: &mut UiDesign, params: &Value) -> Result<(), RpcError> {
    let mut settings = doc.preview.clone();
    if let Some(value) = params.get("scale") {
        settings.scale = if value.is_null() {
            None
        } else {
            Some(
                value
                    .as_f64()
                    .ok_or_else(|| rpc(DesignerError::new("bad_value", "scale must be a number or null")))?
                    as f32,
            )
        };
    }
    if let Some(value) = params.get("dpi") {
        settings.dpi = value
            .as_f64()
            .ok_or_else(|| rpc(DesignerError::new("bad_value", "dpi must be a number")))?
            as f32;
    }
    if let Some(value) = params.get("scale_mode") {
        settings.scale_mode = serde_json::from_value(value.clone())
            .map_err(|e| rpc(DesignerError::new("bad_value", format!("scale_mode: {e}"))))?;
    }
    if let Some(value) = params.get("box_model") {
        settings.box_model = value
            .as_bool()
            .ok_or_else(|| rpc(DesignerError::new("bad_value", "box_model must be a boolean")))?;
    }
    match params.get("size") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) => settings.use_preset(name).map_err(rpc)?,
        Some(Value::Array(a)) if a.len() == 2 => {
            let n = |v: &Value| v.as_f64().map(|f| f as f32);
            match (n(&a[0]), n(&a[1])) {
                (Some(w), Some(h)) => settings.size = [w, h],
                _ => return Err(rpc(DesignerError::new("bad_size", "size is [width, height]"))),
            }
        }
        Some(_) => {
            return Err(rpc(DesignerError::new(
                "bad_size",
                "size is [width, height] or a preset name",
            )));
        }
    }
    if let Some(v) = params.get("safe_area").filter(|v| !v.is_null()) {
        let insets: Option<Vec<f32>> = v
            .as_array()
            .filter(|a| a.len() == 4)
            .map(|a| a.iter().filter_map(|x| x.as_f64().map(|f| f as f32)).collect());
        match insets.as_deref() {
            Some(&[l, t, r, b]) => settings.safe_area = [l, t, r, b],
            _ => {
                return Err(rpc(DesignerError::new(
                    "bad_size",
                    "safe_area is [left, top, right, bottom]",
                )));
            }
        }
    }
    if let Some(v) = params.get("locale") {
        settings.locale = v.as_str().map(str::to_owned);
    }
    // The themed preview's theme (ADR-7677): a name sticks to the document like the other settings,
    // null goes back to the active theme.
    match params.get("theme") {
        None => {}
        Some(Value::Null) => settings.theme = None,
        Some(Value::String(name)) => settings.theme = Some(name.clone()),
        Some(_) => return Err(RpcError::invalid_params("`theme` must be a theme name")),
    }
    if let Some(v) = params.get("pseudo").and_then(Value::as_bool) {
        settings.pseudo = v;
    }
    if let Some(v) = params.get("repeat_count").filter(|v| !v.is_null()) {
        let Some(count) = v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n <= MAX_REPEAT_COUNT)
        else {
            return Err(rpc(DesignerError::new(
                "bad_value",
                format!("repeat_count is an item count up to {MAX_REPEAT_COUNT}"),
            )));
        };
        settings.repeat_count = count;
    }
    if let Some(v) = params.get("samples") {
        let Some(map) = v.as_object() else {
            return Err(RpcError::invalid_params(
                "`samples` maps a binding source to a value",
            ));
        };
        settings.samples = map
            .iter()
            .map(|(k, v)| {
                let text = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                (k.clone(), text)
            })
            .collect();
    }
    settings.validate().map_err(rpc)?;
    doc.preview = settings;
    Ok(())
}

fn frame_json(frame: &PreviewFrame, only_visible: bool) -> Value {
    let nodes: Vec<Value> = frame
        .nodes
        .iter()
        .filter(|n| !only_visible || n.visible)
        .map(|n| {
            json!({
                "path": n.path,
                "item": n.item,
                "name": n.name,
                "kind": n.kind,
                "rect": n.rect,
                "clip": n.clip,
                "visible": n.visible, "layout_dir": if n.rtl { "rtl" } else { "ltr" },
                "text": n.text,
                "box_model_bands": n.box_model_bands,
                "outside_safe_area": n.outside_safe_area,
            })
        })
        .collect();
    let outside: Vec<&str> = frame
        .nodes
        .iter()
        .filter(|n| n.outside_safe_area)
        .map(|n| n.path.as_str())
        .collect();
    let lists: Vec<Value> = frame
        .lists
        .iter()
        .map(|l| {
            json!({
                "path": l.path,
                "items": l.items,
                "row_height": l.row_height,
                "materialized": l.materialized,
                "first_visible_key": l.first_visible_key,
            })
        })
        .collect();
    json!({
        "size": frame.size,
        "safe_area": frame.safe_area,
        "scale": frame.scale,
        "outside_safe_area": outside,
        "truncated": frame.truncated,
        "nodes": nodes,
        "lists": lists,
    })
}

/// `editor.ui_designer_preview`.
fn ui_designer_preview(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let root = project_root(world);
    let choice = runtime_theme_choice(world);
    let mut runtime = world
        .remove_non_send::<crate::ui_designer_interactive::DesignerRuntime>()
        .unwrap_or_default();
    let result = (|| {
        let doc = doc(world)?;
        apply_preview_params(doc, params)?;
        if let Some(root) = &root {
            doc.preview.assets_root = Some(root.join("assets"));
        }
        if let Some(value) = params.get("interactive") {
            let on = value
                .as_bool()
                .ok_or_else(|| rpc(DesignerError::new("bad_value", "interactive is a boolean")))?;
            if on {
                runtime.interactive = true;
            } else {
                runtime.stop();
            }
        }
        let localization = root.as_deref().and_then(project_localization);
        let mut commands = Vec::new();
        let mut samples = doc.preview.samples.clone();
        let frame = if runtime.interactive {
            runtime.ensure(doc, &choice, || localization).map_err(rpc)?;
            let session = runtime
                .session
                .as_mut()
                .ok_or_else(|| rpc(DesignerError::new("parse", "preview did not start")))?;
            if let Some(input) = params.get("input") {
                session.input(input).map_err(rpc)?;
            }
            commands = session.commands();
            samples = session.samples();
            session.frame()
        } else {
            if params.get("input").is_some() {
                return Err(rpc(DesignerError::new(
                    "bad_value",
                    "start interactive preview before sending input",
                )));
            }
            preview(doc.file(), &doc.preview, localization).map_err(rpc)?
        };
        let only_visible = params
            .get("visible_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let mut out = frame_json(&frame, only_visible);
        out["scale_override"] = json!(doc.preview.scale);
        out["scale_mode"] = json!(doc.preview.scale_mode.unwrap_or_default());
        out["dpi"] = json!(doc.preview.dpi);
        out["box_model"] = json!(doc.preview.box_model);
        out["locale"] = json!(doc.preview.locale);
        out["pseudo"] = json!(doc.preview.pseudo);
        out["theme"] = json!(doc.preview.theme);
        out["revision"] = json!(doc.revision());
        out["interactive"] = json!(runtime.interactive);
        out["commands"] = json!(commands);
        out["samples"] = json!(samples);
        Ok(out)
    })();
    world.insert_non_send(runtime);
    result
}

fn runtime_theme_choice(world: &World) -> ThemeChoice {
    let root = project_root(world);
    ThemeChoice {
        themes: world.get_resource::<regia_ui::UiThemes>().cloned(),
        active: world.get_resource::<regia_ui::ActiveTheme>().cloned(),
        files: root.as_deref().map(project_theme_files).unwrap_or_default(),
        project: root,
        theme: None,
    }
}

fn ui_designer_style_sources(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let choice = runtime_theme_choice(world);
    let mut runtime = world
        .remove_non_send::<crate::ui_designer_interactive::DesignerRuntime>()
        .unwrap_or_default();
    let result = (|| {
        let doc = doc(world)?;
        let path = match params.get("node") {
            Some(Value::String(node)) => path_string(&resolve_node(&doc.file().root, node).map_err(rpc)?),
            None => path_string(&doc.selection.clone().unwrap_or_default()),
            _ => return Err(rpc(DesignerError::new("bad_node", "node is a path or #name"))),
        };
        runtime
            .ensure(doc, &choice, || {
                choice.project.as_deref().and_then(project_localization)
            })
            .map_err(rpc)?;
        runtime
            .session
            .as_mut()
            .ok_or_else(|| rpc(DesignerError::new("parse", "preview did not start")))?
            .inspect(&path)
            .map_err(rpc)
    })();
    world.insert_non_send(runtime);
    let out = result?;
    if let Some(token) = params.get("open_token") {
        let token = token
            .as_str()
            .ok_or_else(|| rpc(DesignerError::new("bad_value", "open_token is a token name")))?;
        let present = out["properties"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|p| p["sources"].as_array().into_iter().flatten())
            .flat_map(|s| s["tokens"].as_array().into_iter().flatten())
            .any(|t| t.as_str() == Some(token));
        if !present {
            return Err(rpc(DesignerError::new(
                "bad_value",
                "the node has no such theme source",
            )));
        }
        crate::ui_designer_interactive::open_theme_source(
            world,
            &choice,
            out["theme"]["theme"].as_str().unwrap_or_default(),
            token,
        )
        .map_err(rpc)?;
    }
    Ok(out)
}

/// `editor.ui_designer_render`.
fn ui_designer_render(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    doc(world)?;
    let root = project_root(world);
    // Checked before any device is made; the name itself goes to the document's settings
    // (`apply_preview_params`), where the designer's theme picker puts it too (ADR-7677).
    if params
        .get("theme")
        .is_some_and(|v| !v.is_null() && !v.is_string())
    {
        return Err(RpcError::invalid_params("`theme` must be a theme name"));
    }
    let choice = ThemeChoice {
        themes: world.get_resource::<regia_ui::UiThemes>().cloned(),
        active: world.get_resource::<regia_ui::ActiveTheme>().cloned(),
        files: root.as_deref().map(project_theme_files).unwrap_or_default(),
        theme: None,
        project: root.clone(),
    };
    // A headless editor without an adapter answers `editor_headless`, like its view methods, instead
    // of the designer opening a device of its own (ADR-5180).
    if world.get_resource::<regia_render::RenderDevice>().is_none()
        && let Some(err) = crate::headless::no_adapter_error(world)
    {
        return Err(err);
    }
    let device = preview_device(world).map_err(rpc)?;
    let mut runtime = world
        .remove_non_send::<crate::ui_designer_interactive::DesignerRuntime>()
        .unwrap_or_default();
    let result = (|| {
        let doc = doc(world)?;
        let theme_before = doc.preview.theme.clone();
        apply_preview_params(doc, params)?;
        if let Some(root) = &root {
            doc.preview.assets_root = Some(root.join("assets"));
        }
        let localization = root.as_deref().and_then(project_localization);
        let rendered = if runtime.interactive {
            runtime.ensure(doc, &choice, || localization).and_then(|()| {
                runtime
                    .session
                    .as_mut()
                    .ok_or_else(|| DesignerError::new("render_failed", "interactive runtime is unavailable"))?
                    .render(&device)
            })
        } else {
            themed_preview(doc.file(), &doc.preview, localization, &choice, &device)
        };
        let frame = match rendered {
            Ok(frame) => frame,
            Err(e) => {
                doc.preview.theme = theme_before;
                return Err(rpc(e));
            }
        };
        let png = crate::render::viewport_png_base64(&frame.to_color_image()).map_err(RpcError::internal)?;
        Ok(json!({
            "width": frame.width, "height": frame.height, "theme": frame.theme,
            "theme_errors": frame.theme_errors, "drawn_pixels": frame.drawn_pixels(),
            "mime": "image/png", "png_base64": png, "revision": doc.revision(),
            "interactive": runtime.interactive,
        }))
    })();
    world.insert_non_send(runtime);
    result
}

/// `editor.ui_designer_save`.
fn ui_designer_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match params.get("path").and_then(Value::as_str) {
        Some(p) => Some(resolve_doc_path(world, p, ".ui.ron")?),
        None => None,
    };
    let canonical = params.get("canonical").and_then(Value::as_bool).unwrap_or(false);
    let doc = doc(world)?;
    let saved = doc.save_with(path, canonical).map_err(rpc)?;
    let mut out = summary(doc);
    out["saved"] = json!(saved.display().to_string());
    Ok(out)
}

const NODE: ParamSchema = ParamSchema {
    name: "node",
    kind: "string",
    required: false,
    description: "A node path (\"/\" root, \"/0/2\") or \"#name\".",
};

/// The `editor.ui_designer_*` methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.ui_designer_open",
                tool: "ui_designer_open",
                description: "Open a .ui.ron file in the UI designer (older format versions are migrated in \
                    memory; the file is not touched until a save), or start an empty document with new: true. \
                    Replaces the open document.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The .ui.ron file (relative to the project root, or absolute). With new: true, where it will save.",
                    },
                    ParamSchema {
                        name: "new",
                        kind: "boolean",
                        required: false,
                        description: "Start an empty document instead of reading `path`.",
                    },
                ],
                result: "{ open, path, version, opened_version, dirty, revision, undo, redo, had_comments, last_save_canonical, comments_lost, selection, tree: [{ path, depth, name, kind, classes, bindings, command, children, repeat, in_template }] }",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_designer_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_state",
                tool: "ui_designer_state",
                description: "Read the UI designer: the open document (path, dirty, undo/redo counts), its node \
                    tree with paths, and one node's fields in full (the selection, or `node`). Fields use the \
                    .ui.ron names; absent fields hold their defaults.",
                params: &[NODE],
                result: "{ open, path, version, dirty, revision, undo, redo, selection, selected, tree, presets, sizes, node: { path, fields }, shared?, mixed? }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_designer_state as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_edit",
                tool: "ui_designer_edit",
                description: "Edit the open UI document. Each mutating op is one undo step. select only changes the \
                    selection and does not push a step; undo and redo move the existing stack. A failed op changes \
                    nothing. anchor_preset {node, preset, keep_position?} preserves the measured rectangle by default; false snaps with zero offsets. op: add {parent?, index?, preset | node}, remove {node}, move {node, parent, index?}, \
                    duplicate {node}, set {node, key, value} (key is a dotted .ui.ron field such as style.size, \
                    bindings, command, widget, text; value is JSON in the .ui.ron shape; null resets), \
                    repeat {node, source, key?} (the node's children become a template spawned per item of the \
                    list at source; no source removes it), converter {node, binding?, converter} (sets one \
                    binding's converter; null clears it), image {node, asset | icon | id, nine_slice?, \
                    preserve_aspect?, remove?} (names the node's picture by manifest path, icon-atlas name or \
                    renderer id; nine_slice [l, t, r, b] source px, null removes; remove: true drops the image), \
                    mask {node, asset | icon | id, fit?, remove?} (the same for the group mask), select {node?}, \
                    undo, redo. Several nodes (ADR-9730): select_many {nodes} (the first is the primary); set, \
                    remove and duplicate take `nodes` (or work on a multi-selection) as one step; align {nodes?, \
                    align} lines the nodes' laid-out rects up (left, center, right, top, middle, bottom) or spaces \
                    them evenly (distribute_h, distribute_v); group {nodes?, name?} wraps siblings in a new panel. \
                    transform {node, rect, mode?, snap?, keep_aspect?, reorder?} moves or resizes a node to a rect \
                    of the preview frame: an absolutely placed node writes style.inset and style.size, a node in \
                    the flow style.margin and style.size, and a move over a sibling reorders it; snap (when given) to the \
                    parent's edges, the siblings and a grid. transform {nodes?, delta, snap?} (ADR-9980) moves \
                    every top-level node of nodes (default: the selection) by delta [dx, dy] frame px as one step, \
                    snapping the group's bounding rect; it answers the snapped delta, the group rect and the \
                    guides. copy {nodes?} and cut {nodes?} put the nodes' JSON \
                    on the designer's clipboard (kept across documents); paste {text?, parent?, index?} inserts \
                    it (after the selection by default) with unique names. Typed widget fields: set \
                    {key: \"widget.<field>\"} (widget.max of a slider, widget.options of a dropdown, \
                    widget.max_chars, widget.secret, widget.read_only and widget.filter of a text input; \
                    book/agents/ui-designer.md lists every one); image.fit (stretch, contain, cover) through set. \
                    Containers (ADR-7827): split (widget.axis, ratio, min_ratio, max_ratio; children are the two \
                    panes), foldable (widget.title, open, group), tabs (widget.tabs, active; child i is the page of \
                    tab i); text.markup (true parses <b>, <i>, <u>, <s>, <color=#..>, <size=..>, <icon=..>); \
                    nav.up, nav.down, nav.left, nav.right name a focus neighbor by node name (null clears). \
                    Screen fields (ADR-8252): set {key: \"disabled_subtree\", value: true}, set \
                    {key: \"pivot\", value: [0, 1]}, set {key: \"document.back\", value: \"pop\" | \
                    \"ignore\" | null} (the file's back policy; null leaves the stack default).",
                params: &[
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: true,
                        description: "anchor_preset, add, remove, move, duplicate, set, repeat, converter, bind_source, image, mask, select, select_many, transform, align, group, copy, cut, paste, undo or redo.",
                    },
                    NODE,
                    ParamSchema {
                        name: "parent",
                        kind: "string",
                        required: false,
                        description: "add, move, paste: the new parent (add defaults to the selection, paste to the selection's parent).",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "number",
                        required: false,
                        description: "add, move, paste: position among the parent's children (default: last; paste: after the selection).",
                    },
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "add: panel, text, button, toggle, slider, progress, text_input, dropdown, image, list, scroll, lobby_list, scoreboard, radar, minimap, marker_layer, split, foldable or tabs. anchor_preset: one of the runtime AnchorPreset names (top_left, center, full_rect, etc.).",
                    },
                    ParamSchema {
                        name: "key",
                        kind: "string",
                        required: false,
                        description: "set: dotted field path (style.padding, bindings, command, text, widget, ...). repeat: the path of each item's stable key inside the item (\"id\", \".\" for the item itself); without one items follow by position.",
                    },
                    ParamSchema {
                        name: "keep_position",
                        kind: "boolean",
                        required: false,
                        description: "anchor_preset: preserve the measured rectangle (default true); false snaps with zero offsets.",
                    },
                    ParamSchema {
                        name: "source",
                        kind: "string",
                        required: false,
                        description: "repeat: the list, \"Model.field\", or \".field\" of the enclosing repeat's item; absent or null removes the repeat. bind_source: the field the binding reads (clears theme and animation).",
                    },
                    ParamSchema {
                        name: "row_height",
                        kind: "string",
                        required: false,
                        description: "repeat on a list: list (the list's row height) or template (the first template node's fixed height, ADR-7751); absent keeps the node's.",
                    },
                    ParamSchema {
                        name: "theme",
                        kind: "string",
                        required: false,
                        description: "bind_source: a token of the active theme (\"color.accent\"); the value follows a theme switch (ADR-7751).",
                    },
                    ParamSchema {
                        name: "animation",
                        kind: "object",
                        required: false,
                        description: "bind_source: {\"from\": 0, \"to\": 1, \"duration\": 0.8, \"delay\": 0, \"ease\": \"quad_in_out\", \"repeat\": \"once\" | \"loop\" | \"ping_pong\"} driven by the game clock (ADR-7751).",
                    },
                    ParamSchema {
                        name: "binding",
                        kind: "number",
                        required: false,
                        description: "converter, bind_source: which binding of the node (the single `bind` first, then `bindings`; default 0).",
                    },
                    ParamSchema {
                        name: "converter",
                        kind: "any",
                        required: false,
                        description: "converter: JSON in the .ui.ron shape: {\"number\": {\"precision\": 2, \"group\": \",\"}}, {\"clamp\": {\"min\": 0, \"max\": 1}}, {\"map_range\": {\"from\": [0, 100], \"to\": [0, 1], \"clamp\": true}}, \"bool_to_visibility\", {\"enum_text\": {\"table\": [[\"Idle\", \"Waiting\"]], \"default\": \"?\"}}, {\"loc_key\": {\"prefix\": \"state-\"}}; null clears it.",
                    },
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: false,
                        description: "set: the JSON value (e.g. [200, \"Auto\"] for style.size); null resets.",
                    },
                    ParamSchema {
                        name: "asset",
                        kind: "string",
                        required: false,
                        description: "image, mask: manifest path of a picture (\"assets/ui/panel.png\"; editor.ui_designer_images lists the project's).",
                    },
                    ParamSchema {
                        name: "icon",
                        kind: "string",
                        required: false,
                        description: "image, mask: name of an icon in the icon atlas (without @size).",
                    },
                    ParamSchema {
                        name: "id",
                        kind: "number",
                        required: false,
                        description: "image, mask: renderer image id (for pictures game code registers).",
                    },
                    ParamSchema {
                        name: "nine_slice",
                        kind: "any",
                        required: false,
                        description: "image: [left, top, right, bottom] insets in source px (the image then fills the node as a nine-slice background); null removes.",
                    },
                    ParamSchema {
                        name: "preserve_aspect",
                        kind: "boolean",
                        required: false,
                        description: "image: keep the picture's aspect ratio.",
                    },
                    ParamSchema {
                        name: "fit",
                        kind: "string",
                        required: false,
                        description: "image: stretch, contain, cover or tile (repeat at native size, ADR-7751); null leaves `fit` unwritten (preserve_aspect decides). mask: stretch, contain, cover or tile.",
                    },
                    ParamSchema {
                        name: "remove",
                        kind: "boolean",
                        required: false,
                        description: "image, mask: true removes the field.",
                    },
                    ParamSchema {
                        name: "nodes",
                        kind: "array",
                        required: false,
                        description: "select_many, set, remove, duplicate, align, group, copy, cut, transform with delta: node paths or \"#name\"s (default: the selection).",
                    },
                    ParamSchema {
                        name: "rect",
                        kind: "array",
                        required: false,
                        description: "transform: [x, y, width, height] in the preview frame's px (a move uses x and y).",
                    },
                    ParamSchema {
                        name: "delta",
                        kind: "array",
                        required: false,
                        description: "transform: [dx, dy] in the preview frame's px; moves every top-level node of `nodes` (or the selection) by it as one step (ADR-9980).",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "transform: move (default) or resize.",
                    },
                    ParamSchema {
                        name: "snap",
                        kind: "any",
                        required: false,
                        description: "transform: true (the parent's edges, the siblings and an 8 px grid within 6 px), {edges, siblings, grid, threshold}, or false (default: the rect is taken as given).",
                    },
                    ParamSchema {
                        name: "keep_aspect",
                        kind: "boolean",
                        required: false,
                        description: "transform: a resize keeps the node's aspect ratio.",
                    },
                    ParamSchema {
                        name: "reorder",
                        kind: "boolean",
                        required: false,
                        description: "transform: a move of a node in the flow over a sibling reorders it (default true); false writes its margin.",
                    },
                    ParamSchema {
                        name: "align",
                        kind: "string",
                        required: false,
                        description: "align: left, center, right, top, middle, bottom, distribute_h or distribute_v.",
                    },
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "group: the new panel's name (made unique).",
                    },
                    ParamSchema {
                        name: "text",
                        kind: "string",
                        required: false,
                        description: "paste: node JSON (one node or a list) instead of the designer's clipboard.",
                    },
                ],
                result: "{ open, path, dirty, revision, undo, redo, selection, selected, node, rect?, writes?, guides?, changed?, clipboard? }",
                mutating: true,
                permission: Permission::EditScene,
            },
            ui_designer_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_lint",
                tool: "ui_designer_lint",
                description: "Lint the open UI document: duplicate names, binding sources that are not \
                    Model.field, targets that do not fit the node, two_way on read-only targets, commands on \
                    nodes that get no clicks, root fields spawn_ui ignores, repeat sources, keys and templates \
                    (repeat_*), item bindings outside a repeat, converter settings and targets (converter_*), \
                    and (when the game registered view models) unknown models and fields.",
                params: &[],
                result: "{ schema, errors, findings: [{ code, severity, node, message }] }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_designer_lint as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_images",
                tool: "ui_designer_images",
                description: "List the open UI document's pictures (ADR-3148): every image and mask with its node \
                    path and name, whether it names an asset path, an icon or a renderer id, and found (the \
                    project has the file, the icon atlas has the icon; null when that cannot be told), the \
                    distinct asset paths (what the cook packs), how many are missing (regia assets validate \
                    reports them as ui_image_missing), and the picker lists: the project's pictures under \
                    assets/ and the icon atlas names.",
                params: &[],
                result: "{ references: [{ node, name, field, kind, value, found }], assets, missing, pictures, icons, project, revision }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_designer_images as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_preview",
                tool: "ui_designer_preview",
                description: "Lay the open UI document out with the runtime layout at a window size and safe area \
                    and return every node's rect (physical px), its shown text (localized spans resolved in \
                    `locale`, optionally pseudo-localized) and whether it crosses the safe area. Sample values \
                    stand in for view-model data; a repeat node shows repeat_count sample items, its rows \
                    reported with their template's path and an item index. A repeat on a list widget is \
                    virtualized: `lists` says how many rows the list materializes at the top of its scroll \
                    range and its first visible key; rows past that are not visible. Settings stick to the document.",
                params: &[
                    ParamSchema {
                        name: "interactive",
                        kind: "boolean",
                        required: false,
                        description: "Start or stop the private sample runtime; no authored document changes.",
                    },
                    ParamSchema {
                        name: "input",
                        kind: "object",
                        required: false,
                        description: "Interactive input: {action: click|focus|type|set_value|list|scroll_to, node: path or #name, ...runtime parameters}; type selects all and submits text.",
                    },
                    ParamSchema {
                        name: "size",
                        kind: "any",
                        required: false,
                        description: "[width, height] or a preset: 1920x1080, 1280x720, 1280x800, 2400x1080, 1080x2400 (presets set their insets).",
                    },
                    ParamSchema {
                        name: "safe_area",
                        kind: "array",
                        required: false,
                        description: "[left, top, right, bottom] insets in px.",
                    },
                    ParamSchema {
                        name: "locale",
                        kind: "string",
                        required: false,
                        description: "Locale of the project's Fluent files to resolve localized spans in.",
                    },
                    ParamSchema {
                        name: "pseudo",
                        kind: "boolean",
                        required: false,
                        description: "Pseudo-localize resolved text.",
                    },
                    ParamSchema {
                        name: "samples",
                        kind: "object",
                        required: false,
                        description: "Binding source -> sample value, e.g. {\"HudVm.health\": 0.4}. Under a repeat: \"Vm.items[].name\" (every item), \"Vm.items[1].name\" (one item), \"Vm.items[]#count\" (that repeat's item count).",
                    },
                    ParamSchema {
                        name: "repeat_count",
                        kind: "number",
                        required: false,
                        description: "Sample items each repeat node shows (default 3, at most 256).",
                    },
                    ParamSchema {
                        name: "visible_only",
                        kind: "boolean",
                        required: false,
                        description: "Leave out hidden nodes.",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "number",
                        required: false,
                        description: "Manual UI scale, 0.05..=16; null uses the rule (view state only).",
                    },
                    ParamSchema {
                        name: "scale_mode",
                        kind: "any",
                        required: false,
                        description: "Preview rule in ScaleMode JSON form; null uses the runtime default. Never saved to the document.",
                    },
                    ParamSchema {
                        name: "dpi",
                        kind: "number",
                        required: false,
                        description: "Simulated display DPI, 24..=1536 (default 96), for ConstantPhysicalSize and the project's DPI curve.",
                    },
                    ParamSchema {
                        name: "box_model",
                        kind: "boolean",
                        required: false,
                        description: "Show selected-node margin and padding bands on the designer canvas (view state only).",
                    },
                    ParamSchema {
                        name: "theme",
                        kind: "string",
                        required: false,
                        description: "Theme editor.ui_designer_render (and the panel's themed preview) draws with; sticks to the document; null: the active theme.",
                    },
                ],
                result: "{ size, safe_area, scale, outside_safe_area: [path], truncated, nodes: [{ path, item, name, kind, rect, clip, visible, layout_dir, text, box_model_bands, outside_safe_area }], lists: [{ path, items, row_height, materialized, first_visible_key }], scale_override, scale_mode, dpi, box_model, locale, pseudo, theme, revision }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_designer_preview as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_render",
                tool: "ui_designer_render",
                description: "Draw the open UI document with the runtime UI renderer and the project theme into an \
                    offscreen texture and return it as a PNG: the same scratch document, samples and settings as \
                    editor.ui_designer_preview, in real pixels (theme colors, borders, radii, text). Needs a Vulkan \
                    device (lavapipe counts). drawn_pixels counts pixels that differ from the backdrop.",
                params: &[
                    ParamSchema {
                        name: "size",
                        kind: "any",
                        required: false,
                        description: "[width, height] or a preset, as for editor.ui_designer_preview.",
                    },
                    ParamSchema {
                        name: "safe_area",
                        kind: "array",
                        required: false,
                        description: "[left, top, right, bottom] insets in px.",
                    },
                    ParamSchema {
                        name: "locale",
                        kind: "string",
                        required: false,
                        description: "Locale of the project's Fluent files.",
                    },
                    ParamSchema {
                        name: "pseudo",
                        kind: "boolean",
                        required: false,
                        description: "Pseudo-localize resolved text.",
                    },
                    ParamSchema {
                        name: "samples",
                        kind: "object",
                        required: false,
                        description: "Binding source -> sample value, as for editor.ui_designer_preview.",
                    },
                    ParamSchema {
                        name: "repeat_count",
                        kind: "number",
                        required: false,
                        description: "Sample items each repeat node shows (default 3, at most 256).",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "number",
                        required: false,
                        description: "Manual UI scale, 0.05..=16; null uses the rule (view state only).",
                    },
                    ParamSchema {
                        name: "scale_mode",
                        kind: "any",
                        required: false,
                        description: "Preview rule in ScaleMode JSON form; null uses the runtime default. Never saved to the document.",
                    },
                    ParamSchema {
                        name: "dpi",
                        kind: "number",
                        required: false,
                        description: "Simulated display DPI, 24..=1536 (default 96), for ConstantPhysicalSize and the project's DPI curve.",
                    },
                    ParamSchema {
                        name: "box_model",
                        kind: "boolean",
                        required: false,
                        description: "Show selected-node margin and padding bands on the designer canvas (view state only).",
                    },
                    ParamSchema {
                        name: "theme",
                        kind: "string",
                        required: false,
                        description: "Theme to draw with; sticks to the document like the other settings (the panel's theme picker sets the same); null: the active one.",
                    },
                ],
                result: "{ width, height, theme, theme_errors, drawn_pixels, mime, png_base64, revision, interactive }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_designer_render as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_save",
                tool: "ui_designer_save",
                description: "Save the open UI document as .ui.ron at the current format version (temp file and \
                    rename; the text is parsed back first). Only the changed fields are rewritten: comments, \
                    key order and every other byte of the file it was opened from stay. If that cannot produce \
                    this exact document the canonical form is written and last_save_canonical says so.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "Where to save (default: where it was opened).",
                    },
                    ParamSchema {
                        name: "canonical",
                        kind: "boolean",
                        required: false,
                        description: "Write the canonical form instead (drops the file's comments).",
                    },
                ],
                result: "{ saved, path, dirty, revision, last_save_canonical, comments_lost }",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_designer_save as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_designer_style_sources",
                tool: "ui_designer_style_sources",
                description: "Inspect a preview node's computed style with every winning and losing theme, class, variation, inline and default source, including token links.",
                params: &[
                    NODE,
                    ParamSchema {
                        name: "open_token",
                        kind: "string",
                        required: false,
                        description: "Open the Theme panel at this token, which must occur in the node source history.",
                    },
                ],
                result: "{node, path, properties: [{name, group, value, source, detail, sources: [{source, kind, detail, value, tokens, winning}]}], theme}",
                mutating: false,
                permission: Permission::Read,
            },
            ui_designer_style_sources as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn code(e: &RpcError) -> String {
        e.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn interactive_preview_dispatch_keeps_document_and_writes_only_samples() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        call(w, "editor.ui_designer_open", json!({"new": true})).expect("open");
        call(
            w,
            "editor.ui_designer_edit",
            json!({"op":"add", "parent":"/", "node": {
                "name":"accept", "widget":"button", "command":"accept", "style":{"size":[160,40]}
            }}),
        )
        .expect("button");
        call(
            w,
            "editor.ui_designer_edit",
            json!({"op":"add", "parent":"/", "node": {
                "name":"name", "widget":{"text_input":["",false]}, "style":{"size":[160,40]},
                "bindings":[{"source":"Vm.name", "target":"input", "mode":"two_way"}]
            }}),
        )
        .expect("input");
        let before = w
            .resource::<UiDesigner>()
            .doc()
            .expect("doc")
            .text()
            .expect("text");
        let started = call(
            w,
            "editor.ui_designer_preview",
            json!({"interactive":true, "samples":{"Vm.name":"Ada"}}),
        )
        .expect("play");
        assert_eq!(started["interactive"], true);
        let clicked = call(
            w,
            "editor.ui_designer_preview",
            json!({"input":{"action":"click", "node":"#accept"}}),
        )
        .expect("click");
        assert!(
            clicked["commands"]
                .as_array()
                .expect("commands")
                .iter()
                .any(|e| e["value"] == "accept")
        );
        let typed = call(
            w,
            "editor.ui_designer_preview",
            json!({"input":{"action":"type", "node":"#name", "text":"Grace"}}),
        )
        .expect("type");
        assert_eq!(typed["samples"]["Vm.name"], "Grace");
        assert_eq!(
            w.resource::<UiDesigner>()
                .doc()
                .expect("doc")
                .text()
                .expect("text"),
            before
        );
        let stopped = call(w, "editor.ui_designer_preview", json!({"interactive":false})).expect("stop");
        assert_eq!(stopped["interactive"], false);
    }

    #[test]
    fn designer_style_sources_dispatch_inspects_the_preview_node() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        call(w, "editor.ui_designer_open", json!({"new":true})).expect("open");
        call(
            w,
            "editor.ui_designer_edit",
            json!({"op":"add", "parent":"/", "preset":"button"}),
        )
        .expect("button");
        let out = call(w, "editor.ui_designer_style_sources", json!({"node":"/0"})).expect("style sources");
        assert_eq!(out["path"], "/0");
        assert!(
            out["properties"]
                .as_array()
                .expect("properties")
                .iter()
                .any(|p| p["name"] == "radius" && p["source"] == "theme")
        );
    }

    #[test]
    fn every_method_has_a_schema_and_valid_error_codes() {
        let methods = methods();
        assert_eq!(methods.len(), 9);
        for (schema, _) in &methods {
            assert!(schema.name.starts_with("editor.ui_designer_"));
            assert_eq!(
                schema.mutating,
                schema.permission != Permission::Read,
                "{}",
                schema.name
            );
        }
        for c in ERROR_CODES.iter().chain(&RENDER_ERROR_CODES) {
            assert!(
                c.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{c}"
            );
        }
    }

    /// Spec 81 § 5: each method's permission is the one the table names (open and save
    /// `assets`, edit `edit_scene`, the readers `read`).
    #[test]
    fn each_method_has_the_permission_the_spec_names() {
        let want = [
            ("editor.ui_designer_open", Permission::Assets),
            ("editor.ui_designer_state", Permission::Read),
            ("editor.ui_designer_style_sources", Permission::Read),
            ("editor.ui_designer_edit", Permission::EditScene),
            ("editor.ui_designer_images", Permission::Read),
            ("editor.ui_designer_lint", Permission::Read),
            ("editor.ui_designer_preview", Permission::Read),
            ("editor.ui_designer_render", Permission::Read),
            ("editor.ui_designer_save", Permission::Assets),
        ];
        let methods = methods();
        assert_eq!(methods.len(), want.len());
        for (name, permission) in want {
            let (schema, _) = methods
                .iter()
                .find(|(s, _)| s.name == name)
                .unwrap_or_else(|| panic!("no method {name}"));
            assert_eq!(schema.permission, permission, "{name}");
        }
    }

    /// Spec 81 § 4.1 / § 5: `add` takes a full node as JSON (decoded with the format's own types,
    /// a bad one is `ui_designer_bad_value`), `select` changes no undo history and refuses a
    /// node that is not there, and `repeat_count` goes up to 256.
    #[test]
    fn add_by_json_select_and_the_repeat_count_bound() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        ui_designer_open(w, &json!({ "new": true })).expect("new");
        let added = ui_designer_edit(
            w,
            &json!({ "op": "add", "parent": "/", "node": { "name": "hp", "classes": ["bar"] } }),
        )
        .expect("add a JSON node");
        assert_eq!(added["node"], "/0");
        let state = ui_designer_state(w, &json!({ "node": "#hp" })).expect("state");
        assert_eq!(state["node"]["fields"]["classes"], json!(["bar"]));
        let bad = ui_designer_edit(
            w,
            &json!({ "op": "add", "parent": "/", "node": { "widget": "rocket" } }),
        )
        .expect_err("not a node");
        assert_eq!(code(&bad), "ui_designer_bad_value");
        let bad = ui_designer_edit(
            w,
            &json!({ "op": "add", "parent": "/", "preset": "panel", "node": {} }),
        )
        .expect_err("both preset and node");
        assert_eq!(code(&bad), "F0002");

        let undo_len = |w: &World| {
            w.resource::<UiDesigner>()
                .doc()
                .map(UiDesign::undo_len)
                .unwrap_or_default()
        };
        let before = undo_len(w);
        ui_designer_edit(w, &json!({ "op": "select", "node": "#hp" })).expect("select");
        assert_eq!(
            w.resource::<UiDesigner>().doc().and_then(|d| d.selection.clone()),
            Some(vec![0])
        );
        let bad = ui_designer_edit(w, &json!({ "op": "select", "node": "/9" })).expect_err("no node");
        assert_eq!(code(&bad), "ui_designer_bad_node");
        ui_designer_edit(w, &json!({ "op": "select", "node": null })).expect("clear selection");
        assert_eq!(undo_len(w), before, "selection is not an undo step");

        ui_designer_preview(w, &json!({ "repeat_count": MAX_REPEAT_COUNT })).expect("256 items");
        let bad = ui_designer_preview(w, &json!({ "repeat_count": MAX_REPEAT_COUNT + 1 })).expect_err("257");
        assert_eq!(code(&bad), "ui_designer_bad_value");
        let bad = ui_designer_preview(w, &json!({ "repeat_count": -1 })).expect_err("negative");
        assert_eq!(code(&bad), "ui_designer_bad_value");
    }

    /// The table reaches each handler under its wire name, on a new document.
    #[test]
    fn every_wire_name_reaches_its_handler() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let table = methods();
        let call = |w: &mut World, name: &str, params: Value| {
            let (_, handler) = table.iter().find(|(s, _)| s.name == name).expect(name);
            handler(w, &params)
        };
        call(w, "editor.ui_designer_open", json!({ "new": true })).expect("open");
        call(w, "editor.ui_designer_state", json!({})).expect("state");
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "add", "parent": "/", "preset": "panel" }),
        )
        .expect("edit");
        call(w, "editor.ui_designer_lint", json!({})).expect("lint");
        call(w, "editor.ui_designer_images", json!({})).expect("images");
        assert!(call(w, "editor.ui_designer_preview", json!({ "size": "12x34" })).is_err());
        assert!(
            call(w, "editor.ui_designer_render", json!({ "theme": 5 })).is_err(),
            "a theme that is not a name is refused before any device is made"
        );
        assert!(
            call(w, "editor.ui_designer_save", json!({})).is_err(),
            "a new document has no path"
        );
    }

    /// Spec 81 § 7: each documented error code comes back from the method that owns it.
    #[test]
    fn every_error_code_is_returned() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-agent-codes");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let mut seen = std::collections::BTreeSet::new();
        let mut see = |result: Result<Value, RpcError>| {
            seen.insert(code(&result.expect_err("must fail")));
        };

        see(ui_designer_lint(w, &json!({})));
        let broken = dir.join("broken.ui.ron");
        std::fs::write(&broken, "(version: 3, root: (").expect("write");
        see(ui_designer_open(
            w,
            &json!({ "path": broken.display().to_string() }),
        ));
        see(ui_designer_open(
            w,
            &json!({ "path": dir.join("nope.ui.ron").display().to_string() }),
        ));

        ui_designer_open(w, &json!({ "new": true })).expect("new");
        see(ui_designer_edit(w, &json!({ "op": "undo" })));
        see(ui_designer_edit(w, &json!({ "op": "redo" })));
        see(ui_designer_edit(w, &json!({ "op": "explode" })));
        see(ui_designer_edit(w, &json!({ "op": "remove", "node": "/3" })));
        see(ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "/", "key": "widget", "value": "rocket" }),
        ));
        ui_designer_edit(w, &json!({ "op": "add", "parent": "/", "preset": "panel" })).expect("add");
        ui_designer_edit(w, &json!({ "op": "add", "parent": "/0", "preset": "panel" })).expect("add");
        see(ui_designer_edit(
            w,
            &json!({ "op": "move", "node": "/0", "parent": "/0/0" }),
        ));
        see(ui_designer_preview(w, &json!({ "size": "12x34" })));

        let expected: std::collections::BTreeSet<String> =
            ERROR_CODES.iter().map(|c| (*c).to_owned()).collect();
        assert_eq!(seen, expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_edit_lint_preview_save_flow() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-agent");
        let path = dir.join("hud.ui.ron");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        assert_eq!(ui_designer_state(w, &json!({})).expect("state")["open"], false);
        let err = ui_designer_edit(w, &json!({ "op": "undo" })).expect_err("no doc");
        assert_eq!(code(&err), "ui_designer_no_document");

        let opened =
            ui_designer_open(w, &json!({ "new": true, "path": path.display().to_string() })).expect("new");
        assert_eq!(opened["tree"].as_array().map(Vec::len), Some(1));

        let added =
            ui_designer_edit(w, &json!({ "op": "add", "parent": "/", "preset": "progress" })).expect("add");
        assert_eq!(added["node"], "/0");
        ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "/0", "key": "name", "value": "hp" }),
        )
        .expect("name");
        ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "#hp", "key": "bindings",
                     "value": [{ "source": "HudVm.health", "target": "progress" }] }),
        )
        .expect("bind");
        ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "#hp", "key": "style.size", "value": [200, 20] }),
        )
        .expect("size");
        let bad = ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "#hp", "key": "widget", "value": "rocket" }),
        )
        .expect_err("bad widget");
        assert_eq!(code(&bad), "ui_designer_bad_value");
        let bad = ui_designer_edit(w, &json!({ "op": "explode" })).expect_err("bad op");
        assert_eq!(code(&bad), "ui_designer_bad_op");
        let bad = ui_designer_edit(w, &json!({ "op": "remove", "node": "/7" })).expect_err("bad node");
        assert_eq!(code(&bad), "ui_designer_bad_node");

        let state = ui_designer_state(w, &json!({ "node": "#hp" })).expect("state");
        assert_eq!(state["node"]["fields"]["bindings"][0]["source"], "HudVm.health");
        assert_eq!(state["dirty"], true);

        let lint = ui_designer_lint(w, &json!({})).expect("lint");
        assert_eq!(lint["errors"], 0, "{lint}");

        let frame = ui_designer_preview(
            w,
            &json!({ "size": "1280x800", "samples": { "HudVm.health": 0.4 } }),
        )
        .expect("preview");
        assert_eq!(frame["size"], json!([1280.0, 800.0]));
        assert_eq!(frame["nodes"][1]["name"], "hp");
        let bad = ui_designer_preview(w, &json!({ "size": [0, 10] })).expect_err("bad size");
        assert_eq!(code(&bad), "ui_designer_bad_size");

        let saved = ui_designer_save(w, &json!({})).expect("save");
        assert_eq!(saved["dirty"], false);
        let text = std::fs::read_to_string(&path).expect("saved file");
        assert!(text.contains("HudVm.health"), "{text}");

        let undone = ui_designer_edit(w, &json!({ "op": "undo" })).expect("undo");
        assert_eq!(undone["dirty"], true);
        ui_designer_open(w, &json!({ "path": path.display().to_string() })).expect("reopen");
        let state = ui_designer_state(w, &json!({ "node": "#hp" })).expect("state");
        assert_eq!(state["node"]["fields"]["style"]["size"], json!([200.0, 20.0]));
        let missing = ui_designer_open(
            w,
            &json!({ "path": dir.join("nope.ui.ron").display().to_string() }),
        )
        .expect_err("missing");
        assert_eq!(code(&missing), "ui_designer_io");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The repeat and converter ops, the preview's sample items, and a save that keeps a comment.
    #[test]
    fn repeat_converter_preview_and_comment_keeping_save() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-agent-repeat");
        let path = dir.join("list.ui.ron");
        std::fs::write(
            &path,
            "// keep this comment\n(version: 9, root: (children: [\n    // and this one\n    (name: \"rows\", children: [(name: \"row\", text: (spans: [\"-\"]))]),\n]))\n",
        )
        .expect("write");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        ui_designer_open(w, &json!({ "path": path.display().to_string() })).expect("open");
        ui_designer_edit(
            w,
            &json!({ "op": "repeat", "node": "#rows", "source": "Vm.items", "key": "id" }),
        )
        .expect("repeat");
        ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "#row", "key": "bindings", "value": [{ "source": ".score" }] }),
        )
        .expect("bind");
        ui_designer_edit(
            w,
            &json!({ "op": "converter", "node": "#row", "converter": { "number": { "precision": 2 } } }),
        )
        .expect("converter");

        let state = ui_designer_state(w, &json!({ "node": "#row" })).expect("state");
        assert_eq!(state["tree"][1]["repeat"], "Vm.items");
        assert_eq!(state["tree"][2]["in_template"], true);
        assert_eq!(
            state["node"]["fields"]["bindings"][0]["converter"]["number"]["precision"],
            2
        );

        let bad = ui_designer_edit(
            w,
            &json!({ "op": "converter", "node": "#row", "converter": "rocket" }),
        )
        .expect_err("unknown converter");
        assert_eq!(code(&bad), "ui_designer_bad_value");
        let bad = ui_designer_edit(
            w,
            &json!({ "op": "converter", "node": "#row", "binding": 3, "converter": null }),
        )
        .expect_err("no such binding");
        assert_eq!(code(&bad), "ui_designer_bad_value");
        assert!(ui_designer_edit(w, &json!({ "op": "converter", "node": "#row" })).is_err());

        let frame = ui_designer_preview(
            w,
            &json!({ "repeat_count": 4, "samples": { "Vm.items[].score": 1.5, "Vm.items[2].score": 9 } }),
        )
        .expect("preview");
        let rows: Vec<&Value> = frame["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .filter(|n| n["name"] == "row")
            .collect();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[3]["item"], 3);
        assert_eq!(rows[3]["path"], "/0/0");
        assert_eq!(rows[0]["text"], "1.50");
        assert_eq!(rows[2]["text"], "9.00");
        assert_eq!(frame["truncated"], false);
        let bad = ui_designer_preview(w, &json!({ "repeat_count": 100000 })).expect_err("too many");
        assert_eq!(code(&bad), "ui_designer_bad_value");

        let saved = ui_designer_save(w, &json!({})).expect("save");
        assert_eq!(saved["last_save_canonical"], false);
        assert_eq!(saved["comments_lost"], false);
        let text = std::fs::read_to_string(&path).expect("saved");
        assert!(
            text.contains("// keep this comment") && text.contains("// and this one"),
            "{text}"
        );
        assert!(
            text.contains(r#"repeat: (source: "Vm.items", key: "id")"#),
            "{text}"
        );

        ui_designer_edit(w, &json!({ "op": "repeat", "node": "#rows", "source": null })).expect("clear");
        let saved = ui_designer_save(w, &json!({ "canonical": true })).expect("save");
        assert_eq!(saved["last_save_canonical"], true);
        assert_eq!(saved["comments_lost"], true);
        let text = std::fs::read_to_string(&path).expect("saved");
        assert!(!text.contains("//") && !text.contains("repeat"), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `editor.ui_designer_render` draws the open document with the runtime renderer at the asked
    /// size (lavapipe counts as a GPU), or reports `ui_designer_no_gpu` on a machine without one.
    #[test]
    fn render_draws_the_open_document_with_the_theme() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let err = ui_designer_render(w, &json!({})).expect_err("no document");
        assert_eq!(code(&err), "ui_designer_no_document");
        ui_designer_open(w, &json!({ "new": true })).expect("new");
        ui_designer_edit(w, &json!({ "op": "add", "parent": "/", "preset": "button" })).expect("add");
        let out = match ui_designer_render(w, &json!({ "size": [320, 180], "safe_area": [0, 0, 0, 0] })) {
            Ok(out) => out,
            Err(e) => {
                assert_eq!(code(&e), "ui_designer_no_gpu", "{e:?}");
                eprintln!("skipped: no GPU device");
                return;
            }
        };
        assert_eq!(
            (out["width"].as_u64(), out["height"].as_u64()),
            (Some(320), Some(180))
        );
        assert_eq!(out["mime"], "image/png");
        let png = crate::reference::base64_decode(out["png_base64"].as_str().expect("png")).expect("base64");
        assert!(png.starts_with(b"\x89PNG"), "a PNG");
        assert!(out["drawn_pixels"].as_u64().expect("count") > 100, "{out}");
        assert!(RENDER_ERROR_CODES.contains(&"ui_designer_unknown_theme"));
        let err = ui_designer_render(w, &json!({ "theme": "no-such-theme" })).expect_err("unknown theme");
        assert_eq!(code(&err), "ui_designer_unknown_theme");
    }

    /// ADR-5180: a headless editor without an adapter (`--no-gpu`) answers `editor.ui_designer_render`
    /// with `editor_headless`, like its view methods, and opens no device of the designer's own;
    /// `preview_device` refuses the same way for any other caller.
    #[test]
    fn a_host_without_a_gpu_refuses_the_themed_render_and_opens_no_device() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        crate::headless::prepare(w, false);
        ui_designer_open(w, &json!({ "new": true })).expect("new");
        ui_designer_edit(w, &json!({ "op": "add", "parent": "/", "preset": "button" })).expect("add");
        let err = ui_designer_render(w, &json!({ "size": [320, 180] })).expect_err("refused");
        assert_eq!(err.code, regia_agent::protocol::UNSUPPORTED, "{err:?}");
        assert_eq!(code(&err), "editor_headless", "{err:?}");
        assert!(
            w.get_resource::<crate::ui_designer_gpu::DesignerGpu>().is_none(),
            "no designer device"
        );
        assert!(
            w.get_resource::<regia_render::RenderDevice>().is_none(),
            "no world device"
        );
        let err = crate::ui_designer_gpu::preview_device(w).err().expect("refused");
        assert_eq!(err.code, "no_gpu", "{err}");
        assert!(err.message.contains("--no-gpu"), "{err}");
        assert!(
            w.get_resource::<crate::ui_designer_gpu::DesignerGpu>().is_none(),
            "still no designer device"
        );
        // The layout preview needs no GPU and still answers.
        ui_designer_preview(w, &json!({ "size": [320, 180] })).expect("preview");
    }

    /// ADR-3148: the image and mask ops name pictures by asset path, icon or id (one undo step each);
    /// editor.ui_designer_images lists them with what the project has; the layout preview places an
    /// asset-named image where the raw-id form goes; the themed render draws the project's source
    /// picture (and the fallback checker for a missing one) when a Vulkan device exists.
    #[test]
    fn image_ops_list_preview_and_render_named_pictures() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-images");
        let ui = dir.join("assets").join("ui");
        std::fs::create_dir_all(&ui).expect("dir");
        image::RgbaImage::from_pixel(16, 16, image::Rgba([0, 255, 0, 255]))
            .save(ui.join("panel.png"))
            .expect("png");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        w.insert_resource(crate::agent_ext::EditorProject {
            root: Some(dir.clone()),
        });
        ui_designer_open(w, &json!({ "new": true })).expect("new");
        for node in ["/", "/"] {
            ui_designer_edit(w, &json!({ "op": "add", "parent": node, "preset": "image" })).expect("add");
        }
        for (i, name) in ["pic", "gone"].iter().enumerate() {
            let node = format!("/{i}");
            ui_designer_edit(
                w,
                &json!({ "op": "set", "node": node, "key": "name", "value": name }),
            )
            .expect("name");
            ui_designer_edit(
                w,
                &json!({ "op": "set", "node": node, "key": "style.size", "value": [100, 100] }),
            )
            .expect("size");
        }
        let raw_rect = |w: &mut World| {
            let out = ui_designer_preview(w, &json!({ "size": [320, 180], "safe_area": [0, 0, 0, 0] }))
                .expect("preview");
            out["nodes"].as_array().expect("nodes")[1]["rect"].clone()
        };
        let before = raw_rect(w);
        let undo_before = doc(w).expect("doc").undo_len();
        ui_designer_edit(
            w,
            &json!({ "op": "image", "node": "#pic", "asset": "assets/ui/panel.png" }),
        )
        .expect("asset");
        assert_eq!(doc(w).expect("doc").undo_len(), undo_before + 1, "one undo step");
        ui_designer_edit(
            w,
            &json!({ "op": "image", "node": "#gone", "asset": "ui/gone.png" }),
        )
        .expect("gone");
        ui_designer_edit(
            w,
            &json!({ "op": "mask", "node": "#pic", "icon": "ring", "fit": "contain" }),
        )
        .expect("mask");
        let err = ui_designer_edit(
            w,
            &json!({ "op": "image", "node": "#pic", "asset": "a", "icon": "b" }),
        )
        .expect_err("two sources");
        assert_eq!(code(&err), "ui_designer_bad_value");
        assert_eq!(raw_rect(w), before, "the named picture lays out like the raw id");

        let state = ui_designer_state(w, &json!({ "node": "#pic" })).expect("state");
        assert_eq!(state["node"]["fields"]["image"]["asset"], "assets/ui/panel.png");
        assert_eq!(state["node"]["fields"]["mask"]["icon"], "ring");
        let images = ui_designer_images(w, &json!({})).expect("images");
        assert_eq!(images["assets"], json!(["assets/ui/panel.png", "ui/gone.png"]));
        assert_eq!(images["pictures"], json!(["assets/ui/panel.png"]));
        assert_eq!(images["missing"], json!(1), "{images}");
        assert_eq!(images["references"][0]["found"], json!(true));
        assert_eq!(images["references"][1]["field"], "mask");
        assert_eq!(
            images["references"][1]["found"],
            Value::Null,
            "no icon atlas: unknown"
        );

        // The saved file round-trips the named pictures.
        let path = dir.join("assets").join("ui").join("hud.ui.ron");
        ui_designer_save(w, &json!({ "path": path.display().to_string() })).expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(text.contains("asset: \"assets/ui/panel.png\""), "{text}");
        let file = regia_ui::parse_ui_ron(&text, "hud", &regia_ui::UiMigrators::default()).expect("parse");
        assert_eq!(&file, doc(w).expect("doc").file());

        // 960x540 against the 1920x1080 reference: a 100 px node is 50 physical px.
        let out = match ui_designer_render(w, &json!({ "size": [960, 540], "safe_area": [0, 0, 0, 0] })) {
            Ok(out) => out,
            Err(e) => {
                assert_eq!(code(&e), "ui_designer_no_gpu", "{e:?}");
                eprintln!("skipped: no GPU device");
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
        };
        let png = crate::reference::base64_decode(out["png_base64"].as_str().expect("png")).expect("base64");
        let img = image::load_from_memory(&png).expect("decode").into_rgba8();
        let count = |f: &dyn Fn(&image::Rgba<u8>) -> bool| img.pixels().filter(|p| f(p)).count();
        let green = count(&|p| p[1] > 200 && p[0] < 60 && p[2] < 60);
        // The fallback is a 2x2 magenta and black checker stretched over the node (filtered).
        let magenta = count(&|p| p[0] > 150 && p[2] > 150 && p[1] < 60 && p[0].abs_diff(p[2]) < 40);
        assert!(
            green > 1000,
            "the project's picture is drawn: {green} green pixels"
        );
        assert!(
            magenta > 300,
            "the missing picture draws the fallback: {magenta} magenta pixels"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no project open the themed render cannot read source pictures; `ThemeChoice::project`
    /// promises the runtime fallback (the magenta and black checker). It used to draw an id with
    /// no picture behind it: a blank (white) box that hid the missing picture.
    #[test]
    fn without_a_project_the_render_draws_the_fallback_for_asset_pictures() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        w.insert_resource(crate::agent_ext::EditorProject { root: None });
        ui_designer_open(w, &json!({ "new": true })).expect("new");
        ui_designer_edit(w, &json!({ "op": "add", "parent": "/", "preset": "image" })).expect("add");
        ui_designer_edit(
            w,
            &json!({ "op": "set", "node": "/0", "key": "style.size", "value": [100, 100] }),
        )
        .expect("size");
        ui_designer_edit(
            w,
            &json!({ "op": "image", "node": "/0", "asset": "ui/panel.png" }),
        )
        .expect("asset");
        let out = match ui_designer_render(w, &json!({ "size": [960, 540], "safe_area": [0, 0, 0, 0] })) {
            Ok(out) => out,
            Err(e) => {
                assert_eq!(code(&e), "ui_designer_no_gpu", "{e:?}");
                eprintln!("skipped: no GPU device");
                return;
            }
        };
        let png = crate::reference::base64_decode(out["png_base64"].as_str().expect("png")).expect("base64");
        let img = image::load_from_memory(&png).expect("decode").into_rgba8();
        let magenta = img
            .pixels()
            .filter(|p| p[0] > 150 && p[2] > 150 && p[1] < 60 && p[0].abs_diff(p[2]) < 40)
            .count();
        assert!(
            magenta > 300,
            "no project: the asset picture draws the fallback: {magenta} magenta pixels"
        );
    }

    /// A document with two absolutely placed panels (`#a`, `#b`) and a column of three.
    const CANVAS_DOC: &str = r#"(
    version: 11,
    root: (
        style: (size: (Percent(100), Percent(100))),
        children: [
            // Kept by every save.
            (name: "a", style: (position: absolute, inset: (50, 60, Auto, Auto), size: (100, 40))),
            (name: "b", style: (position: absolute, inset: (300, 60, Auto, Auto), size: (120, 40))),
            (name: "col", style: (direction: column, size: (200, 300), margin: (0, 200, 0, 0)), children: [
                (name: "r0", style: (size: (200, 50))),
                (name: "r1", style: (size: (200, 50))),
            ]),
        ],
    ),
)
"#;

    /// Calls the dispatch table under its wire name.
    fn call(w: &mut World, name: &str, params: Value) -> Result<Value, RpcError> {
        let table = methods();
        let (_, handler) = table
            .iter()
            .find(|(s, _)| s.name == name)
            .unwrap_or_else(|| panic!("no method {name}"));
        handler(w, &params)
    }

    fn open_canvas_doc(w: &mut World, dir: &Path) -> PathBuf {
        let path = dir.join("hud.ui.ron");
        std::fs::write(&path, CANVAS_DOC).expect("write");
        call(
            w,
            "editor.ui_designer_open",
            json!({ "path": path.display().to_string() }),
        )
        .expect("open");
        path
    }

    fn rect(w: &mut World, path: &str) -> Vec<f64> {
        let frame = call(w, "editor.ui_designer_preview", json!({})).expect("preview");
        let nodes = frame["nodes"].as_array().cloned().unwrap_or_default();
        let node = nodes
            .iter()
            .find(|n| n["path"] == path)
            .unwrap_or_else(|| panic!("no {path}"));
        node["rect"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_f64)
            .collect()
    }

    /// ADR-9730, spec 81 § 4.7 through the dispatcher: `transform` moves an absolutely placed node by
    /// (+10, +20) (its inset is written and its laid-out rect moves by that), one undo restores the
    /// saved bytes, a snapped resize lands on the sibling's edge, and the root cannot move.
    #[test]
    fn transform_moves_resizes_and_snaps_through_the_dispatcher() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-transform");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        let a = rect(w, "/0");
        let moved = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "node": "#a", "rect": [a[0] + 10.0, a[1] + 20.0, a[2], a[3]], "snap": false }),
        )
        .expect("transform");
        assert_eq!(moved["writes"], json!(["inset"]));
        assert_eq!(moved["undo"], 1, "one undo step");
        let after = rect(w, "/0");
        assert!(
            (after[0] - a[0] - 10.0).abs() < 0.01 && (after[1] - a[1] - 20.0).abs() < 0.01,
            "{a:?} -> {after:?}"
        );
        call(w, "editor.ui_designer_edit", json!({ "op": "undo" })).expect("undo");
        let out = dir.join("undone.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": out.display().to_string() }),
        )
        .expect("save");
        assert_eq!(
            std::fs::read_to_string(&out).expect("read"),
            CANVAS_DOC,
            "one undo restores the bytes"
        );

        let a = rect(w, "/0");
        let b = rect(w, "/1");
        let a_right = a[0] + a[2];
        let resized = call(
            w,
            "editor.ui_designer_edit",
            json!({
                "op": "transform", "node": "#b", "mode": "resize",
                "rect": [a_right + 3.0, b[1], b[0] + b[2] - a_right - 3.0, b[3]],
                "snap": { "grid": null },
            }),
        )
        .expect("resize");
        assert!(
            resized["guides"]
                .as_array()
                .is_some_and(|g| g.iter().any(|g| g["source"] == "sibling")),
            "{resized}"
        );
        let b2 = rect(w, "/1");
        assert!((b2[0] - a_right).abs() < 0.01, "lands on a's right edge: {b2:?}");
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "node": "/", "rect": [0, 0, 10, 10] }),
        )
        .expect_err("the root");
        assert_eq!(code(&e), "ui_designer_bad_node");
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "node": "#a", "rect": [0, 0] }),
        )
        .expect_err("short rect");
        assert_eq!(code(&e), "ui_designer_bad_value");
    }

    /// ADR-9730, spec 81 § 4.4: a typed widget field goes through `set` with `widget.<field>`, and
    /// saves the bytes the whole widget set as JSON saves.
    #[test]
    fn typed_widget_keys_through_set() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let text_after = |w: &mut World, key: &str, value: Value| {
            call(w, "editor.ui_designer_open", json!({ "new": true })).expect("new");
            call(
                w,
                "editor.ui_designer_edit",
                json!({ "op": "add", "parent": "/", "preset": "slider" }),
            )
            .expect("add");
            call(
                w,
                "editor.ui_designer_edit",
                json!({ "op": "set", "node": "/0", "key": key, "value": value }),
            )
            .expect("set");
            w.resource::<UiDesigner>()
                .doc()
                .and_then(|d| d.text().ok())
                .unwrap_or_default()
        };
        let typed = text_after(w, "widget.max", json!(2.0));
        let by_json = text_after(w, "widget", json!({ "slider": [0.5, 0.0, 2.0, 0.0] }));
        assert_eq!(typed, by_json);
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "set", "node": "/0", "key": "widget.max", "value": "high" }),
        )
        .expect_err("not a number");
        assert_eq!(code(&e), "ui_designer_bad_value");
    }

    /// ADR-9730, spec 81 § 4.8: `select_many` and a `set` of `opacity` on three nodes is one undo
    /// step; `state` reports the shared and mixed fields; align, group, remove and duplicate of
    /// several nodes are one step each.
    #[test]
    fn multi_select_edits_through_the_dispatcher() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-multi");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "select_many", "nodes": ["#a", "#b", "#col"] }),
        )
        .expect("select_many");
        let state = call(w, "editor.ui_designer_state", json!({})).expect("state");
        assert_eq!(state["selected"], json!(["/0", "/1", "/2"]));
        let undo = state["undo"].as_u64().unwrap_or_default();
        let set = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "set", "key": "opacity", "value": 0.5 }),
        )
        .expect("set on the selection");
        assert_eq!(set["undo"].as_u64(), Some(undo + 1), "one undo step");
        let state = call(w, "editor.ui_designer_state", json!({})).expect("state");
        assert_eq!(state["shared"]["opacity"], json!(0.5));
        assert!(
            state["mixed"]
                .as_array()
                .is_some_and(|m| m.contains(&json!("name"))),
            "{state}"
        );
        call(w, "editor.ui_designer_edit", json!({ "op": "undo" })).expect("undo");
        let state = call(w, "editor.ui_designer_state", json!({ "node": "#b" })).expect("state");
        assert_eq!(
            state["node"]["fields"].get("opacity"),
            None,
            "one undo restores all three"
        );

        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "align", "nodes": ["#a", "#col"], "align": "left" }),
        )
        .expect("align");
        let (a, col) = (rect(w, "/0"), rect(w, "/2"));
        assert!((a[0] - col[0]).abs() < 0.01, "{a:?} {col:?}");
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "align", "nodes": ["#a", "#b"], "align": "diagonal" }),
        )
        .expect_err("bad kind");
        assert_eq!(code(&e), "ui_designer_bad_value");
        let grouped = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "group", "nodes": ["#a", "#b"], "name": "pair" }),
        )
        .expect("group");
        assert_eq!(grouped["node"], "/0");
        let dup = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "duplicate", "nodes": ["#pair", "#col"] }),
        )
        .expect("duplicate");
        assert_eq!(dup["selected"], json!(["/1", "/3"]));
        let before = dup["undo"].as_u64().unwrap_or_default();
        let removed = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "remove", "nodes": ["/1", "/3"] }),
        )
        .expect("remove");
        assert_eq!(removed["undo"].as_u64(), Some(before + 1));
    }

    /// ADR-9730, spec 81 § 4.8 acceptance: a subtree copied in one document pastes into a second one
    /// (names made unique, bindings kept), and the saved second document loads with the runtime
    /// loader.
    #[test]
    fn copy_and_paste_across_documents_through_the_dispatcher() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-paste-agent");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "set", "node": "#r0", "key": "bindings", "value": [{ "source": "HudVm.name" }] }),
        )
        .expect("bind");
        let copied = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "copy", "nodes": ["#col"] }),
        )
        .expect("copy");
        assert!(
            copied["clipboard"]
                .as_str()
                .is_some_and(|t| t.contains("HudVm.name")),
            "{copied}"
        );
        call(w, "editor.ui_designer_open", json!({ "new": true })).expect("second document");
        call(w, "editor.ui_designer_edit", json!({ "op": "paste" })).expect("paste");
        let again = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "paste", "parent": "/" }),
        )
        .expect("paste again");
        assert_eq!(again["node"], "/1");
        let state = call(w, "editor.ui_designer_state", json!({})).expect("state");
        let names: Vec<&str> = state["tree"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|n| n["name"].as_str())
            .collect();
        assert_eq!(names, ["col", "r0", "r1", "col_2", "r0_2", "r1_2"]);
        let path = dir.join("second.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": path.display().to_string() }),
        )
        .expect("save");
        let text = std::fs::read_to_string(&path).expect("read");
        let loaded = regia_ui::parse_ui_ron(&text, "second.ui.ron", &regia_ui::UiMigrators::default())
            .expect("the runtime loader reads the pasted subtree");
        assert_eq!(
            loaded.root.children[1].children[0].bindings[0].source,
            "HudVm.name"
        );
        let cut = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "cut", "nodes": ["/1"] }),
        )
        .expect("cut");
        assert!(cut["clipboard"].as_str().is_some_and(|t| t.contains("col_2")));
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "paste", "text": "[1, 2]" }),
        )
        .expect_err("not nodes");
        assert_eq!(code(&e), "ui_designer_bad_value");
    }

    /// ADR-9980 (`.ui.ron` v11): the text input's `max_chars`, `secret`, `read_only` and `filter`
    /// and an image's `fit` go through `set` (`widget.<field>`, `image.fit`); a typed edit saves
    /// the bytes the whole widget set as JSON saves, and a bad value is refused.
    #[test]
    fn text_input_fields_and_image_fit_through_set() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let text_after = |w: &mut World, preset: &str, edits: &[(&str, Value)]| {
            call(w, "editor.ui_designer_open", json!({ "new": true })).expect("new");
            call(
                w,
                "editor.ui_designer_edit",
                json!({ "op": "add", "parent": "/", "preset": preset }),
            )
            .expect("add");
            for (key, value) in edits {
                call(
                    w,
                    "editor.ui_designer_edit",
                    json!({ "op": "set", "node": "/0", "key": key, "value": value }),
                )
                .unwrap_or_else(|e| panic!("set {key}: {e:?}"));
            }
            w.resource::<UiDesigner>()
                .doc()
                .and_then(|d| d.text().ok())
                .unwrap_or_default()
        };
        let typed = text_after(
            w,
            "text_input",
            &[
                ("widget.max_chars", json!(4)),
                ("widget.secret", json!("*")),
                ("widget.read_only", json!(true)),
                ("widget.filter", json!("integer")),
            ],
        );
        assert!(
            typed.contains("max_chars: 4")
                && typed.contains("secret: '*'")
                && typed.contains("filter: integer"),
            "{typed}"
        );
        let by_json = text_after(
            w,
            "text_input",
            &[(
                "widget",
                json!({ "text_input": ["", false, { "max_chars": 4, "secret": "*", "read_only": true, "filter": "integer" }] }),
            )],
        );
        assert_eq!(typed, by_json);
        // Clearing a limit writes the short form again.
        let cleared = text_after(
            w,
            "text_input",
            &[("widget.max_chars", json!(4)), ("widget.max_chars", json!(0))],
        );
        assert!(cleared.contains("text_input(\"\", false)"), "{cleared}");
        for (key, bad) in [
            ("widget.filter", json!("hex")),
            ("widget.secret", json!("**")),
            ("widget.max_chars", json!(-1)),
        ] {
            let e = call(
                w,
                "editor.ui_designer_edit",
                json!({ "op": "set", "node": "/0", "key": key, "value": bad }),
            )
            .expect_err("bad value");
            assert_eq!(code(&e), "ui_designer_bad_value", "{key}");
        }
        let fit = text_after(w, "image", &[("image.fit", json!("cover"))]);
        assert!(fit.contains("fit: cover"), "{fit}");
        // ADR-7751 folds `tile` into v11 image fits; an unknown fit is still refused.
        let tile = text_after(w, "image", &[("image.fit", json!("tile"))]);
        assert!(tile.contains("fit: tile"), "{tile}");
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "set", "node": "/0", "key": "image.fit", "value": "squash" }),
        )
        .expect_err("not a fit");
        assert_eq!(code(&e), "ui_designer_bad_value");
    }

    /// ADR-7751 (the v11 fold) through the dispatcher: `bind_source` sets a binding's theme token,
    /// animation or field (one at a time; none or two is refused), `repeat` takes `row_height`
    /// and keeps it across a source edit, and `image` takes `fit: tile`. The saved text uses the
    /// v11 spellings, lints clean, and parses back to the same document.
    #[test]
    fn bind_source_row_height_and_tile_through_the_dispatcher() {
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let edit = |w: &mut World, params: Value| call(w, "editor.ui_designer_edit", params);
        call(w, "editor.ui_designer_open", json!({ "new": true })).expect("new");
        edit(w, json!({ "op": "add", "parent": "/", "preset": "text" })).expect("text");
        edit(
            w,
            json!({ "op": "set", "node": "/0", "key": "bindings", "value": [{ "source": "Vm.x" }, { "source": "Vm.y", "target": "opacity" }] }),
        )
        .expect("bindings");
        edit(
            w,
            json!({ "op": "bind_source", "node": "/0", "theme": "color.accent" }),
        )
        .expect("theme");
        edit(
            w,
            json!({ "op": "bind_source", "node": "/0", "binding": 1,
                "animation": { "from": 0, "to": 1, "duration": 0.8, "ease": "quad_in_out", "repeat": "ping_pong" } }),
        )
        .expect("animation");
        let state = call(w, "editor.ui_designer_state", json!({ "node": "/0" })).expect("state");
        let bindings = &state["node"]["fields"]["bindings"];
        assert_eq!(bindings[0]["theme"], "color.accent", "{state}");
        assert!(
            bindings[0].get("source").is_none(),
            "the theme clears the field: {state}"
        );
        assert_eq!(bindings[1]["animation"]["repeat"], "ping_pong", "{state}");
        for bad in [
            json!({ "op": "bind_source", "node": "/0" }),
            json!({ "op": "bind_source", "node": "/0", "theme": "color.accent", "source": "Vm.x" }),
            json!({ "op": "bind_source", "node": "/0", "animation": { "from": 0 } }),
        ] {
            let e = edit(w, bad.clone()).expect_err("refused");
            assert_eq!(code(&e), "ui_designer_bad_value", "{bad}");
        }

        edit(w, json!({ "op": "add", "parent": "/", "preset": "list" })).expect("list");
        edit(w, json!({ "op": "add", "parent": "/1", "preset": "panel" })).expect("template");
        edit(
            w,
            json!({ "op": "set", "node": "/1/0", "key": "style.size", "value": ["Auto", 56] }),
        )
        .expect("size");
        edit(
            w,
            json!({ "op": "repeat", "node": "/1", "source": "Vm.items", "key": "id", "row_height": "template" }),
        )
        .expect("repeat");
        edit(
            w,
            json!({ "op": "repeat", "node": "/1", "source": "Vm.rows", "key": "id" }),
        )
        .expect("source");
        let e = edit(
            w,
            json!({ "op": "repeat", "node": "/1", "source": "Vm.rows", "row_height": "tall" }),
        )
        .expect_err("bad row height");
        assert_eq!(code(&e), "ui_designer_bad_value");

        edit(w, json!({ "op": "add", "parent": "/", "preset": "image" })).expect("image");
        edit(w, json!({ "op": "image", "node": "/2", "id": 7, "fit": "tile" })).expect("tile");
        let e = edit(w, json!({ "op": "image", "node": "/2", "fit": "squash" })).expect_err("bad fit");
        assert_eq!(code(&e), "ui_designer_bad_value");

        let text = w
            .resource::<UiDesigner>()
            .doc()
            .and_then(|d| d.text().ok())
            .unwrap_or_default();
        for spelled in [
            "theme: \"color.accent\"",
            "repeat: ping_pong",
            "ease: \"quad_in_out\"",
            "source: \"Vm.rows\"",
            "row_height: template",
            "fit: tile",
        ] {
            assert!(text.contains(spelled), "{spelled} in {text}");
        }
        let lint = call(w, "editor.ui_designer_lint", json!({})).expect("lint");
        let codes: Vec<&str> = lint["findings"]
            .as_array()
            .map(|f| f.iter().filter_map(|x| x["code"].as_str()).collect())
            .unwrap_or_default();
        for code in [
            "binding_empty_source",
            "repeat_template_height",
            "repeat_on_list",
            "binding_bad_animation",
        ] {
            assert!(!codes.contains(&code), "{code}: {lint}");
        }
        let parsed =
            regia_ui::parse_ui_ron(&text, "saved.ui.ron", &regia_ui::UiMigrators::default()).expect("parse");
        assert_eq!(&parsed, w.resource::<UiDesigner>().doc().expect("doc").file());
    }

    /// ADR-9980, spec 81 § 4.7: `transform` with `nodes` and a `delta` moves every top-level node by
    /// the same delta (a child of a moved node rides along and is not moved twice) in one undo
    /// step, and one undo restores the saved bytes.
    #[test]
    fn group_transform_moves_every_node_by_the_delta_in_one_undo_step() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-group-move");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        let before = ["/0", "/1", "/2", "/2/0"].map(|p| rect(w, p));
        let undo = call(w, "editor.ui_designer_state", json!({})).expect("state")["undo"]
            .as_u64()
            .unwrap_or_default();
        let moved = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "nodes": ["#a", "#b", "#col", "#r0"], "delta": [10, 20], "snap": false }),
        )
        .expect("group transform");
        assert_eq!(moved["undo"].as_u64(), Some(undo + 1), "one undo step: {moved}");
        assert_eq!(moved["delta"], json!([10.0, 20.0]));
        for (path, was) in ["/0", "/1", "/2", "/2/0"].iter().zip(&before) {
            let now = rect(w, path);
            assert!(
                (now[0] - was[0] - 10.0).abs() < 0.01 && (now[1] - was[1] - 20.0).abs() < 0.01,
                "{path}: {was:?} -> {now:?}"
            );
        }
        call(w, "editor.ui_designer_edit", json!({ "op": "undo" })).expect("undo");
        let out = dir.join("undone.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": out.display().to_string() }),
        )
        .expect("save");
        assert_eq!(
            std::fs::read_to_string(&out).expect("read"),
            CANVAS_DOC,
            "one undo restores the bytes"
        );
        // Without `nodes` the selection moves; snapping lands the group's box on the parent edge.
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "select_many", "nodes": ["#a", "#b"] }),
        )
        .expect("select");
        let a = rect(w, "/0");
        let snapped = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "delta": [-a[0] + 3.0, 0], "snap": { "grid": null } }),
        )
        .expect("snapped group move");
        assert!(
            rect(w, "/0")[0].abs() < 0.01,
            "the group's left edge lands on 0: {snapped}"
        );
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "nodes": ["#a"], "delta": [1] }),
        )
        .expect_err("short delta");
        assert_eq!(code(&e), "ui_designer_bad_value");
        let e = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "nodes": ["/"], "delta": [1, 1] }),
        )
        .expect_err("the root");
        assert_eq!(code(&e), "ui_designer_bad_node");
    }

    /// Runs `ops` through `editor.ui_designer_edit` on a fresh canvas document and returns the
    /// saved bytes (one world each, so the selection and clipboard start the same).
    fn saved_after(label: &str, ops: &[Value]) -> String {
        let dir = regia_test_utils::unique_temp_dir(label);
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        for op in ops {
            call(w, "editor.ui_designer_edit", op.clone()).unwrap_or_else(|e| panic!("{op}: {e:?}"));
        }
        let out = dir.join("out.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": out.display().to_string() }),
        )
        .expect("save");
        std::fs::read_to_string(&out).expect("read")
    }

    /// Lane w7-editor-smalls-z (1): `parent: null` on `add` and `paste` is the default parent, like
    /// an absent key; an unknown parent is still `ui_designer_bad_node`.
    #[test]
    fn a_null_parent_is_the_default_parent_for_add_and_paste() {
        let select = json!({ "op": "select", "node": "#col" });
        let add = |parent: Option<Value>| {
            let mut op = json!({ "op": "add", "preset": "panel" });
            if let Some(p) = parent {
                op["parent"] = p;
            }
            op
        };
        let absent = saved_after("ui-designer-null-parent-a", &[select.clone(), add(None)]);
        let null = saved_after(
            "ui-designer-null-parent-b",
            &[select.clone(), add(Some(Value::Null))],
        );
        assert_eq!(null, absent, "add: null parent = no parent");
        assert_ne!(absent, CANVAS_DOC, "the add changed the document");

        let copy = json!({ "op": "copy", "nodes": ["#r0"] });
        let paste = |parent: Option<Value>| {
            let mut op = json!({ "op": "paste" });
            if let Some(p) = parent {
                op["parent"] = p;
            }
            op
        };
        let absent = saved_after(
            "ui-designer-null-paste-a",
            &[copy.clone(), select.clone(), paste(None)],
        );
        let null = saved_after(
            "ui-designer-null-paste-b",
            &[copy.clone(), select.clone(), paste(Some(Value::Null))],
        );
        assert_eq!(null, absent, "paste: null parent = no parent");
        assert_ne!(absent, CANVAS_DOC, "the paste changed the document");

        let dir = regia_test_utils::unique_temp_dir("ui-designer-null-parent-bad");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        call(w, "editor.ui_designer_edit", copy).expect("copy");
        for op in [add(Some(json!("/9"))), paste(Some(json!("/9")))] {
            let e = call(w, "editor.ui_designer_edit", op.clone()).expect_err("unknown parent");
            assert_eq!(code(&e), "ui_designer_bad_node", "{op}");
        }
    }

    /// Lane w7-editor-smalls-z (2): `transform` with both `rect` and `delta`, and any op with both
    /// `nodes` and `node`, is refused (invalid params) and leaves the document at its revision;
    /// the single forms still work.
    #[test]
    fn conflicting_transform_and_node_params_are_refused() {
        let dir = regia_test_utils::unique_temp_dir("ui-designer-conflicting-params");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        open_canvas_doc(w, &dir);
        let revision = |w: &mut World| {
            call(w, "editor.ui_designer_state", json!({})).expect("state")["revision"].clone()
        };
        let before = revision(w);
        let a = rect(w, "/0");
        for op in [
            json!({ "op": "transform", "node": "#a", "rect": [a[0] + 10.0, a[1], a[2], a[3]], "delta": [5, 5], "snap": false }),
            json!({ "op": "transform", "nodes": ["#b"], "node": "#a", "delta": [5, 5], "snap": false }),
            json!({ "op": "transform", "nodes": ["#b"], "node": "#a", "rect": [a[0] + 10.0, a[1], a[2], a[3]], "snap": false }),
            json!({ "op": "align", "nodes": ["#a", "#b"], "node": "#a", "align": "left" }),
        ] {
            let e = call(w, "editor.ui_designer_edit", op.clone()).expect_err("conflicting params");
            assert_eq!(code(&e), "F0002", "{op}: {e:?}");
            assert_eq!(revision(w), before, "{op}: the document did not change");
        }
        assert_eq!(rect(w, "/0"), a, "#a did not move");
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "node": "#a", "delta": [5, 5], "snap": false }),
        )
        .expect("node + delta");
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "nodes": ["#b"], "delta": [5, 5], "snap": false }),
        )
        .expect("nodes + delta");
        let a = rect(w, "/0");
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "transform", "node": "#a", "rect": [a[0] + 10.0, a[1], a[2], a[3]], "snap": false }),
        )
        .expect("node + rect");
        assert_ne!(revision(w), before);
    }
    #[test]
    fn w12_anchor_presets_keep_rect_and_undo_through_dispatch() {
        let mut w = World::new();
        call(&mut w, "editor.ui_designer_open", json!({"new": true})).expect("valid test fixture");
        call(
            &mut w,
            "editor.ui_designer_edit",
            json!({"op":"add", "parent":"/", "preset":"button"}),
        )
        .expect("valid test fixture");
        for size in [[1280, 720], [1920, 1080], [800, 1280]] {
            call(&mut w, "editor.ui_designer_preview", json!({"size":size})).expect("valid test fixture");
            for preset in regia_ui::anchors::AnchorPreset::ALL {
                let before = rect(&mut w, "/0");
                call(
                    &mut w,
                    "editor.ui_designer_edit",
                    json!({"op":"anchor_preset", "node":"/0", "preset":preset.name(), "keep_position":true}),
                )
                .expect("valid test fixture");
                let after = rect(&mut w, "/0");
                for (a, b) in before.iter().zip(&after) {
                    assert!((a - b).abs() <= 1.0, "{preset:?}: {before:?} -> {after:?}");
                }
                call(&mut w, "editor.ui_designer_edit", json!({"op":"undo"})).expect("valid test fixture");
                assert_eq!(rect(&mut w, "/0"), before);
            }
        }
    }

    #[test]
    fn w12_keep_position_survives_automatic_parent_reflow() {
        let mut world = World::new();
        call(&mut world, "editor.ui_designer_open", json!({"new":true})).expect("new document");
        for (parent, preset) in [("/", "panel"), ("/0", "button")] {
            call(
                &mut world,
                "editor.ui_designer_edit",
                json!({"op":"add","parent":parent,"preset":preset}),
            )
            .expect("add node");
        }
        call(
            &mut world,
            "editor.ui_designer_edit",
            json!({"op":"set","node":"/0/0","key":"style.size","value":[120,40]}),
        )
        .expect("child size");
        let before = rect(&mut world, "/0/0");
        for preset in regia_ui::anchors::AnchorPreset::ALL {
            call(
                &mut world,
                "editor.ui_designer_edit",
                json!({"op":"anchor_preset","node":"/0/0","preset":preset.name(),"keep_position":true}),
            )
            .expect("preserve child when parent reflows");
            let after = rect(&mut world, "/0/0");
            assert!(
                before.iter().zip(&after).all(|(a, b)| (a - b).abs() <= 1.0),
                "{preset:?}: {before:?} -> {after:?}"
            );
            call(&mut world, "editor.ui_designer_edit", json!({"op":"undo"})).expect("undo anchor");
            assert_eq!(rect(&mut world, "/0/0"), before);
        }
    }

    #[test]
    fn w12_manual_preview_scale_is_used_and_invalid_values_are_atomic() {
        let mut w = World::new();
        call(&mut w, "editor.ui_designer_open", json!({"new":true})).expect("valid test fixture");
        let frame =
            call(&mut w, "editor.ui_designer_preview", json!({"scale":1.5})).expect("valid test fixture");
        assert_eq!(frame["scale"], 1.5);
        assert!(call(&mut w, "editor.ui_designer_preview", json!({"scale":0})).is_err());
        assert_eq!(
            call(&mut w, "editor.ui_designer_preview", json!({})).expect("valid test fixture")["scale"],
            1.5
        );
        assert_eq!(
            call(&mut w, "editor.ui_designer_state", json!({})).expect("valid test fixture")["dirty"],
            false
        );
    }

    /// The v11 containers, markup and focus-neighbors fold (ADR-7827) through the dispatcher: a
    /// document with all three containers saves byte for byte, the presets add each container with
    /// its parts, typed fields, `text.markup` and `nav.<dir>` edit through `set`, the lint names a
    /// neighbor that matches no node, and undoing every edit restores the bytes.
    #[test]
    fn containers_markup_and_neighbors_through_the_dispatcher() {
        const PARITY: &str = include_str!("../../regia_ui/tests/fixtures/ui.v11-parity.example.ui.ron");
        let dir = regia_test_utils::unique_temp_dir("ui-designer-parity");
        let mut app = regia_app::App::new();
        let w = app.world_mut();
        let path = dir.join("settings.ui.ron");
        std::fs::write(&path, PARITY).expect("write");
        call(
            w,
            "editor.ui_designer_open",
            json!({ "path": path.display().to_string() }),
        )
        .expect("open");
        let lint = call(w, "editor.ui_designer_lint", json!({})).expect("lint");
        assert_eq!(lint["errors"], 0, "{lint}");
        let state = call(w, "editor.ui_designer_state", json!({ "node": "#body" })).expect("state");
        let ratio = state["node"]["fields"]["widget"]["split"]["ratio"]
            .as_f64()
            .unwrap_or_default();
        assert!((ratio - 0.3).abs() < 1e-6, "{state}");
        let frame = call(w, "editor.ui_designer_preview", json!({})).expect("preview");
        let shown = |frame: &Value, name: &str| {
            frame["nodes"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|n| n["name"] == name)
                .map(|n| n["visible"].clone())
        };
        assert_eq!(
            shown(&frame, "controls"),
            Some(json!(true)),
            "the active page shows"
        );
        assert_eq!(
            shown(&frame, "general"),
            Some(json!(false)),
            "the other page is hidden"
        );
        assert_eq!(
            shown(&frame, "vsync"),
            Some(json!(false)),
            "a closed section hides its body"
        );
        let same = dir.join("same.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": same.display().to_string() }),
        )
        .expect("save");
        assert_eq!(
            std::fs::read_to_string(&same).expect("read"),
            PARITY,
            "byte for byte"
        );

        let mut steps = 0;
        for preset in ["split", "foldable", "tabs"] {
            let added = call(
                w,
                "editor.ui_designer_edit",
                json!({ "op": "add", "parent": "#row", "preset": preset }),
            )
            .expect("add");
            steps += 1;
            let node = added["node"].as_str().unwrap_or_default().to_owned();
            let state = call(w, "editor.ui_designer_state", json!({})).expect("state");
            let row = state["tree"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| r["path"] == json!(node))
                .cloned()
                .unwrap_or_default();
            assert_eq!(row["kind"], json!(preset), "{row}");
            let parts = if preset == "foldable" { 1 } else { 2 };
            assert_eq!(row["children"], json!(parts), "{row}");
        }
        for (node, key, value) in [
            ("#body", "widget.axis", json!("vertical")),
            ("#body", "widget.ratio", json!(0.4)),
            ("#audio_fold", "widget.group", Value::Null),
            ("#pages", "widget.active", json!(0)),
            ("#general", "text.markup", json!(true)),
            ("#quit", "nav.right", json!("reset")),
        ] {
            call(
                w,
                "editor.ui_designer_edit",
                json!({ "op": "set", "node": node, "key": key, "value": value }),
            )
            .unwrap_or_else(|e| panic!("{node} {key}: {e:?}"));
            steps += 1;
        }
        let state = call(w, "editor.ui_designer_state", json!({ "node": "#quit" })).expect("state");
        assert_eq!(
            state["node"]["fields"]["nav"],
            json!({ "up": "apply", "right": "reset" })
        );
        let bad = call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "set", "node": "#body", "key": "widget.axis", "value": "diagonal" }),
        )
        .expect_err("not an axis");
        assert_eq!(code(&bad), "ui_designer_bad_value");
        call(
            w,
            "editor.ui_designer_edit",
            json!({ "op": "set", "node": "#apply", "key": "nav.left", "value": "nobody" }),
        )
        .expect("set");
        steps += 1;
        let lint = call(w, "editor.ui_designer_lint", json!({})).expect("lint");
        let codes: Vec<&str> = lint["findings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| f["code"].as_str())
            .collect();
        assert!(codes.contains(&"nav_unknown_target"), "{lint}");
        let edited = dir.join("edited.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": edited.display().to_string() }),
        )
        .expect("save");
        let text = std::fs::read_to_string(&edited).expect("read");
        for needle in [
            "axis: vertical",
            "ratio: 0.4",
            "markup: true",
            "right: \"reset\"",
            "left: \"nobody\"",
        ] {
            assert!(text.contains(needle), "{needle} in\n{text}");
        }
        for _ in 0..steps {
            call(w, "editor.ui_designer_edit", json!({ "op": "undo" })).expect("undo");
        }
        let undone = dir.join("undone.ui.ron");
        call(
            w,
            "editor.ui_designer_save",
            json!({ "path": undone.display().to_string() }),
        )
        .expect("save");
        // The save in between is now the merge source, so a default it wrote out (`axis: horizontal`
        // in the original) comes back in the canonical form; the document is the original one.
        let migrators = regia_ui::UiMigrators::default();
        let back = std::fs::read_to_string(&undone).expect("read");
        assert_eq!(
            regia_ui::parse_ui_ron(&back, "undone", &migrators).expect("parse"),
            regia_ui::parse_ui_ron(PARITY, "original", &migrators).expect("parse"),
            "every edit undone"
        );
    }
}
