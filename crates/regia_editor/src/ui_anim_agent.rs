// SPDX-License-Identifier: MPL-2.0
//! The UI animation editor's agent surface (`book/agents/ui-anim-designer.md`, ADR-3092):
//! `editor.ui_anim_open`, `_list`, `_edit`, `_set_key`, `_remove_key`, `_scrub`, `_validate` and `_save`.
//! The model is [`UiAnimEditor`]; the UI Animation panel shares it.

use regia_agent::protocol::{INTERNAL_ERROR, INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_asset_pipeline::ui_anim_validate::{target_of, validate_ui_anim};
use regia_asset_pipeline::validators::Finding;
use regia_ecs::prelude::*;
use regia_ui::anim_timeline::{AnimEvent, AnimKey, AnimValue, LoopMode, UiAnimDef};
use serde_json::{Value, json};

use crate::ui_anim_doc::{
    AnimOp, UiAnimDoc, UiAnimEditor, UiAnimError, apply_to_file, node_facts, sample_def, values_json,
};
use crate::ui_designer::UiDesigner;
use crate::ui_designer_agent::{project_root, resolve_path};
use crate::ui_designer_preview::{preview, project_localization};
use crate::viewport_agent::Handler;

/// Every error code the methods return (as `data.code`).
pub const ERROR_CODES: [&str; 13] = [
    "ui_anim_no_document",
    "ui_anim_unknown_animation",
    "ui_anim_unknown_track",
    "ui_anim_unknown_key",
    "ui_anim_unknown_event",
    "ui_anim_duplicate_animation",
    "ui_anim_bad_name",
    "ui_anim_bad_value",
    "ui_anim_invalid",
    "ui_anim_parse",
    "ui_anim_io",
    "ui_anim_nothing_to_undo",
    "ui_anim_nothing_to_redo",
];

fn rpc(e: UiAnimError) -> RpcError {
    let (class, hint) = match e.code {
        "no_document" => (NOT_FOUND, "call editor.ui_anim_open first"),
        "unknown_animation" | "unknown_track" | "unknown_key" | "unknown_event" => (
            NOT_FOUND,
            "editor.ui_anim_list shows the animations, tracks, keys and events",
        ),
        "io" => (INTERNAL_ERROR, "check the path and its permissions"),
        "parse" => (
            INVALID_PARAMS,
            "the file is not a loadable .uianim.ron; nothing was changed",
        ),
        _ => (
            INVALID_PARAMS,
            "the message names the problem; nothing was changed",
        ),
    };
    RpcError::new(class, &format!("ui_anim_{}", e.code), e.message, hint)
}

fn editor(world: &mut World) -> &mut UiAnimEditor {
    world
        .get_resource_or_insert_with(UiAnimEditor::default)
        .into_inner()
}

fn doc(world: &mut World) -> Result<&mut UiAnimDoc, RpcError> {
    editor(world).doc_mut().map_err(rpc)
}

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, RpcError> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a string) is required")))
}

fn opt_str_param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a string"))),
    }
}

fn f32_param(params: &Value, key: &str) -> Result<f32, RpcError> {
    params
        .get(key)
        .and_then(Value::as_f64)
        .map(|n| n as f32)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a number) is required")))
}

fn opt_f32(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|n| Some(n as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be a number"))),
    }
}

fn animation_name(doc: &UiAnimDoc, params: &Value) -> Result<String, RpcError> {
    match opt_str_param(params, "animation")? {
        Some(name) => Ok(name.to_owned()),
        None => doc.selected.clone().ok_or_else(|| {
            RpcError::invalid_params("`animation` is required: the library has none selected")
        }),
    }
}

fn finding_json(f: &Finding) -> Value {
    json!({ "code": f.code, "severity": format!("{:?}", f.severity).to_lowercase(), "text": f.text, "hint": f.hint })
}

fn findings(world: &mut World) -> Result<Vec<Finding>, RpcError> {
    let facts = world
        .get_resource::<UiDesigner>()
        .and_then(UiDesigner::doc)
        .map(|d| node_facts(d.file()));
    let doc = doc(world)?;
    // Without an open `.ui.ron` document there are no nodes to check targets against: only the
    // format's own checks run then, so a target is never reported missing for want of a document.
    let has_document = facts.is_some();
    Ok(validate_ui_anim(doc.library(), &facts.unwrap_or_default())
        .into_iter()
        .filter(|f| has_document || f.code == "ui_anim_invalid")
        .collect())
}

fn def_json(name: &str, def: &UiAnimDef) -> Value {
    json!({
        "name": name,
        "target": target_of(name),
        "duration": def.duration(),
        "mode": serde_json::to_value(def.mode).unwrap_or(Value::Null),
        "loops": def.loops,
        "speed": def.speed,
        "tracks": def.tracks.iter().map(|t| json!({
            "property": t.property,
            "keys": t.keys.iter().map(|k| json!({ "time": k.time, "value": k.value, "ease": k.ease })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "events": def.events.iter().map(|e| json!({ "time": e.time, "name": e.name, "payload": e.payload })).collect::<Vec<_>>(),
    })
}

fn summary(world: &mut World) -> Result<Value, RpcError> {
    let ed = editor(world);
    let (time, reverse) = (ed.time, ed.reverse);
    let doc = ed.doc_mut().map_err(rpc)?;
    Ok(json!({
        "open": true,
        "path": doc.path.as_ref().map(|p| p.display().to_string()),
        "dirty": doc.dirty(),
        "revision": doc.revision(),
        "undo": doc.undo_len(),
        "redo": doc.redo_len(),
        "selected": doc.selected,
        "count": doc.library().animations.len(),
        "time": time,
        "reverse": reverse,
    }))
}

fn ui_anim_open(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let new = params.get("new").and_then(Value::as_bool).unwrap_or(false);
    let path = match opt_str_param(params, "path")? {
        Some(p) if new => Some(doc_path(world, p)?),
        Some(p) => Some(resolve_path(world, p)?),
        None => None,
    };
    let opened = if new {
        UiAnimDoc::new_document(path)
    } else {
        let path = path.ok_or_else(|| RpcError::invalid_params("`path` is required unless `new` is true"))?;
        UiAnimDoc::open(&path).map_err(rpc)?
    };
    editor(world).set_doc(opened);
    ui_anim_list(world, &json!({}))
}

fn ui_anim_list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let found = findings(world)?;
    let mut out = summary(world)?;
    let doc = doc(world)?;
    out["animations"] = Value::Array(
        doc.library()
            .animations
            .iter()
            .map(|(n, d)| def_json(n, d))
            .collect(),
    );
    out["findings"] = Value::Array(found.iter().map(finding_json).collect());
    Ok(out)
}

fn parse_mode(text: &str) -> Result<LoopMode, RpcError> {
    match text {
        "once" => Ok(LoopMode::Once),
        "loop" => Ok(LoopMode::Loop),
        "ping_pong" => Ok(LoopMode::PingPong),
        other => Err(RpcError::new(
            INVALID_PARAMS,
            "ui_anim_bad_value",
            format!("unknown mode `{other}`"),
            "once, loop or ping_pong",
        )),
    }
}

fn key_from(params: &Value) -> Result<AnimKey, RpcError> {
    let time = f32_param(params, "time")?;
    let value: AnimValue = serde_json::from_value(
        params
            .get("value")
            .cloned()
            .ok_or_else(|| RpcError::invalid_params("`value` (a number or [r, g, b, a]) is required"))?,
    )
    .map_err(|_| RpcError::invalid_params("`value` must be a number or four numbers [r, g, b, a]"))?;
    Ok(AnimKey {
        time,
        value,
        ease: opt_str_param(params, "ease")?.map(str::to_owned),
    })
}

fn index_param(params: &Value) -> Result<Option<usize>, RpcError> {
    match params.get("index") {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params("`index` must be a whole number")),
    }
}

fn ui_anim_edit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let op = str_param(params, "op")?;
    let doc = doc(world)?;
    match op {
        "undo" => doc.undo().map_err(rpc)?,
        "redo" => doc.redo().map_err(rpc)?,
        "select" => {
            let name = str_param(params, "animation")?;
            if !doc.library().animations.contains_key(name) {
                return Err(rpc(UiAnimError::new(
                    "unknown_animation",
                    format!("no animation named `{name}`"),
                )));
            }
            doc.selected = Some(name.to_owned());
        }
        // The dope sheet's move (w13, P-223): keys across tracks, one undo step.
        "move_keys" => {
            let name = animation_name(doc, params)?;
            let by = opt_f32(params, "by")?
                .ok_or_else(|| RpcError::invalid_params("`by` (seconds) is required"))?;
            let keys = params
                .get("keys")
                .and_then(Value::as_array)
                .ok_or_else(|| RpcError::invalid_params("`keys` ([{ property, time }]) is required"))?
                .iter()
                .map(|k| -> Result<crate::ui_anim_panel::DopeKey, RpcError> {
                    Ok(crate::ui_anim_panel::DopeKey {
                        property: str_param(k, "property")?.to_owned(),
                        time: opt_f32(k, "time")?
                            .ok_or_else(|| RpcError::invalid_params("each key needs a `time`"))?,
                    })
                })
                .collect::<Result<Vec<_>, RpcError>>()?;
            crate::ui_anim_panel::move_dope_keys(doc, &name, &keys, by).map_err(rpc)?;
        }
        _ => {
            let op = match op {
                "add_animation" => AnimOp::AddAnimation {
                    name: str_param(params, "name")?.to_owned(),
                    property: opt_str_param(params, "property")?.map(str::to_owned),
                },
                "rename_animation" => AnimOp::RenameAnimation {
                    from: animation_name(doc, params)?,
                    to: str_param(params, "to")?.to_owned(),
                },
                "remove_animation" => AnimOp::RemoveAnimation(animation_name(doc, params)?),
                "set_playback" => AnimOp::SetPlayback {
                    name: animation_name(doc, params)?,
                    mode: opt_str_param(params, "mode")?.map(parse_mode).transpose()?,
                    loops: match params.get("loops") {
                        None | Some(Value::Null) => None,
                        Some(v) => Some(
                            v.as_u64()
                                .and_then(|n| u32::try_from(n).ok())
                                .ok_or_else(|| RpcError::invalid_params("`loops` must be a whole number"))?,
                        ),
                    },
                    speed: opt_f32(params, "speed")?,
                },
                "add_track" => AnimOp::AddTrack {
                    name: animation_name(doc, params)?,
                    property: str_param(params, "property")?.to_owned(),
                },
                "remove_track" => AnimOp::RemoveTrack {
                    name: animation_name(doc, params)?,
                    property: str_param(params, "property")?.to_owned(),
                },
                // The event track (`.uianim.ron` v2, ADR-8900).
                "set_event" => AnimOp::SetEvent {
                    name: animation_name(doc, params)?,
                    index: index_param(params)?,
                    event: AnimEvent {
                        time: opt_f32(params, "time")?.ok_or_else(|| {
                            RpcError::invalid_params("set_event: `time` (seconds) is required")
                        })?,
                        name: str_param(params, "event")?.to_owned(),
                        payload: opt_str_param(params, "payload")?.unwrap_or_default().to_owned(),
                    },
                },
                "remove_event" => AnimOp::RemoveEvent {
                    name: animation_name(doc, params)?,
                    index: index_param(params)?
                        .ok_or_else(|| RpcError::invalid_params("remove_event: `index` is required"))?,
                },
                other => {
                    return Err(RpcError::new(
                        INVALID_PARAMS,
                        "ui_anim_bad_value",
                        format!("unknown op `{other}`"),
                        "add_animation, rename_animation, remove_animation, set_playback, add_track, remove_track, move_keys, set_event, remove_event, select, undo, redo",
                    ));
                }
            };
            doc.apply(op).map_err(rpc)?;
        }
    }
    ui_anim_list(world, &json!({}))
}

fn ui_anim_set_key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let key = key_from(params)?;
    let doc = doc(world)?;
    let name = animation_name(doc, params)?;
    doc.apply(AnimOp::SetKey {
        name,
        property: str_param(params, "property")?.to_owned(),
        key,
    })
    .map_err(rpc)?;
    ui_anim_list(world, &json!({}))
}

fn ui_anim_remove_key(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let time = f32_param(params, "time")?;
    let doc = doc(world)?;
    let name = animation_name(doc, params)?;
    doc.apply(AnimOp::RemoveKey {
        name,
        property: str_param(params, "property")?.to_owned(),
        time,
    })
    .map_err(rpc)?;
    ui_anim_list(world, &json!({}))
}

/// `editor.ui_anim_scrub`: the values at a time, and with `preview` the target node laid out with them.
fn ui_anim_scrub(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let time = f32_param(params, "time")?;
    let reverse = params.get("reverse").and_then(Value::as_bool).unwrap_or(false);
    let speed = opt_f32(params, "speed")?;
    let want_preview = params.get("preview").and_then(Value::as_bool).unwrap_or(false);
    let root = project_root(world);
    let (name, def) = {
        let doc = doc(world)?;
        let name = animation_name(doc, params)?;
        let def = doc.library().animations.get(&name).cloned().ok_or_else(|| {
            rpc(UiAnimError::new(
                "unknown_animation",
                format!("no animation named `{name}`"),
            ))
        })?;
        (name, def)
    };
    let values = sample_def(&def, time, reverse, speed).map_err(rpc)?;
    let ed = editor(world);
    ed.time = time;
    ed.reverse = reverse;
    let mut out = json!({
        "animation": name,
        "time": time,
        "duration": def.playback_duration(speed),
        "values": values_json(&values),
    });
    if want_preview {
        let target = match opt_str_param(params, "node")? {
            Some(n) => Some(n.to_owned()),
            None => target_of(&name).map(str::to_owned),
        };
        let designer = world.get_resource::<UiDesigner>().and_then(UiDesigner::doc);
        let (Some(designer), Some(target)) = (designer, target) else {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "ui_anim_bad_value",
                "`preview` needs an open .ui.ron in the designer and a target node (`node`, or a name like `node::label`)",
                "open one with editor.ui_designer_open",
            ));
        };
        let (applied, skipped) = apply_to_file(designer.file(), &target, &values).ok_or_else(|| {
            RpcError::new(
                NOT_FOUND,
                "ui_anim_unknown_node",
                format!("the document has no node named `{target}`"),
                "editor.ui_designer_state lists node names",
            )
        })?;
        let settings = designer.preview.clone();
        let frame = preview(
            &applied,
            &settings,
            root.as_deref().and_then(project_localization),
        )
        .map_err(|e| {
            RpcError::new(
                INVALID_PARAMS,
                &format!("ui_designer_{}", e.code),
                e.message,
                "see editor.ui_designer_preview",
            )
        })?;
        let node = frame.nodes.iter().find(|n| n.name.as_deref() == Some(&target));
        out["preview"] = json!({
            "node": target,
            "rect": node.map(|n| n.rect),
            "visible": node.map(|n| n.visible),
            "not_in_layout": skipped,
        });
    }
    Ok(out)
}

fn ui_anim_validate(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let found = findings(world)?;
    let errors = found
        .iter()
        .filter(|f| f.severity == regia_asset_pipeline::validators::Severity::Error)
        .count();
    Ok(json!({
        "ok": errors == 0,
        "errors": errors,
        "findings": found.iter().map(finding_json).collect::<Vec<_>>(),
    }))
}

/// A `.uianim.ron` path the caller saves or creates, through the shared document-path rule
/// (ADR-7388, audit rc1-sec-paths-z finding 1): another kind of project file is
/// `ui_anim_bad_value`, a path leading out of the project `path_outside_project`.
fn doc_path(world: &World, text: &str) -> Result<std::path::PathBuf, RpcError> {
    if !text.to_ascii_lowercase().ends_with(".uianim.ron") {
        return Err(RpcError::new(
            INVALID_PARAMS,
            "ui_anim_bad_value",
            format!("`path` must name a .uianim.ron file, not {text:?}"),
            "UI animation libraries are assets/ui/<name>.uianim.ron",
        ));
    }
    resolve_path(world, text)
}

fn ui_anim_save(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = match opt_str_param(params, "path")? {
        Some(p) => Some(doc_path(world, p)?),
        None => None,
    };
    let doc = doc(world)?;
    let saved = doc.save(path).map_err(rpc)?;
    let mut out = summary(world)?;
    out["saved"] = json!(saved.display().to_string());
    Ok(out)
}

const ANIMATION: ParamSchema = ParamSchema {
    name: "animation",
    kind: "string",
    required: false,
    description: "The animation (default: the selected one).",
};
const PROPERTY: ParamSchema = ParamSchema {
    name: "property",
    kind: "string",
    required: true,
    description: "A tween property: opacity, background, border_color, text_color, inset_left/top/right/bottom, width, height, translate_x, translate_y, scale.",
};
const TIME: ParamSchema = ParamSchema {
    name: "time",
    kind: "number",
    required: true,
    description: "Seconds from the start of the animation.",
};

/// The `editor.ui_anim_*` methods.
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.ui_anim_open",
                tool: "ui_anim_open",
                description: "Open a .uianim.ron library in the UI animation editor, or start an empty one with new: true. \
                    A malformed or newer file is refused with the format's own error (ui_anim_parse). Replaces the open library.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "The file (relative to the project root, or absolute). With new: true, where it will save.",
                    },
                    ParamSchema {
                        name: "new",
                        kind: "boolean",
                        required: false,
                        description: "Start an empty library instead of reading `path`.",
                    },
                ],
                result: "{ open, path, dirty, revision, undo, redo, selected, count, animations: [{ name, target, duration, mode, loops, speed, tracks: [{ property, keys: [{ time, value, ease }] }] }], findings }",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_anim_open as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_list",
                tool: "ui_anim_list",
                description: "Read the open library: every animation with its tracks and keys, undo state, and the validation findings against the designer's open .ui.ron.",
                params: &[],
                result: "{ open, path, dirty, revision, undo, redo, selected, count, animations, findings: [{ code, severity, text, hint }] }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_anim_list as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_edit",
                tool: "ui_anim_edit",
                description: "One edit of the open library, one undo step: add_animation { name, property? }, rename_animation { animation?, to }, \
                    remove_animation, set_playback { mode?, loops?, speed? }, add_track { property }, remove_track { property }, \
                    move_keys { keys: [{ property, time }], by } (the dope sheet: keys across tracks in one step), \
                    set_event { index?, time, event, payload? } (the event track: replace event `index`, or add one), remove_event { index }, select, undo, redo. \
                    An animation named \"node::label\" targets the node named `node`.",
                params: &[
                    ParamSchema {
                        name: "op",
                        kind: "string",
                        required: true,
                        description: "add_animation, rename_animation, remove_animation, set_playback, add_track, remove_track, move_keys, set_event, remove_event, select, undo or redo.",
                    },
                    ANIMATION,
                    ParamSchema {
                        name: "name",
                        kind: "string",
                        required: false,
                        description: "add_animation: the new name.",
                    },
                    ParamSchema {
                        name: "to",
                        kind: "string",
                        required: false,
                        description: "rename_animation: the new name.",
                    },
                    ParamSchema {
                        name: "property",
                        kind: "string",
                        required: false,
                        description: "add_animation (first track), add_track, remove_track: a tween property.",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "set_playback: once, loop or ping_pong.",
                    },
                    ParamSchema {
                        name: "loops",
                        kind: "integer",
                        required: false,
                        description: "set_playback: passes, 1 to 64.",
                    },
                    ParamSchema {
                        name: "speed",
                        kind: "number",
                        required: false,
                        description: "set_playback: 0.05 to 20.",
                    },
                    ParamSchema {
                        name: "keys",
                        kind: "array",
                        required: false,
                        description: "move_keys: the keys to move, [{ property, time }], across tracks.",
                    },
                    ParamSchema {
                        name: "by",
                        kind: "number",
                        required: false,
                        description: "move_keys: seconds to move them (each new time snapped to 10 ms, at least 0).",
                    },
                    ParamSchema {
                        name: "index",
                        kind: "integer",
                        required: false,
                        description: "set_event (absent: add an event), remove_event: the event's place in the animation's events list.",
                    },
                    ParamSchema {
                        name: "time",
                        kind: "number",
                        required: false,
                        description: "set_event: seconds from the start of one pass, 0 to the animation length.",
                    },
                    ParamSchema {
                        name: "event",
                        kind: "string",
                        required: false,
                        description: "set_event: the event name game code matches (1 to 256 bytes).",
                    },
                    ParamSchema {
                        name: "payload",
                        kind: "string",
                        required: false,
                        description: "set_event: free text handed to the handler (default empty).",
                    },
                ],
                result: "as editor.ui_anim_list",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_anim_edit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_set_key",
                tool: "ui_anim_set_key",
                description: "Set the key at `time` on a track (replacing the one at that time, else inserting in time order; the track is created when missing). One undo step.",
                params: &[
                    ANIMATION,
                    PROPERTY,
                    TIME,
                    ParamSchema {
                        name: "value",
                        kind: "any",
                        required: true,
                        description: "A number, or linear [r, g, b, a] for a colour property.",
                    },
                    ParamSchema {
                        name: "ease",
                        kind: "string",
                        required: false,
                        description: "Ease of the segment that ends at this key (linear when absent).",
                    },
                ],
                result: "as editor.ui_anim_list",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_anim_set_key as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_remove_key",
                tool: "ui_anim_remove_key",
                description: "Remove the key at `time` from a track. The last key of a track cannot be removed (remove the track instead).",
                params: &[ANIMATION, PROPERTY, TIME],
                result: "as editor.ui_anim_list",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_anim_remove_key as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_scrub",
                tool: "ui_anim_scrub",
                description: "Sample an animation at `time` through UiAnimDef::compile and TweenPlan::sample without changing the library or any document. \
                    With preview: true the target node of the designer's open .ui.ron is laid out with the sampled values (on a copy).",
                params: &[
                    ANIMATION,
                    TIME,
                    ParamSchema {
                        name: "reverse",
                        kind: "boolean",
                        required: false,
                        description: "Play the keys backwards.",
                    },
                    ParamSchema {
                        name: "speed",
                        kind: "number",
                        required: false,
                        description: "Replaces the animation's speed.",
                    },
                    ParamSchema {
                        name: "preview",
                        kind: "boolean",
                        required: false,
                        description: "Also lay out the target node with the values.",
                    },
                    ParamSchema {
                        name: "node",
                        kind: "string",
                        required: false,
                        description: "preview: the node name (default: the animation's `node::` prefix).",
                    },
                ],
                result: "{ animation, time, duration, values: { property: number | [r,g,b,a] }, preview?: { node, rect, visible, not_in_layout } }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_anim_scrub as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_validate",
                tool: "ui_anim_validate",
                description: "Check the open library against the nodes of the designer's open .ui.ron: ui_anim_missing_node, ui_anim_ambiguous_node, ui_anim_property_unsupported, ui_anim_invalid.",
                params: &[],
                result: "{ ok, errors, findings: [{ code, severity, text, hint }] }",
                mutating: false,
                permission: Permission::Read,
            },
            ui_anim_validate as Handler,
        ),
        (
            MethodSchema {
                name: "editor.ui_anim_save",
                tool: "ui_anim_save",
                description: "Write the open library (atomically: a temp file and a rename). The text is parsed back with the runtime loader first.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "Where to save (default: where it was opened).",
                }],
                result: "{ saved, path, dirty, revision, undo, redo, selected, count }",
                mutating: true,
                permission: Permission::Assets,
            },
            ui_anim_save as Handler,
        ),
    ]
}
