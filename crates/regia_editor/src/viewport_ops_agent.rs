// SPDX-License-Identifier: MPL-2.0
//! The agent surface of [`crate::viewport_ops`] (ADR-7204, `book/agents/viewport-tools.md`):
//! `editor.view_preset`, `editor.camera_ease`, `editor.set_pivot_mode`, `editor.set_snap`,
//! `editor.gizmo_drag`, `editor.menu_invoke`, `editor.outliner_set`, `editor.set_tool` and
//! `editor.rotate_step`.

use regia_agent::protocol::{INVALID_PARAMS, NOT_FOUND, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport::{EditorCamera, GizmoState};
use crate::viewport_ops::{
    CameraEases, GizmoDelta, GizmoPivot, MAIN_CAMERA, MenuError, PivotMode, ViewPreset, gizmo_drag,
    invoke_menu,
};

/// A protocol handler.
pub type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn camera_json(camera: &EditorCamera) -> Value {
    json!({
        "target": camera.target.to_array(),
        "distance": camera.distance,
        "yaw": camera.yaw,
        "pitch": camera.pitch,
        "eye": camera.eye().to_array(),
    })
}

fn vec3_param(params: &Value, name: &str) -> Result<Option<Vec3>, RpcError> {
    let Some(value) = params.get(name).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params(format!("`{name}` must be an array of 3 finite numbers"));
    let items = value.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().ok_or_else(bad)? as f32;
        if !slot.is_finite() {
            return Err(bad());
        }
    }
    Ok(Some(Vec3::from_array(out)))
}

fn entity_param(world: &World, params: &Value, name: &str) -> Result<Option<Entity>, RpcError> {
    let Some(value) = params.get(name).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bits = value
        .as_u64()
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be an entity id")))?;
    crate::command::resolve_entity(world, bits)
        .map(Some)
        .ok_or_else(|| {
            RpcError::not_found(format!("no entity {bits}"), "list entities with editor.hierarchy")
        })
}

/// `editor.view_preset`.
fn view_preset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = params.get("preset").and_then(Value::as_str).ok_or_else(|| {
        RpcError::invalid_params(
            "`preset` is required (perspective, top, front, side, bottom, back, opposite_side)",
        )
    })?;
    let preset = ViewPreset::from_name(name).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "unknown preset `{name}` (perspective, top, front, side, bottom, back, opposite_side)"
        ))
    })?;
    let instant = params.get("instant").and_then(Value::as_bool).unwrap_or(false);
    let from = world.get_resource_or_insert_with(EditorCamera::default).clone();
    let to = preset.pose(&from);
    let mut eases = world.get_resource_or_insert_with(CameraEases::default).clone();
    if instant {
        eases.moves.remove(&MAIN_CAMERA);
        world.insert_resource(to.clone());
    } else {
        eases.start(MAIN_CAMERA, from.clone(), to.clone());
    }
    world.insert_resource(eases);
    Ok(json!({
        "preset": preset.name(),
        "from": camera_json(&from),
        "to": camera_json(&to),
        "duration": if instant { 0.0 } else { crate::viewport_ops::EASE_SECONDS },
    }))
}

/// `editor.camera_ease`.
fn camera_ease(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let advance = match params.get("advance") {
        None | Some(Value::Null) => 0.0,
        Some(v) => v
            .as_f64()
            .filter(|s| s.is_finite() && *s >= 0.0)
            .ok_or_else(|| RpcError::invalid_params("`advance` must be a number of seconds >= 0"))?
            as f32,
    };
    let mut eases = world.get_resource_or_insert_with(CameraEases::default).clone();
    let pose = eases.advance(MAIN_CAMERA, advance);
    let running = eases.moves.get(&MAIN_CAMERA).map(|e| (e.elapsed, e.duration));
    world.insert_resource(eases);
    if let Some(pose) = &pose {
        world.insert_resource(pose.clone());
    }
    let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    Ok(json!({
        "camera": camera_json(&camera),
        "active": running.is_some(),
        "elapsed": running.map(|r| r.0),
        "duration": running.map(|r| r.1),
    }))
}

fn pivot_json(world: &mut World) -> Value {
    let mode = world.get_resource_or_insert_with(GizmoPivot::default).0;
    let selected = world
        .get_resource::<crate::selection::Selection>()
        .map(|s| s.entities().to_vec())
        .unwrap_or_default();
    // An offset made for another selection lapses here as it does in the viewport's frame.
    crate::viewport_ops::lapse_pivot_offset(world, &selected);
    let offset = crate::viewport_ops::pivot_offset_for(world, &selected);
    json!({ "mode": mode.name(), "offset": offset.to_array() })
}

/// `editor.set_pivot_mode`.
fn set_pivot_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(name) = params.get("mode").and_then(Value::as_str) {
        let mode = PivotMode::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown mode `{name}` (median, individual, active, bounds)"
            ))
        })?;
        world.insert_resource(GizmoPivot(mode));
    }
    // The temporary pivot offset of the current selection (ADR-7756); null or [0,0,0] clears it.
    if let Some(value) = params.get("offset") {
        let offset = if value.is_null() {
            Vec3::ZERO
        } else {
            vec3_param(params, "offset")?.unwrap_or(Vec3::ZERO)
        };
        let selected = world
            .get_resource::<crate::selection::Selection>()
            .map(|s| s.entities().to_vec())
            .unwrap_or_default();
        if selected.is_empty() && offset != Vec3::ZERO {
            return Err(RpcError::new(
                INVALID_PARAMS,
                "pivot_offset_no_selection",
                "a pivot offset belongs to a selection, and nothing is selected",
                "select entities with editor.select first",
            ));
        }
        crate::viewport_ops::set_pivot_offset(world, &selected, offset);
    }
    Ok(pivot_json(world))
}

/// `editor.set_snap`: the toolbar's snap toggle and step menus; the steps are saved to the project
/// (`viewport_ops::set_snap_steps`, ADR-7756).
fn set_snap(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    crate::viewport_ops::ensure_snap_steps(world);
    let mut gizmo = world.get_resource_or_insert_with(GizmoState::default).clone();
    let step = |name: &str| -> Result<Option<f32>, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(v) => v
                .as_f64()
                .map(|x| x as f32)
                .filter(|x| x.is_finite() && *x > 0.0)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number > 0"))),
        }
    };
    let (distance, angle, scale) = (step("distance")?, step("angle_deg")?, step("scale")?);
    if let Some(enabled) = params.get("enabled") {
        gizmo.snap = enabled
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?;
    }
    world.insert_resource(gizmo);
    let mut saved = None;
    if distance.is_some() || angle.is_some() || scale.is_some() {
        saved = crate::viewport_ops::set_snap_steps(world, distance, angle, scale).map_err(|e| {
            RpcError::new(
                regia_agent::protocol::INTERNAL_ERROR,
                e.code(),
                e.to_string(),
                "check that the project's .regia directory is writable",
            )
        })?;
    }
    let gizmo = world.get_resource_or_insert_with(GizmoState::default).clone();
    Ok(json!({
        "enabled": gizmo.snap,
        "distance": gizmo.snap_distance,
        "angle_deg": gizmo.snap_angle_deg,
        "scale": gizmo.snap_scale,
        "saved": saved.map(|p| p.display().to_string()),
    }))
}

/// `editor.set_tool`: the viewport tool (Q select, W translate, E rotate, R scale).
fn set_tool(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if let Some(value) = params.get("tool").filter(|v| !v.is_null()) {
        let name = value.as_str().unwrap_or_default();
        let mode = crate::viewport::GizmoMode::from_name(name).ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "tool_unknown",
                format!("tool {value} (select, translate, rotate, scale)"),
                "pass one of select, translate, rotate, scale",
            )
        })?;
        world.get_resource_or_insert_with(GizmoState::default).mode = mode;
    }
    let mode = world.get_resource_or_insert_with(GizmoState::default).mode;
    Ok(json!({ "tool": mode.name(), "gizmo": mode != crate::viewport::GizmoMode::Select }))
}

/// `editor.rotate_step`: the U / Shift+U keys.
fn rotate_step_call(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let back = params.get("back").and_then(Value::as_bool).unwrap_or(false);
    let outcome = crate::viewport_ops::rotate_step(world, if back { -1.0 } else { 1.0 })
        .map_err(|e| crate::hierarchy_agent::hierarchy_rpc_error(&e))?;
    let degrees = world
        .get_resource_or_insert_with(GizmoState::default)
        .snap_angle_deg;
    let moved: Vec<Value> = outcome
        .moved
        .iter()
        .map(|e| {
            let t = world
                .get::<regia_scene::prelude::Transform>(*e)
                .copied()
                .unwrap_or_default();
            json!({ "entity": e.to_bits(), "translation": t.translation.to_array(), "rotation": t.rotation.to_array() })
        })
        .collect();
    Ok(json!({
        "moved": moved,
        "degrees": if back { -degrees } else { degrees },
        "pivot": outcome.pivot.map(|p| p.to_array()),
    }))
}

/// `editor.gizmo_drag`.
fn gizmo_drag_call(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities = crate::hierarchy_agent::entities_param(world, params)?;
    let active = entity_param(world, params, "active")?;
    let translation = vec3_param(params, "translate")?.unwrap_or(Vec3::ZERO);
    let rotation = vec3_param(params, "rotate_deg")?.map_or(Quat::IDENTITY, |deg| {
        Quat::from_euler(
            regia_core::glam::EulerRot::YXZ,
            deg.y.to_radians(),
            deg.x.to_radians(),
            deg.z.to_radians(),
        )
    });
    let scale = vec3_param(params, "scale")?.unwrap_or(Vec3::ONE);
    let duplicate = params.get("duplicate").and_then(Value::as_bool).unwrap_or(false);
    // `space` (ADR-7756): the axes `translate` and `rotate_deg` are given in; absent = the parent's.
    let delta = match params.get("space").filter(|v| !v.is_null()) {
        None => GizmoDelta {
            translation,
            rotation,
            scale,
        },
        Some(value) => {
            let space = crate::viewport::GizmoSpace::from_name(value.as_str().unwrap_or_default())
                .ok_or_else(|| {
                    RpcError::new(
                        INVALID_PARAMS,
                        "gizmo_space_unknown",
                        format!("space {value} (world, local, parent, view)"),
                        "pass one of world, local, parent, view",
                    )
                })?;
            let anchor = active
                .or_else(|| {
                    world
                        .get_resource::<crate::selection::Selection>()
                        .and_then(crate::selection::Selection::primary)
                        .filter(|p| entities.contains(p))
                })
                .or_else(|| entities.last().copied())
                .ok_or_else(|| {
                    crate::hierarchy_agent::hierarchy_rpc_error(
                        &crate::hierarchy_ops::HierarchyError::NoEntities,
                    )
                })?;
            let parent = crate::viewport_ops::parent_affine(world, anchor);
            let local = world
                .get::<regia_scene::prelude::Transform>(anchor)
                .copied()
                .unwrap_or_default();
            let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
            let view = crate::views::ViewCamera::of(crate::views::ViewKind::Perspective, &camera);
            let (_, parent_rotation, _) = parent.to_scale_rotation_translation();
            let basis = crate::viewport_ops::space_basis(
                space,
                crate::viewport_ops::to_world(parent, &local).rotation,
                parent_rotation,
                crate::viewport_ops::view_rotation(&view),
            );
            crate::viewport_ops::delta_in_parent(basis, parent, translation, rotation, scale)
        }
    };
    let outcome = gizmo_drag(world, &entities, active, &delta, duplicate, true)
        .map_err(|e| crate::hierarchy_agent::hierarchy_rpc_error(&e))?;
    let moved: Vec<Value> = outcome
        .moved
        .iter()
        .map(|e| {
            let t = world
                .get::<regia_scene::prelude::Transform>(*e)
                .copied()
                .unwrap_or_default();
            json!({
                "entity": e.to_bits(),
                "translation": t.translation.to_array(),
                "rotation": t.rotation.to_array(),
                "scale": t.scale.to_array(),
            })
        })
        .collect();
    Ok(json!({
        "moved": moved,
        "copies": outcome.copies.iter().map(|e| e.to_bits()).collect::<Vec<_>>(),
        "pivot": outcome.pivot.map(|p| p.to_array()),
        "pivot_mode": world.get_resource::<GizmoPivot>().copied().unwrap_or_default().0.name(),
    }))
}

fn menu_rpc_error(err: &MenuError) -> RpcError {
    let (code, regia_code, hint) = match err {
        MenuError::UnknownPath(_) => (
            INVALID_PARAMS,
            "F0002",
            "menu paths are listed in book/agents/viewport-tools.md (File/Save all, Edit/Copy, ...)",
        ),
        MenuError::NothingSelected => (
            INVALID_PARAMS,
            "F0002",
            "select entities with editor.select first",
        ),
        MenuError::ClipboardEmpty => (NOT_FOUND, "F0003", "copy or cut entities first"),
        MenuError::NoRecent(_) => (NOT_FOUND, "F0003", "list them with path File/Recent"),
        MenuError::Failed(_) => (
            regia_agent::protocol::INTERNAL_ERROR,
            "F0001",
            "see logs.read for details",
        ),
    };
    let mut error = RpcError::new(code, regia_code, err.to_string(), hint);
    if let Some(data) = error.data.as_mut() {
        data["menu_error"] = json!(err.code());
    }
    error
}

/// `editor.menu_invoke`.
fn menu_invoke(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = params
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`path` is required (for example \"Edit/Copy\")"))?;
    let mut out = invoke_menu(world, path).map_err(|e| menu_rpc_error(&e))?;
    out["path"] = json!(path);
    Ok(out)
}

/// `editor.outliner_set`.
fn outliner_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = entity_param(world, params, "entity")?
        .ok_or_else(|| RpcError::invalid_params("`entity` is required"))?;
    if let Some(visible) = params.get("visible").filter(|v| !v.is_null()) {
        let visible = visible
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`visible` must be a boolean"))?;
        crate::viewport_ops::set_visible(world, entity, visible);
    }
    if let Some(locked) = params.get("locked").filter(|v| !v.is_null()) {
        let locked = locked
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`locked` must be a boolean"))?;
        crate::viewport_ops::set_locked(world, entity, locked);
    }
    // Row color, unique-name marker, badges and warnings (ADR-7772).
    let row_error = |code: &str| {
        RpcError::new(
            regia_agent::protocol::INVALID_PARAMS,
            code,
            format!("outliner_set: {code}"),
            "see book/agents/outliner.md",
        )
    };
    match params.get("color") {
        None => {}
        Some(Value::Null) => {
            crate::outliner_columns::set_row_color(world, entity, None).map_err(row_error)?
        }
        Some(Value::String(color)) if color.is_empty() => {
            crate::outliner_columns::set_row_color(world, entity, None).map_err(row_error)?;
        }
        Some(Value::String(color)) => {
            crate::outliner_columns::set_row_color(world, entity, Some(color)).map_err(row_error)?;
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`color` must be a color name, \"\" or null",
            ));
        }
    }
    if let Some(unique) = params.get("unique").filter(|v| !v.is_null()) {
        let unique = unique
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`unique` must be a boolean"))?;
        crate::outliner_columns::set_unique_name(world, entity, unique).map_err(row_error)?;
    }
    crate::outliner_columns::enforce_unique_names(world);
    crate::config_checks::update(world);
    let (prefab, overrides) = crate::viewport_ops::prefab_marks(world, entity);
    let warnings: Vec<&str> = world
        .get_resource::<crate::config_checks::ConfigChecks>()
        .map(|c| c.warnings_of(entity).iter().map(|w| w.code).collect())
        .unwrap_or_default();
    Ok(json!({
        "entity": entity.to_bits(),
        "visible": crate::viewport_ops::is_visible(world, entity),
        "locked": world.get::<crate::viewport_ops::EditorLocked>(entity).is_some(),
        "prefab": prefab,
        "prefab_overrides": overrides,
        "color": crate::outliner_columns::row_color_of(world, entity),
        "unique": world.get::<regia_scene::unique_name::UniqueName>(entity).is_some(),
        "badges": crate::outliner_columns::badges(world, entity).iter().map(|b| b.name()).collect::<Vec<_>>(),
        "warnings": warnings,
    }))
}

const P_ENTITIES: ParamSchema = ParamSchema {
    name: "entities",
    kind: "array",
    required: false,
    description: "Entity ids; absent = the editor selection.",
};

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.view_preset",
                tool: "view_preset",
                description: "Turn the perspective camera to a view preset (the panel's 1-4 keys, Alt+2-4 for \
                    bottom, back and the opposite side) with the eased move the panel makes; advance it with \
                    editor.camera_ease.",
                params: &[
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: true,
                        description: "perspective, top, front, side, bottom, back or opposite_side.",
                    },
                    ParamSchema {
                        name: "instant",
                        kind: "boolean",
                        required: false,
                        description: "Jump without easing (default false).",
                    },
                ],
                result: "{ preset, from, to, duration }",
                mutating: false,
                permission: Permission::Read,
            },
            view_preset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.camera_ease",
                tool: "camera_ease",
                description: "Advance the perspective camera's eased move (view preset, focus) by a number of \
                    seconds and read the camera.",
                params: &[ParamSchema {
                    name: "advance",
                    kind: "number",
                    required: false,
                    description: "Seconds to advance (default 0: read only).",
                }],
                result: "{ camera, active, elapsed, duration }",
                mutating: false,
                permission: Permission::Read,
            },
            camera_ease as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_pivot_mode",
                tool: "set_pivot_mode",
                description: "The gizmo pivot for multi-selections: median point, individual origins, the \
                    active element or the center of the selection's mesh bounds; and the temporary pivot \
                    offset of the current selection (lapses when the selection changes). Without \
                    parameters, reads them.",
                params: &[
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "median, individual, active or bounds.",
                    },
                    ParamSchema {
                        name: "offset",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] world meters added to the pivot of the current selection; null or [0, 0, 0] clears it.",
                    },
                ],
                result: "{ mode, offset }",
                mutating: false,
                permission: Permission::Read,
            },
            set_pivot_mode as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_snap",
                tool: "set_snap",
                description: "The gizmo snap toggle (Ctrl inverts it while held) and its steps: the viewport \
                    toolbar's Snap toggle and Grid, Angle and Scale menus. Steps are saved to the open \
                    project's .regia/viewport_snap.ron (saved: its path, or null without a project).",
                params: &[
                    ParamSchema {
                        name: "enabled",
                        kind: "boolean",
                        required: false,
                        description: "Snap on or off.",
                    },
                    ParamSchema {
                        name: "distance",
                        kind: "number",
                        required: false,
                        description: "Translation step, metres (> 0).",
                    },
                    ParamSchema {
                        name: "angle_deg",
                        kind: "number",
                        required: false,
                        description: "Rotation step, degrees (> 0).",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "number",
                        required: false,
                        description: "Scale step (> 0).",
                    },
                ],
                result: "{ enabled, distance, angle_deg, scale, saved }",
                mutating: false,
                permission: Permission::Read,
            },
            set_snap as Handler,
        ),
        (
            MethodSchema {
                name: "editor.gizmo_drag",
                tool: "gizmo_drag",
                description: "One gizmo drag over entities about the pivot mode's point, as one undo step; \
                    `duplicate` is the Alt+drag duplicate (the copies move, the originals stay).",
                params: &[
                    P_ENTITIES,
                    ParamSchema {
                        name: "active",
                        kind: "number",
                        required: false,
                        description: "The active entity for pivot mode active (default: the selection's primary).",
                    },
                    ParamSchema {
                        name: "translate",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] meters added, along the axes of `space` (default: the parent's).",
                    },
                    ParamSchema {
                        name: "rotate_deg",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] Euler degrees (applied yaw Y, then X, then Z) about the pivot, about the axes of `space`.",
                    },
                    ParamSchema {
                        name: "space",
                        kind: "string",
                        required: false,
                        description: "world, local (the active entity's axes), parent (default) or view (the perspective camera's right, up, back).",
                    },
                    ParamSchema {
                        name: "scale",
                        kind: "array",
                        required: false,
                        description: "[x, y, z] factors about the pivot.",
                    },
                    ParamSchema {
                        name: "duplicate",
                        kind: "boolean",
                        required: false,
                        description: "Duplicate first and move the copies (Alt+drag).",
                    },
                ],
                result: "{ moved: [{ entity, translation, rotation, scale }], copies, pivot, pivot_mode }",
                mutating: true,
                permission: Permission::EditScene,
            },
            gizmo_drag_call as Handler,
        ),
        (
            MethodSchema {
                name: "editor.menu_invoke",
                tool: "menu_invoke",
                description: "Run a File or Edit menu item by path: File/Save all, File/Recent[/n], File/Exit, \
                    Edit/Undo, Edit/Redo, Edit/Cut, Edit/Copy, Edit/Paste, Edit/Duplicate, Edit/Delete, \
                    Edit/Select all.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Menu/Item, for example \"Edit/Paste\".",
                }],
                result: "{ path, ... what the item did }",
                mutating: true,
                permission: Permission::EditScene,
            },
            menu_invoke as Handler,
        ),
        (
            MethodSchema {
                name: "editor.outliner_set",
                tool: "outliner_set",
                description: "The Outliner's visibility (an undo step) and lock (not picked or box-selected) \
                    toggles of an entity, its row color and its unique-name marker; reports its prefab link and \
                    override marks, badges and configuration warnings.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity id.",
                    },
                    ParamSchema {
                        name: "visible",
                        kind: "boolean",
                        required: false,
                        description: "Show or hide in the viewport.",
                    },
                    ParamSchema {
                        name: "locked",
                        kind: "boolean",
                        required: false,
                        description: "Lock or unlock.",
                    },
                    ParamSchema {
                        name: "color",
                        kind: "string",
                        required: false,
                        description: "Row color: orange, sky, green, yellow, blue, red, pink, gray, brown or lime \
                            (\"\" or null clears it); editor-only view state.",
                    },
                    ParamSchema {
                        name: "unique",
                        kind: "boolean",
                        required: false,
                        description: "Add or remove the saved UniqueName marker (one undo step).",
                    },
                ],
                result: "{ entity, visible, locked, prefab, prefab_overrides, color, unique, badges, warnings }",
                mutating: true,
                permission: Permission::EditScene,
            },
            outliner_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_tool",
                tool: "set_tool",
                description: "The viewport tool (ADR-7756): select (Q, no gizmo: a click picks, a drag \
                    box-selects), translate (W), rotate (E) or scale (R). Without `tool`, reads it.",
                params: &[ParamSchema {
                    name: "tool",
                    kind: "string",
                    required: false,
                    description: "select, translate, rotate or scale.",
                }],
                result: "{ tool, gizmo }",
                mutating: false,
                permission: Permission::Read,
            },
            set_tool as Handler,
        ),
        (
            MethodSchema {
                name: "editor.rotate_step",
                tool: "rotate_step",
                description: "Turn the selection by the snap angle step about the world up axis through the \
                    pivot, as one undo step (the viewport's U key; Shift+U with back).",
                params: &[ParamSchema {
                    name: "back",
                    kind: "boolean",
                    required: false,
                    description: "Turn the other way (default false).",
                }],
                result: "{ moved: [{ entity, translation, rotation }], degrees, pivot }",
                mutating: true,
                permission: Permission::EditScene,
            },
            rotate_step_call as Handler,
        ),
    ]
}
