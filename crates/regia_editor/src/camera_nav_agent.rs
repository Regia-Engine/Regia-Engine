// SPDX-License-Identifier: MPL-2.0
//! The agent surface of [`crate::camera_nav`] (ADR-7755, `book/agents/viewport-tools.md`):
//! the navigation fields of `editor.set_viewport`, `editor.camera_to_selection`,
//! `editor.selection_to_camera` and `editor.viewport_layout`. Errors carry the lowercase codes of
//! [`crate::camera_nav::CameraMoveError::code`] or `bad_layout`, `bad_view`, `bad_clip`,
//! `bad_speed`, `bad_fov`, `bad_preset`.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_ecs::prelude::*;
use serde_json::{Map, Value, json};

use crate::camera_nav::{CameraMoveError, CameraNav, NavPreset};
use crate::viewport::EditorCamera;
use crate::views::{PaneSplit, ViewKind, ViewportLayout};

/// A protocol handler.
pub type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

fn bad(code: &str, message: impl Into<String>) -> RpcError {
    RpcError::new(INVALID_PARAMS, code, message, "see book/agents/viewport-tools.md")
}

fn move_error(err: CameraMoveError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        err.code(),
        err.message(),
        "select an entity with a transform (editor.select)",
    )
}

fn number(params: &Value, name: &str, code: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|x| x as f32)
            .filter(|x| x.is_finite())
            .map(Some)
            .ok_or_else(|| bad(code, format!("`{name}` must be a finite number"))),
    }
}

fn camera_json(camera: &EditorCamera) -> Value {
    json!({
        "target": camera.target.to_array(),
        "distance": camera.distance,
        "yaw": camera.yaw,
        "pitch": camera.pitch,
        "eye": camera.eye().to_array(),
        "speed": camera.speed,
        "fov": camera.fov_deg,
        "near": camera.near,
        "far": camera.far,
    })
}

/// The navigation fields `editor.set_viewport` reads and returns.
pub(crate) fn nav_json(world: &World) -> Map<String, Value> {
    let nav = world.get_resource::<CameraNav>().copied().unwrap_or_default();
    let camera = world.get_resource::<EditorCamera>().cloned().unwrap_or_default();
    let mut out = Map::new();
    out.insert(
        "invert".into(),
        json!({
            "orbit_x": nav.invert.orbit_x,
            "orbit_y": nav.invert.orbit_y,
            "pan_x": nav.invert.pan_x,
            "pan_y": nav.invert.pan_y,
            "fly_y": nav.invert.fly_y,
            "wheel": nav.invert.wheel,
        }),
    );
    out.insert("speed".into(), json!(camera.speed));
    out.insert(
        "speed_step".into(),
        json!(crate::camera_nav::speed_step(camera.speed)),
    );
    out.insert("speed_with_distance".into(), json!(nav.speed_with_distance));
    out.insert("orbit_selection".into(), json!(nav.orbit_selection));
    out.insert("zoom_to_cursor".into(), json!(nav.zoom_to_cursor));
    out.insert("navigation_preset".into(), json!(nav.preset.name()));
    out.insert("fov".into(), json!(camera.fov_deg));
    out.insert("near".into(), json!(camera.near));
    out.insert("far".into(), json!(camera.far));
    out
}

/// Reads the navigation fields of `editor.set_viewport` (all optional) and applies them to the
/// session ([`CameraNav`] and the main camera) only when every one is valid.
///
/// # Errors
///
/// `bad_speed`, `bad_fov`, `bad_clip`, `bad_preset`, or `F0002` for a wrong type.
pub(crate) fn apply_nav_params(world: &mut World, params: &Value) -> Result<(), RpcError> {
    let mut nav = world.get_resource::<CameraNav>().copied().unwrap_or_default();
    let mut camera = world.get_resource::<EditorCamera>().cloned().unwrap_or_default();
    let flag = |name: &str| -> Result<Option<bool>, RpcError> {
        match params.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(RpcError::wrong_type(name, "a boolean")),
        }
    };
    if let Some(invert) = params.get("invert").filter(|v| !v.is_null()) {
        let object = invert
            .as_object()
            .ok_or_else(|| RpcError::wrong_type("invert", "an object of booleans"))?;
        for (name, value) in object {
            let slot = match name.as_str() {
                "orbit_x" => &mut nav.invert.orbit_x,
                "orbit_y" => &mut nav.invert.orbit_y,
                "pan_x" => &mut nav.invert.pan_x,
                "pan_y" => &mut nav.invert.pan_y,
                "fly_y" => &mut nav.invert.fly_y,
                "wheel" => &mut nav.invert.wheel,
                other => {
                    return Err(RpcError::invalid_params(format!(
                        "unknown invert axis `{other}` (orbit_x, orbit_y, pan_x, pan_y, fly_y, wheel)"
                    )));
                }
            };
            *slot = value
                .as_bool()
                .ok_or_else(|| RpcError::wrong_type(&format!("invert.{name}"), "a boolean"))?;
        }
    }
    for (name, slot) in [
        ("orbit_selection", &mut nav.orbit_selection),
        ("zoom_to_cursor", &mut nav.zoom_to_cursor),
        ("speed_with_distance", &mut nav.speed_with_distance),
    ] {
        if let Some(on) = flag(name)? {
            *slot = on;
        }
    }
    if let Some(name) = params.get("navigation_preset").filter(|v| !v.is_null()) {
        let name = name
            .as_str()
            .ok_or_else(|| RpcError::wrong_type("navigation_preset", "a string"))?;
        nav.preset = NavPreset::from_name(name).ok_or_else(|| {
            bad(
                "bad_preset",
                format!("unknown navigation preset `{name}` (regia_default, three_button, numpad)"),
            )
        })?;
    }
    if let Some(speed) = number(params, "speed", "bad_speed")? {
        let (min, max) = crate::camera_nav::SPEED_RANGE;
        if !(min..=max).contains(&speed) {
            return Err(bad(
                "bad_speed",
                format!("speed {speed} is outside {min}..={max} m/s"),
            ));
        }
        camera.speed = speed;
    }
    if let Some(step) = params.get("speed_step").filter(|v| !v.is_null()) {
        let step = step
            .as_u64()
            .filter(|s| (1..=crate::camera_nav::SPEED_STEPS.len() as u64).contains(s))
            .ok_or_else(|| bad("bad_speed", "`speed_step` must be a whole number from 1 to 8"))?;
        camera.speed = crate::camera_nav::speed_for_step(step as usize);
    }
    if let Some(fov) = number(params, "fov", "bad_fov")? {
        if !(20.0..=120.0).contains(&fov) {
            return Err(bad("bad_fov", format!("fov {fov} is outside 20..=120 degrees")));
        }
        camera.fov_deg = fov;
    }
    let near = number(params, "near", "bad_clip")?.unwrap_or(camera.near);
    let far = number(params, "far", "bad_clip")?.unwrap_or(camera.far);
    crate::viewport::check_clip(near, far).map_err(|message| bad("bad_clip", message))?;
    (camera.near, camera.far) = (near, far);
    world.insert_resource(nav);
    if world.get_resource::<EditorCamera>() != Some(&camera) {
        world.insert_resource(camera);
    }
    Ok(())
}

/// `editor.camera_to_selection`.
fn camera_to_selection(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    let moved = crate::camera_nav::move_camera_to_selection(world, &camera, crate::viewport_ops::MAIN_CAMERA)
        .map_err(move_error)?;
    world.insert_resource(moved.clone());
    Ok(json!({ "from": camera_json(&camera), "camera": camera_json(&moved) }))
}

/// `editor.selection_to_camera`.
fn selection_to_camera(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    let transform = crate::camera_nav::selection_to_camera(world, &camera).map_err(move_error)?;
    let entity = world
        .get_resource::<crate::selection::Selection>()
        .and_then(crate::selection::Selection::primary)
        .map(Entity::to_bits);
    Ok(json!({
        "entity": entity,
        "translation": transform.translation.to_array(),
        "rotation": transform.rotation.to_array(),
        "scale": transform.scale.to_array(),
        "undoable": true,
    }))
}

fn layout_json(layout: &ViewportLayout) -> Value {
    let shown = if layout.quad { layout.split.pane_count() } else { 1 };
    let panes: Vec<Value> = (0..shown)
        .map(|i| {
            let index = if layout.quad { i } else { layout.active.min(3) };
            json!({ "index": index, "kind": layout.panes[index].kind.name() })
        })
        .collect();
    json!({ "layout": layout.layout_name(), "active": layout.active, "panes": panes })
}

/// `editor.viewport_layout`.
fn viewport_layout(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut layout = world.get_resource_or_insert_with(ViewportLayout::default).clone();
    if let Some(name) = params.get("preset").filter(|v| !v.is_null()) {
        let name = name
            .as_str()
            .ok_or_else(|| RpcError::wrong_type("preset", "a string"))?;
        if !layout.set_layout_name(name) {
            let names: Vec<&str> = std::iter::once("single")
                .chain(PaneSplit::ALL.iter().map(|s| s.name()))
                .collect();
            return Err(bad(
                "bad_layout",
                format!("unknown layout `{name}` ({})", names.join(", ")),
            ));
        }
    }
    if let Some(kinds) = params.get("kinds").filter(|v| !v.is_null()) {
        let kinds = kinds
            .as_array()
            .filter(|k| k.len() <= 4)
            .ok_or_else(|| RpcError::wrong_type("kinds", "an array of up to 4 view names"))?;
        let mut next = layout.panes.clone();
        for (index, kind) in kinds.iter().enumerate() {
            if kind.is_null() {
                continue;
            }
            let name = kind.as_str().unwrap_or_default();
            let kind = ViewKind::from_name(name).ok_or_else(|| {
                bad(
                    "bad_view",
                    format!(
                        "unknown view `{name}` for pane {index} (perspective, top, front, side, bottom, back, opposite_side)"
                    ),
                )
            })?;
            if (index == 0) != (kind == ViewKind::Perspective) {
                return Err(bad(
                    "bad_view",
                    format!("pane {index}: pane 0 is the perspective pane and the others are orthographic"),
                ));
            }
            next[index].kind = kind;
        }
        layout.panes = next;
    }
    let out = layout_json(&layout);
    world.insert_resource(layout);
    Ok(out)
}

/// The methods of this module.
#[must_use]
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.camera_to_selection",
                tool: "camera_to_selection",
                description: "Move camera to selection: the perspective camera's eye goes to the primary selected \
                    entity, looking along its forward (-Z), so a selected camera or light is looked through. \
                    The orbit distance, speed, field of view and clip planes stay.",
                params: &[],
                result: "{ from, camera }",
                mutating: false,
                permission: Permission::Read,
            },
            camera_to_selection as Handler,
        ),
        (
            MethodSchema {
                name: "editor.selection_to_camera",
                tool: "selection_to_camera",
                description: "Move selection to camera: the primary selected entity goes to the perspective camera's \
                    eye, facing where it looks (-Z forward), keeping its scale. One undo step.",
                params: &[],
                result: "{ entity, translation, rotation, scale, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            selection_to_camera as Handler,
        ),
        (
            MethodSchema {
                name: "editor.viewport_layout",
                tool: "viewport_layout",
                description: "The main viewport's pane layout: 1 pane, 2 side by side or stacked, 3 (one large plus \
                    two) or 4. Each pane keeps its own camera and view kind. Without params, reads it.",
                params: &[
                    ParamSchema {
                        name: "preset",
                        kind: "string",
                        required: false,
                        description: "single, side_by_side, stacked, one_large_two or quad.",
                    },
                    ParamSchema {
                        name: "kinds",
                        kind: "array",
                        required: false,
                        description: "View kind per pane index (null keeps one): pane 0 perspective, panes 1-3 one of \
                            top, front, side, bottom, back, opposite_side.",
                    },
                ],
                result: "{ layout, active, panes: [{ index, kind }] }",
                mutating: false,
                permission: Permission::Read,
            },
            viewport_layout as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(EditorCamera::default());
        world
    }

    #[test]
    fn set_viewport_fields_apply_together_or_not_at_all() {
        let mut w = world();
        apply_nav_params(
            &mut w,
            &json!({ "invert": { "orbit_x": true, "wheel": true }, "speed_step": 6, "near": 0.5, "far": 800.0,
                     "orbit_selection": true, "navigation_preset": "three_button", "fov": 75.0 }),
        )
        .expect("valid");
        let nav = *w.resource::<CameraNav>();
        assert!(nav.invert.orbit_x && nav.invert.wheel && !nav.invert.pan_x && nav.orbit_selection);
        assert_eq!(nav.preset, NavPreset::ThreeButton);
        let camera = w.resource::<EditorCamera>().clone();
        assert_eq!(
            (camera.speed, camera.near, camera.far, camera.fov_deg),
            (24.0, 0.5, 800.0, 75.0)
        );
        let out = nav_json(&w);
        assert_eq!(out["speed_step"], json!(6));
        // One bad field: nothing changes.
        let err = apply_nav_params(&mut w, &json!({ "orbit_selection": false, "near": 90.0 }))
            .expect_err("far < 10 near");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("bad_clip"))
        );
        assert!(w.resource::<CameraNav>().orbit_selection);
        for (params, code) in [
            (json!({ "speed": 0.0 }), "bad_speed"),
            (json!({ "speed_step": 9 }), "bad_speed"),
            (json!({ "fov": 5.0 }), "bad_fov"),
            (json!({ "navigation_preset": "other" }), "bad_preset"),
        ] {
            let err = apply_nav_params(&mut w, &params).expect_err("bad");
            assert_eq!(
                err.data.as_ref().and_then(|d| d.get("code")),
                Some(&json!(code)),
                "{params}"
            );
        }
    }

    #[test]
    fn layouts_and_pane_kinds_are_set_by_name() {
        let mut w = world();
        let out = viewport_layout(
            &mut w,
            &json!({ "preset": "one_large_two", "kinds": [null, "bottom", "back"] }),
        )
        .expect("ok");
        assert_eq!(out["layout"], json!("one_large_two"));
        assert_eq!(out["panes"].as_array().map(Vec::len), Some(3));
        assert_eq!(out["panes"][1]["kind"], json!("bottom"));
        let err = viewport_layout(&mut w, &json!({ "preset": "nine" })).expect_err("bad");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("bad_layout"))
        );
        let err = viewport_layout(&mut w, &json!({ "kinds": ["top"] })).expect_err("pane 0");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("bad_view"))
        );
        let err = camera_to_selection(&mut w, &json!({})).expect_err("nothing selected");
        assert_eq!(
            err.data.as_ref().and_then(|d| d.get("code")),
            Some(&json!("nothing_selected"))
        );
    }
}
