// SPDX-License-Identifier: MPL-2.0
//! The agent-protocol surface of independent viewports ([`crate::viewport_instance`],
//! `book/agents/viewports.md`): list the world's instances, edit one (camera, quality preset,
//! layout, show flags), link or unlink cameras, capture one headless, and remove one.
//!
//! Edits land in the [`ViewportInstances`] registry; a panel drawing the instance applies them
//! on its next draw, and captures use them at once.

use regia_agent::protocol::{INVALID_PARAMS, RpcError};
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::viewport::EditorCamera;
use crate::viewport_agent::{Handler, dim, info_json, str_param};
use crate::viewport_instance::{CameraLinkGroup, ViewportInstance, ViewportInstances};
use crate::views::{ViewKind, ViewportQuality};

const P_ID: ParamSchema = ParamSchema {
    name: "id",
    kind: "number",
    required: true,
    description: "Viewport instance id.",
};

fn id_param(params: &Value) -> Result<u64, RpcError> {
    params
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`id` (a viewport instance id) is required"))
}

fn unknown(id: u64) -> RpcError {
    RpcError::not_found(
        format!("no viewport instance {id}"),
        "list them with editor.viewport_instances, or pass `create: true` to editor.viewport_instance_set",
    )
}

fn camera_json(camera: &EditorCamera) -> Value {
    json!({
        "target": camera.target.to_array(),
        "distance": camera.distance,
        "yaw": camera.yaw,
        "pitch": camera.pitch,
        "fov_deg": camera.fov_deg,
        "eye": camera.eye().to_array(),
    })
}

fn instance_json(registry: &ViewportInstances, id: u64) -> Value {
    let Some(record) = registry.get(id) else {
        return Value::Null;
    };
    let view = &record.instance;
    let show = view.layout.show;
    json!({
        "id": id,
        "live": registry.is_live(id),
        "frame": record.frame,
        "camera": camera_json(&view.camera),
        "quality": view.layout.quality.name(),
        "layout": view.layout.layout_name(),
        "near": view.camera.near,
        "far": view.camera.far,
        "active": view.layout.active,
        "game_view": view.layout.game_view,
        "possessed": view.layout.possessed,
        "show": {
            "grid": show.grid, "icons": show.icons, "bounds": show.bounds, "outline": show.outline,
            "proxies": show.proxies, "stats": show.stats, "preview_light": show.preview_light,
            "audio": show.audio,
        },
        "settings": {
            "grid": view.settings.grid, "labels": view.settings.labels,
            "realtime": view.settings.realtime, "audio": view.settings.audio,
        },
        "link": view.link.as_ref().map(CameraLinkGroup::id),
        "panes": record.panes.iter().map(|pane| json!({
            "index": pane.index,
            "kind": pane.kind.name(),
            "eye": pane.view.eye.to_array(),
            "target": pane.view.target.to_array(),
            "width": pane.size[0],
            "height": pane.size[1],
        })).collect::<Vec<_>>(),
        "fps": record.stats.fps(),
    })
}

fn snapshot(world: &World, id: u64) -> Value {
    world
        .get_resource::<ViewportInstances>()
        .map_or(Value::Null, |registry| instance_json(registry, id))
}

/// `editor.viewport_instances`.
fn viewport_instances(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let list: Vec<Value> = world
        .get_resource::<ViewportInstances>()
        .map(|registry| {
            registry
                .ids()
                .into_iter()
                .map(|id| instance_json(registry, id))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({ "instances": list }))
}

fn vec3_param(params: &Value, name: &str) -> Result<Option<Vec3>, RpcError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let parsed = value.as_array().filter(|a| a.len() == 3).and_then(|a| {
        Some(Vec3::new(
            a[0].as_f64()? as f32,
            a[1].as_f64()? as f32,
            a[2].as_f64()? as f32,
        ))
    });
    parsed
        .map(Some)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be [x, y, z]")))
}

/// The edits an `editor.viewport_instance_set` call asks for, validated before any is applied.
#[derive(Default)]
struct InstanceEdits {
    target: Option<Vec3>,
    numbers: Vec<(&'static str, f32)>,
    quality: Option<ViewportQuality>,
    /// A layout name (`ViewportLayout::set_layout_name`, ADR-7755).
    layout: Option<String>,
    /// Clip planes `(near, far)`, checked together (ADR-7755).
    clip: Option<(Option<f32>, Option<f32>)>,
    flags: Vec<(&'static str, bool)>,
}

const NUMBER_FIELDS: [&str; 4] = ["distance", "yaw", "pitch", "fov_deg"];
const FLAG_FIELDS: [&str; 11] = [
    "grid",
    "labels",
    "realtime",
    "audio",
    "icons",
    "bounds",
    "outline",
    "proxies",
    "stats",
    "preview_light",
    "game_view",
];

impl InstanceEdits {
    fn parse(params: &Value) -> Result<Self, RpcError> {
        let mut edits = Self {
            target: vec3_param(params, "target")?,
            ..Self::default()
        };
        for name in NUMBER_FIELDS {
            if let Some(value) = params.get(name) {
                let number = value
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number")))?;
                edits.numbers.push((name, number as f32));
            }
        }
        if let Some(name) = str_param(params, "quality") {
            edits.quality = Some(
                ViewportQuality::from_name(name)
                    .ok_or_else(|| RpcError::invalid_params(format!("unknown quality `{name}`")))?,
            );
        }
        if let Some(name) = str_param(params, "layout") {
            if !crate::views::ViewportLayout::default().set_layout_name(name) {
                return Err(RpcError::invalid_params(format!(
                    "unknown layout `{name}` (single, quad, side_by_side, stacked, one_large_two)"
                )));
            }
            edits.layout = Some(name.to_owned());
        }
        // Per-instance clip planes (ADR-7755); checked against the instance's other plane on apply.
        let plane = |name: &str| -> Result<Option<f32>, RpcError> {
            match params.get(name) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v
                    .as_f64()
                    .map(|x| x as f32)
                    .filter(|x| x.is_finite())
                    .map(Some)
                    .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a number"))),
            }
        };
        let (near, far) = (plane("near")?, plane("far")?);
        if near.is_some() || far.is_some() {
            edits.clip = Some((near, far));
        }
        for name in FLAG_FIELDS {
            if let Some(value) = params.get(name) {
                let flag = value
                    .as_bool()
                    .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean")))?;
                edits.flags.push((name, flag));
            }
        }
        Ok(edits)
    }

    fn apply(&self, view: &mut ViewportInstance) {
        if let Some(target) = self.target {
            view.camera.target = target;
        }
        for (name, value) in &self.numbers {
            match *name {
                "distance" => view.camera.distance = value.clamp(0.2, 5000.0),
                "yaw" => view.camera.yaw = *value,
                "pitch" => view.camera.pitch = value.clamp(-1.45, 1.45),
                _ => view.camera.fov_deg = value.clamp(5.0, 150.0),
            }
        }
        if let Some(quality) = self.quality {
            view.layout.quality = quality;
        }
        if let Some(name) = &self.layout {
            view.layout.set_layout_name(name);
        }
        if let Some((near, far)) = self.clip {
            view.camera.near = near.unwrap_or(view.camera.near);
            view.camera.far = far.unwrap_or(view.camera.far);
        }
        for (name, value) in &self.flags {
            let show = &mut view.layout.show;
            let slot = match *name {
                "grid" => {
                    show.grid = *value;
                    &mut view.settings.grid
                }
                "audio" => {
                    show.audio = *value;
                    &mut view.settings.audio
                }
                "labels" => &mut view.settings.labels,
                "realtime" => &mut view.settings.realtime,
                "icons" => &mut show.icons,
                "bounds" => &mut show.bounds,
                "outline" => &mut show.outline,
                "proxies" => &mut show.proxies,
                "stats" => &mut show.stats,
                "preview_light" => &mut show.preview_light,
                _ => &mut view.layout.game_view,
            };
            *slot = *value;
        }
    }
}

/// `editor.viewport_instance_set`.
fn viewport_instance_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params)?;
    let create = params.get("create").and_then(Value::as_bool).unwrap_or(false);
    let edits = InstanceEdits::parse(params)?;
    if let Some((near, far)) = edits.clip {
        // The planes are checked together against the instance's current ones (ADR-7755).
        let current = world
            .get_resource::<ViewportInstances>()
            .and_then(|r| r.get(id))
            .map_or_else(EditorCamera::default, |record| record.instance.camera.clone());
        crate::viewport::check_clip(near.unwrap_or(current.near), far.unwrap_or(current.far)).map_err(
            |message| {
                RpcError::new(
                    INVALID_PARAMS,
                    "bad_clip",
                    message,
                    "see book/agents/viewports.md",
                )
            },
        )?;
    }
    ViewportInstances::edit(world, id, create, |view| {
        edits.apply(view);
        // A linked camera that moved leads its group.
        view.sync_link();
    })
    .ok_or_else(|| unknown(id))?;
    Ok(snapshot(world, id))
}

/// `editor.viewport_link`.
fn viewport_link(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params)?;
    let group = match params.get("group") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`group` must be a number or null"))?,
        ),
    };
    // Join the handle another instance already holds, so the members really share it.
    let existing = group.and_then(|group| {
        let registry = world.get_resource::<ViewportInstances>()?;
        registry.ids().into_iter().find_map(|other| {
            registry
                .get(other)?
                .instance
                .link
                .clone()
                .filter(|link| link.id() == group)
        })
    });
    ViewportInstances::edit(world, id, false, |view| match group {
        Some(group) => {
            let handle = existing.unwrap_or_else(|| CameraLinkGroup::new(group));
            view.link_to(&handle);
        }
        None => view.unlink(),
    })
    .ok_or_else(|| unknown(id))?;
    Ok(snapshot(world, id))
}

/// `editor.viewport_instance_remove`.
fn viewport_instance_remove(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params)?;
    Ok(json!({ "id": id, "removed": ViewportInstances::remove(world, id) }))
}

/// `editor.capture_viewport_instance`.
fn capture_viewport_instance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_param(params)?;
    let mut view = world
        .get_resource::<ViewportInstances>()
        .and_then(|registry| registry.get(id))
        .map(|record| record.instance.clone())
        .ok_or_else(|| unknown(id))?;
    // Follow a linked leader that moved since this instance's snapshot.
    view.sync_link();
    let kind = match str_param(params, "view") {
        Some(name) => ViewKind::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!("unknown view `{name}` (perspective, top, front, side)"))
        })?,
        None => ViewKind::Perspective,
    };
    if let Some(name) = str_param(params, "quality") {
        view.layout.quality = ViewportQuality::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown quality `{name}`")))?;
    }
    if let Some(game_view) = params.get("game_view").and_then(Value::as_bool) {
        view.layout.game_view = game_view;
    }
    let device = crate::agent_ext::viewport_device(world)?;
    let pane = view.layout.panes.iter().position(|p| p.kind == kind).unwrap_or(0);
    let (kind, camera) = view.pane_camera(pane);
    let request = crate::render::PaneRequest {
        pane,
        kind,
        camera,
        width: dim(params, "width", 320),
        height: dim(params, "height", 240),
        ids: true,
        warm_frames: params
            .get("warm_frames")
            .and_then(Value::as_u64)
            .map_or(0, |v| v.min(16) as u32),
        debug_view: regia_render::DebugViewMode::Lit,
        reference: None,
    };
    let frame = crate::render::capture_instance_pane(&device, world, &view, &request).ok_or_else(|| {
        RpcError::new(
            regia_agent::protocol::UNSUPPORTED,
            "F0011",
            "the viewport capture produced no image",
            "a Vulkan adapter (lavapipe or hardware) is required",
        )
    })?;
    let png = crate::render::viewport_png_base64(&frame.image).map_err(RpcError::internal)?;
    let report = crate::viewport::measure_viewport(&frame.image);
    Ok(json!({
        "id": id,
        "width": frame.image.width(),
        "height": frame.image.height(),
        "mime": "image/png",
        "png_base64": png,
        "view": kind.name(),
        "eye": frame.view.eye.to_array(),
        "target": frame.view.target.to_array(),
        "sky_luminance": report.sky_luminance,
        "lit_peak": report.lit_peak,
        "shading_range": report.shading_range,
        "selection_highlight": report.selection_highlight,
        "grid_pixels": report.grid_pixels,
        "gizmo_axes": report.gizmo_axes,
        "quality": frame.info.quality,
        "info": info_json(&frame.info),
    }))
}

const fn flag(name: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind: "boolean",
        required: false,
        description,
    }
}

const fn number(name: &'static str, description: &'static str) -> ParamSchema {
    ParamSchema {
        name,
        kind: "number",
        required: false,
        description,
    }
}

const SET_PARAMS: &[ParamSchema] = &[
    P_ID,
    flag("create", "Create the instance when the id is unknown."),
    ParamSchema {
        name: "target",
        kind: "array",
        required: false,
        description: "Orbit target [x, y, z].",
    },
    number("distance", "Orbit distance, meters."),
    number("yaw", "Yaw around +Y, radians."),
    number("pitch", "Pitch above the horizon, radians (clamped to ±1.45)."),
    number("fov_deg", "Vertical field of view, degrees."),
    number(
        "near",
        "Near clip plane of the perspective pane, m (0.001 to 100; ADR-7755).",
    ),
    number(
        "far",
        "Far clip plane of the perspective pane, m (1 to 100000; at least ten times near).",
    ),
    ParamSchema {
        name: "quality",
        kind: "string",
        required: false,
        description: "Quality preset name (editor, game, mobile_low, ..., deck).",
    },
    ParamSchema {
        name: "layout",
        kind: "string",
        required: false,
        description: "single, quad, side_by_side, stacked or one_large_two (ADR-7755).",
    },
    flag("grid", "Reference grid."),
    flag("labels", "Selection name labels."),
    flag("realtime", "Render every frame."),
    flag("audio", "Audio gizmos."),
    flag("icons", "Light/camera/decal/audio icons."),
    flag("bounds", "Selection bounds, light ranges, frustums."),
    flag("outline", "Selection outline."),
    flag("proxies", "Proxy cubes for entities with nothing to draw."),
    flag("stats", "Stats overlay."),
    flag("preview_light", "Preview sun when the level has none."),
    flag("game_view", "Hide every editor helper."),
];

const CAPTURE_PARAMS: &[ParamSchema] = &[
    P_ID,
    ParamSchema {
        name: "view",
        kind: "string",
        required: false,
        description: "Pane: perspective (default), top, front or side.",
    },
    number(
        "width",
        "Frame width in pixels. Default 320, clamped to 64..=2048.",
    ),
    number(
        "height",
        "Frame height in pixels. Default 240, clamped to 64..=2048.",
    ),
    ParamSchema {
        name: "quality",
        kind: "string",
        required: false,
        description: "Preset for this call only. Default: the instance's.",
    },
    flag("game_view", "Hide every editor helper for this call."),
    number("warm_frames", "Frames rendered before the kept one. 0..=16."),
];

/// The independent-viewport methods.
#[must_use]
#[allow(clippy::too_many_lines)] // a schema table
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.viewport_instances",
                tool: "viewport_instances",
                description: "List the world's independent viewports (lightbox, before/after, compare panes): \
                    each one's camera, quality preset, layout, show flags, link group, the panes of its last \
                    draw and whether it is live (drawn this frame or the last).",
                params: &[],
                result: "{ instances: [{ id, live, frame, camera, quality, layout, active, game_view, possessed, \
                    show, settings, link, panes: [{index, kind, eye, target, width, height}], fps }] }",
                mutating: false,
                permission: Permission::Read,
            },
            viewport_instances as Handler,
        ),
        (
            MethodSchema {
                name: "editor.viewport_instance_set",
                tool: "viewport_instance_set",
                description: "Edit one viewport instance: perspective camera, quality preset, single/quad \
                    layout, show flags and game view. The panel applies it on its next draw; a linked \
                    camera that moves leads its group. `create` registers a new instance (seeded from the \
                    main viewport) for headless captures.",
                params: SET_PARAMS,
                result: "the instance, as in editor.viewport_instances",
                mutating: true,
                permission: Permission::EditScene,
            },
            viewport_instance_set as Handler,
        ),
        (
            MethodSchema {
                name: "editor.viewport_link",
                tool: "viewport_link",
                description: "Link a viewport instance's camera to a group (members orbit together; joining \
                    adopts the group's camera) or unlink it with `group: null`.",
                params: &[
                    P_ID,
                    ParamSchema {
                        name: "group",
                        kind: "number",
                        required: false,
                        description: "Link group id; null or absent unlinks.",
                    },
                ],
                result: "the instance, as in editor.viewport_instances",
                mutating: true,
                permission: Permission::EditScene,
            },
            viewport_link as Handler,
        ),
        (
            MethodSchema {
                name: "editor.capture_viewport_instance",
                tool: "capture_viewport_instance",
                description: "Render one viewport instance headless with its own camera, quality preset and \
                    show flags (overlays painted in) and return a PNG, the eye it used, measurements and \
                    frame statistics.",
                params: CAPTURE_PARAMS,
                result: "{ id, width, height, mime, png_base64, view, eye, target, sky_luminance, lit_peak, \
                    shading_range, selection_highlight, grid_pixels, gizmo_axes, quality, info }",
                mutating: false,
                permission: Permission::Read,
            },
            capture_viewport_instance as Handler,
        ),
        (
            MethodSchema {
                name: "editor.viewport_instance_remove",
                tool: "viewport_instance_remove",
                description: "Forget a viewport instance: its registry entry, link membership and \
                    viewport-owned renderers. A panel that draws it again registers it afresh.",
                params: &[P_ID],
                result: "{ id, removed }",
                mutating: true,
                permission: Permission::EditScene,
            },
            viewport_instance_remove as Handler,
        ),
    ]
}
