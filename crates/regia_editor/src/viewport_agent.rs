// SPDX-License-Identifier: MPL-2.0
//! The viewport's agent-protocol surface (`book/agents/editor.md`): captures of any pane at any
//! quality preset, the show flags and layout, GPU picking and box select, focus, camera
//! bookmarks, possession, frame statistics, and the asset viewers.
//!
//! Coordinates are capture pixels: `x`/`y` (and box corners) are in a `width`×`height` frame
//! of the named pane (`view`), the same frame `editor.capture_viewport` returns.

use regia_agent::protocol::RpcError;
use regia_agent::schema::{MethodSchema, ParamSchema, Permission};
use regia_core::math::{Vec2, Vec3};
use regia_ecs::prelude::*;
use serde_json::{Value, json};

use crate::render::{PaneRequest, capture_pane, render_pane};
use crate::selection::{SelectMode, Selection};
use crate::viewport::{EditorCamera, ViewportSettings, ViewportStats};
use crate::views::{CameraBookmarks, ViewKind, ViewportLayout, ViewportQuality};

/// A protocol handler.
pub type Handler = fn(&mut World, &Value) -> Result<Value, RpcError>;

pub(crate) const P_WIDTH: ParamSchema = ParamSchema {
    name: "width",
    kind: "number",
    required: false,
    description: "Frame width in pixels. Default 320, clamped to 64..=16384; a capture past the device's texture limit answers screenshot_needs_tiles.",
};
pub(crate) const P_HEIGHT: ParamSchema = ParamSchema {
    name: "height",
    kind: "number",
    required: false,
    description: "Frame height in pixels. Default 240, clamped to 64..=16384.",
};
pub(crate) const P_VIEW: ParamSchema = ParamSchema {
    name: "view",
    kind: "string",
    required: false,
    description: "Pane: perspective (default, the editor camera), top, front or side.",
};
const P_QUALITY: ParamSchema = ParamSchema {
    name: "quality",
    kind: "string",
    required: false,
    description: "Preset for this call: editor, game, mobile_low, mobile_high, desktop_low, \
        desktop_high, desktop_ultra or deck. Default: the viewport's current preset.",
};

pub(crate) fn str_param<'a>(params: &'a Value, name: &str) -> Option<&'a str> {
    params.get(name).and_then(Value::as_str)
}

/// A capture side from `params[name]`, clamped to 64..=`MAX_SIDE`: an integer that does not fit in
/// `u32` (or no integer) is the default, never the largest capture (review sprint-w12-rev-g1 #4).
pub(crate) fn dim(params: &Value, name: &str, default: u32) -> u32 {
    params
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(default)
        .clamp(64, crate::screenshot::MAX_SIDE)
}

fn device(world: &mut World) -> Result<regia_render::RenderDevice, RpcError> {
    crate::agent_ext::viewport_device(world)
}

/// The pane request a call names (`view`, `width`, `height`, `warm_frames`), plus the quality
/// override applied to the world's layout for the call (restored by [`with_quality`]).
pub(crate) fn pane_request(world: &World, params: &Value) -> Result<PaneRequest, RpcError> {
    let kind = match str_param(params, "view") {
        Some(name) => ViewKind::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!("unknown view `{name}` (perspective, top, front, side)"))
        })?,
        None => ViewKind::Perspective,
    };
    let layout = world
        .get_resource::<ViewportLayout>()
        .cloned()
        .unwrap_or_default();
    let pane = layout.panes.iter().position(|p| p.kind == kind).unwrap_or(0);
    let (_, camera) = layout.pane_camera(world, pane);
    Ok(PaneRequest {
        pane,
        kind,
        camera,
        width: dim(params, "width", 320),
        height: dim(params, "height", 240),
        ids: false,
        warm_frames: params
            .get("warm_frames")
            .and_then(Value::as_u64)
            .map_or(0, |v| v.min(16) as u32),
        // The call's view mode, else the pane's own (the View mode menu; ADR-7770).
        debug_view: call_view_mode(params)?
            .unwrap_or(layout.display(pane).view_mode)
            .debug_view(),
        reference: world
            .get_resource::<crate::reference::ViewportReferences>()
            .and_then(|r| r.panes.get(&pane).cloned()),
    })
}

/// The call's `debug_view` as a pane view mode (`None` when absent): a renderer debug view or a
/// View mode menu name (`lit_wireframe`, `wireframe_only`, `lighting_only`; ADR-7770).
fn call_view_mode(params: &Value) -> Result<Option<crate::views::ViewMode>, RpcError> {
    let Some(name) = str_param(params, "debug_view") else {
        return Ok(None);
    };
    let num = |key: &str| params.get(key).and_then(Value::as_f64).map(|v| v as f32);
    crate::views::ViewMode::from_params(name, num("tile_m"), num("target_px_per_m"), num("tolerance"))
        .map(Some)
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown debug_view `{name}` (lit, wireframe, uv_checker, texel_density, material_id, \
                 image_light, image_light_occlusion, lightmap_density, lightmap_only, lightmap_invalid, \
                 unlit, normals, base_color, roughness, metallic, ambient_occlusion, overdraw, \
                 light_complexity, shadow_cascades, lod_color, motion_vectors, mip_level, cloud_shadow_map, \
                 lit_wireframe, wireframe_only, lighting_only)"
            ))
        })
}

/// Runs `f` with the call's `quality` (and `game_view`) applied to the layout, then restores it.
fn with_quality<T>(
    world: &mut World,
    params: &Value,
    f: impl FnOnce(&mut World) -> Result<T, RpcError>,
) -> Result<T, RpcError> {
    let saved = world.get_resource_or_insert_with(ViewportLayout::default).clone();
    let mut layout = saved.clone();
    if let Some(name) = str_param(params, "quality") {
        layout.quality = ViewportQuality::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown quality `{name}` (editor, game, mobile_low, mobile_high, desktop_low, desktop_high, desktop_ultra, deck)"
            ))
        })?;
    }
    if let Some(game_view) = params.get("game_view").and_then(Value::as_bool) {
        layout.game_view = game_view;
    }
    // A `debug_view` draws as the pane would in that view mode (one code path, ADR-7770).
    if let Some(mode) = call_view_mode(params)? {
        let kind = str_param(params, "view")
            .and_then(ViewKind::from_name)
            .unwrap_or(ViewKind::Perspective);
        let pane = layout.panes.iter().position(|p| p.kind == kind).unwrap_or(0);
        layout.display_mut(pane).view_mode = mode;
    }
    world.insert_resource(layout);
    let out = f(world);
    world.insert_resource(saved);
    out
}

pub(crate) fn info_json(info: &crate::render::PaneInfo) -> Value {
    let g = &info.gpu;
    json!({
        "quality": info.quality,
        "profile": info.profile,
        "taau": info.taau,
        "atmosphere": info.atmosphere,
        "possessed": info.possessed,
        "preview_sun": info.preview_sun,
        "meshes": info.meshes,
        "proxies": info.proxies,
        "local_lights": info.local_lights,
        "runtime_renderer": info.runtime_renderer,
        "debug_legend": info.debug_legend.iter().map(|e| json!({
            "name": e.name,
            "color": e.color,
            "hex": format!(
                "#{:02x}{:02x}{:02x}",
                (e.color[0] * 255.0 + 0.5) as u8,
                (e.color[1] * 255.0 + 0.5) as u8,
                (e.color[2] * 255.0 + 0.5) as u8
            ),
        })).collect::<Vec<_>>(),
        "draws": {
            "drawn": info.draws.drawn,
            "culled": info.draws.culled,
            "instanced": info.draws.instanced,
            "per_object": info.draws.per_object,
            "double_sided_per_object": info.draws.double_sided_per_object,
        },
        "gpu_ms": {
            "frame": g.frame_ms, "shadows": g.shadows_ms, "prepass": g.prepass_ms,
            "forward": g.forward_ms, "ssao": g.ssao_ms, "ssr": g.ssr_ms, "taau": g.taau_ms,
            "atmosphere": g.atmosphere_ms, "clouds": g.clouds_ms, "fog": g.fog_ms, "post": g.post_ms,
        },
        "cpu_ms": info.cpu_ms,
        // The pane display settings in effect (ADR-7770).
        "view_mode": info.view_mode,
        "render_size": info.render_size,
        "screen_percentage": info.screen_percentage,
        "exposure_ev": info.exposure_ev,
        "counters": {
            "pipelines": info.counters.pipelines,
            "bind_groups": info.counters.bind_groups,
            "blocking_polls": info.counters.blocking_polls,
            "submits": info.counters.submits,
        },
    })
}

/// The pane index a call's `view` names (as [`pane_request`] picks it).
fn pane_of(world: &World, params: &Value) -> Result<usize, RpcError> {
    pane_request(world, params).map(|request| request.pane)
}

fn reference_json(reference: &crate::reference::ReferenceImage) -> Value {
    json!({
        "mode": reference.mode.name(),
        "opacity": reference.opacity,
        "rect_uv": reference.rect_uv,
        "width": reference.texture.width(),
        "height": reference.texture.height(),
    })
}

/// `editor.set_reference`.
fn set_reference(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let pane = pane_of(world, params)?;
    let previous = world
        .get_resource::<crate::reference::ViewportReferences>()
        .and_then(|r| r.panes.get(&pane).cloned());
    let bytes = match (str_param(params, "png_base64"), str_param(params, "path")) {
        (Some(text), _) => Some(crate::reference::base64_decode(text).map_err(RpcError::invalid_params)?),
        (None, Some(path)) => Some(
            std::fs::read(crate::agent_ext::document_request_path(world, "path", path)?)
                .map_err(|err| RpcError::invalid_params(format!("cannot read `{path}`: {err}")))?,
        ),
        (None, None) => None,
    };
    let texture = match bytes {
        Some(bytes) => {
            std::sync::Arc::new(crate::reference::decode_image(&bytes).map_err(RpcError::invalid_params)?)
        }
        None => previous
            .as_ref()
            .map(|r| std::sync::Arc::clone(&r.texture))
            .ok_or_else(|| {
                RpcError::invalid_params("`png_base64` or `path` is required (the pane has no reference yet)")
            })?,
    };
    let mode = match str_param(params, "mode") {
        Some(name) => crate::reference::ReferenceMode::from_name(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown mode `{name}` (behind, over, difference, silhouette)"
            ))
        })?,
        None => previous.as_ref().map_or_else(Default::default, |r| r.mode),
    };
    let opacity = params.get("opacity").and_then(Value::as_f64).map_or_else(
        || previous.as_ref().map_or(0.5, |r| r.opacity),
        |v| (v as f32).clamp(0.0, 1.0),
    );
    let rect_uv = match params.get("rect_uv") {
        Some(value) => {
            let numbers: Vec<f32> = value
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_f64).map(|v| v as f32).collect())
                .unwrap_or_default();
            let rect: [f32; 4] = numbers
                .try_into()
                .map_err(|_| RpcError::invalid_params("`rect_uv` is four numbers [u0, v0, u1, v1]"))?;
            if rect[2] <= rect[0] || rect[3] <= rect[1] {
                return Err(RpcError::invalid_params(
                    "`rect_uv` must have u1 > u0 and v1 > v0",
                ));
            }
            rect
        }
        None => previous.as_ref().map_or([0.0, 0.0, 1.0, 1.0], |r| r.rect_uv),
    };
    let reference = crate::reference::ReferenceImage {
        texture,
        opacity,
        mode,
        rect_uv,
    };
    let out = reference_json(&reference);
    world
        .get_resource_or_insert_with(crate::reference::ViewportReferences::default)
        .panes
        .insert(pane, reference);
    Ok(out)
}

/// `editor.clear_reference`.
fn clear_reference(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let pane = if params.get("view").is_some() {
        Some(pane_of(world, params)?)
    } else {
        None
    };
    let mut references = world.get_resource_or_insert_with(crate::reference::ViewportReferences::default);
    let cleared = match pane {
        Some(pane) => references.panes.remove(&pane).is_some(),
        None => {
            let any = !references.panes.is_empty();
            references.panes.clear();
            any
        }
    };
    Ok(json!({ "cleared": cleared }))
}

/// `editor.get_reference`.
fn get_reference(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let pane = pane_of(world, params)?;
    Ok(world
        .get_resource::<crate::reference::ViewportReferences>()
        .and_then(|r| r.panes.get(&pane))
        .map_or_else(
            || json!({ "reference": null }),
            |r| json!({ "reference": reference_json(r) }),
        ))
}

/// `editor.capture_viewport`.
fn capture_viewport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    // `client`: that play-in-editor client's view, its own camera and renderer (ADR-2623).
    if crate::pie_worlds::client_param(params)?.is_some() {
        return crate::pie_worlds::capture_client(world, params);
    }
    let device = device(world)?;
    with_quality(world, params, |world| {
        let request = pane_request(world, params)?;
        // Past the device's texture limit a capture needs tiles (ADR-7768); one render otherwise.
        let limit = crate::screenshot::device_limit(&device);
        let plan = crate::screenshot::plan_tiles([request.width, request.height], limit);
        if plan.count() > 1 {
            return Err(crate::screenshot::ShotError::NeedsTiles {
                size: [request.width, request.height],
                limit,
                tiles: plan.count(),
            }
            .to_rpc());
        }
        let frame = capture_pane(&device, world, &request).ok_or_else(|| {
            RpcError::new(
                regia_agent::protocol::UNSUPPORTED,
                "F0011",
                "the viewport capture produced no image",
                "a Vulkan adapter (lavapipe or hardware) is required",
            )
        })?;
        let png = crate::render::viewport_png_base64(&frame.image).map_err(RpcError::internal)?;
        let report =
            crate::viewport::measure_viewport_with(&frame.image, &crate::theme::tokens(world).overlay);
        Ok(json!({
            "width": frame.image.width(),
            "height": frame.image.height(),
            "mime": "image/png",
            "png_base64": png,
            "view": request.kind.name(),
            "sky_luminance": report.sky_luminance,
            "sky_blue_bias": report.sky_blue_bias,
            "lit_peak": report.lit_peak,
            "shading_range": report.shading_range,
            "selection_highlight": report.selection_highlight,
            "grid_pixels": report.grid_pixels,
            "gizmo_axes": report.gizmo_axes,
            "taau": frame.info.taau,
            "profile": frame.info.profile,
            "quality": frame.info.quality,
            "info": info_json(&frame.info),
            "reference": request.reference.as_ref().map(reference_json),
        }))
    })
}

// Editor agent requests run in the edit world; the visible viewport may be in PIE.
#[derive(Resource, Default)]
struct PendingDebugFlags {
    fog: Option<bool>,
    colliders: Option<bool>,
    navigation: Option<bool>,
    paths: Option<bool>,
    game_view: Option<bool>,
    lockstep: Option<bool>,
}

/// Applies queued agent debug flags to play once, preserving subsequent play-view GUI edits.
pub(crate) fn sync_debug_flags(edit: &mut World, mut play: Option<&mut World>) {
    // The fog debug view fills the game's `FogDebugView` only while a world's Show ▸ Fog is on.
    for world in [Some(&mut *edit), play.as_deref_mut()].into_iter().flatten() {
        let on = world
            .get_resource::<ViewportLayout>()
            .is_some_and(|layout| layout.effective_show().fog);
        crate::fog_debug::sync_flag(world, on);
    }
    let pending = edit.remove_resource::<PendingDebugFlags>();
    if let (Some(pending), Some(play)) = (pending, play) {
        let mut layout = play.get_resource_or_insert_with(ViewportLayout::default);
        let layout = &mut *layout;
        for (value, slot) in [
            (pending.colliders, &mut layout.show.colliders),
            (pending.navigation, &mut layout.show.navigation),
            (pending.paths, &mut layout.show.paths),
            (pending.fog, &mut layout.show.fog),
            (pending.lockstep, &mut layout.show.lockstep),
        ] {
            if let Some(value) = value {
                *slot = value;
            }
        }
        if let Some(value) = pending.game_view {
            layout.game_view = value;
        }
    }
}

/// `editor.set_viewport`.
pub(crate) fn set_viewport(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut settings = world
        .get_resource_or_insert_with(ViewportSettings::default)
        .clone();
    let mut layout = world.get_resource_or_insert_with(ViewportLayout::default).clone();
    let flag = |name: &str| params.get(name).and_then(Value::as_bool);
    if let Some(value) = params.get("overlay_blur") {
        let radius = value.as_f64().filter(|r| r.is_finite()).ok_or_else(|| {
            RpcError::invalid_params("`overlay_blur` must be a number of points, 0 for off")
        })?;
        if !(0.0..=f64::from(crate::viewport_blur::MAX_OVERLAY_BLUR)).contains(&radius) {
            return Err(RpcError::invalid_params(format!(
                "overlay_blur {radius} is outside 0..=64"
            )));
        }
        settings.overlay_blur = radius as f32;
    }
    if let Some(grid) = flag("grid") {
        settings.grid = grid;
        layout.show.grid = grid;
    }
    if let Some(labels) = flag("labels") {
        settings.labels = labels;
    }
    if let Some(realtime) = flag("realtime") {
        settings.realtime = realtime;
    }
    if let Some(audio) = flag("audio") {
        settings.audio = audio;
        layout.show.audio = audio;
    }
    for (name, slot) in [
        ("icons", &mut layout.show.icons),
        ("bounds", &mut layout.show.bounds),
        ("outline", &mut layout.show.outline),
        ("proxies", &mut layout.show.proxies),
        ("stats", &mut layout.show.stats),
        ("preview_light", &mut layout.show.preview_light),
        ("colliders", &mut layout.show.colliders),
        ("light_radii", &mut layout.show.light_radii),
        ("navigation", &mut layout.show.navigation),
        ("paths", &mut layout.show.paths),
        ("lockstep", &mut layout.show.lockstep),
        ("post_volumes", &mut layout.show.post_volumes),
        ("fog", &mut layout.show.fog),
        ("ai_bt", &mut layout.show.ai_bt),
        ("ai_state_tree", &mut layout.show.ai_state_tree),
        ("ai_goap", &mut layout.show.ai_goap),
        ("ai_squad", &mut layout.show.ai_squad),
        ("ai_formation", &mut layout.show.ai_formation),
        ("ai_move_to", &mut layout.show.ai_move_to),
        ("ai_blackboard", &mut layout.show.ai_blackboard),
        ("game_view", &mut layout.game_view),
        ("possessed", &mut layout.possessed),
    ] {
        if let Some(value) = flag(name) {
            *slot = value;
        }
    }
    if let Some(name) = str_param(params, "quality") {
        layout.quality = ViewportQuality::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown quality `{name}`")))?;
    }
    // Composition guides (ADR-7768): `guides { thirds, center, safe_areas, letterbox }`.
    if let Some(guides) = params.get("guides") {
        crate::overlays::apply_guides_param(world, guides)?;
    }
    if let Some(name) = str_param(params, "layout")
        && !layout.set_layout_name(name)
    {
        return Err(RpcError::invalid_params(format!(
            "unknown layout `{name}` (single, quad, side_by_side, stacked, one_large_two)"
        )));
    }
    // Camera navigation (ADR-7755): applied only when every field is valid.
    crate::camera_nav_agent::apply_nav_params(world, params)?;
    // The pane display settings (ADR-7770): `pane` (default the active pane).
    let pane = match params.get("pane") {
        None | Some(Value::Null) => layout.active.min(3),
        Some(value) => value
            .as_u64()
            .filter(|p| *p < 4)
            .ok_or_else(|| RpcError::invalid_params("`pane` must be 0..=3"))? as usize,
    };
    crate::viewport_badges::apply_display_params(&mut layout, pane, params)?;
    let display = *layout.display(pane);
    let mut out = json!({
        "grid": settings.grid,
        "labels": settings.labels,
        "realtime": settings.realtime,
        "overlay_blur": settings.overlay_blur,
        "quality": layout.quality.name(),
        "layout": layout.layout_name(),
        "icons": layout.show.icons,
        "bounds": layout.show.bounds,
        "outline": layout.show.outline,
        "proxies": layout.show.proxies,
        "stats": layout.show.stats,
        "preview_light": layout.show.preview_light,
        "colliders": layout.show.colliders,
        "light_radii": layout.show.light_radii,
        "navigation": layout.show.navigation,
        "paths": layout.show.paths,
        "lockstep": layout.show.lockstep,
        "post_volumes": layout.show.post_volumes,
        "fog": layout.show.fog,
        "ai_bt": layout.show.ai_bt,
        "ai_state_tree": layout.show.ai_state_tree,
        "ai_goap": layout.show.ai_goap,
        "ai_squad": layout.show.ai_squad,
        "ai_formation": layout.show.ai_formation,
        "ai_move_to": layout.show.ai_move_to,
        "ai_blackboard": layout.show.ai_blackboard,
        "audio": layout.show.audio,
        "game_view": layout.game_view,
        "possessed": layout.possessed,
        "guides": crate::overlays::guides_json(world),
        "pane": pane,
        "view_mode": display.view_mode.name(),
        "icon_filters": crate::viewport_badges::icon_filters_json(&display.icons),
        "icon_size": display.icons.size_percent,
        "icon_opacity": display.icons.opacity_percent,
        "exposure": display.exposure.name(),
        "exposure_ev": display.exposure.fixed_ev(),
        "screen_percentage": display.screen_percentage(),
    });
    if let Value::Object(fields) = &mut out {
        fields.extend(crate::camera_nav_agent::nav_json(world));
    }
    let show = layout.show;
    let ai_touched = [
        "ai_bt",
        "ai_state_tree",
        "ai_goap",
        "ai_squad",
        "ai_formation",
        "ai_move_to",
        "ai_blackboard",
    ]
    .iter()
    .any(|name| flag(name).is_some());
    world.insert_resource(settings);
    world.insert_resource(layout);
    if ai_touched {
        // `None` forces the layer write. A failed call returned before this insert.
        crate::viewport::sync_ai_debug_from_show(world, &show, None);
    }
    if ["colliders", "navigation", "paths", "fog", "lockstep", "game_view"]
        .iter()
        .any(|name| flag(name).is_some())
    {
        let mut pending = world.get_resource_or_insert_with(PendingDebugFlags::default);
        if let Some(value) = flag("colliders") {
            pending.colliders = Some(value);
        }
        if let Some(value) = flag("navigation") {
            pending.navigation = Some(value);
        }
        if let Some(value) = flag("paths") {
            pending.paths = Some(value);
        }
        if let Some(value) = flag("fog") {
            pending.fog = Some(value);
        }
        if let Some(value) = flag("lockstep") {
            pending.lockstep = Some(value);
        }
        if let Some(value) = flag("game_view") {
            pending.game_view = Some(value);
        }
    }
    Ok(out)
}

pub(crate) fn entity_json(world: &World, entity: Entity) -> Value {
    json!({ "entity": entity.to_bits(), "name": crate::outliner::display_name(world, entity) })
}

/// Renders the named pane's id buffer.
pub(crate) fn id_frame(world: &mut World, params: &Value) -> Result<crate::render::PaneFrame, RpcError> {
    let device = device(world)?;
    let mut request = pane_request(world, params)?;
    request.ids = true;
    render_pane(&device, world, &request)
        .filter(|frame| frame.ids.is_some())
        .ok_or_else(|| RpcError::internal("the id buffer could not be rendered"))
}

/// `editor.pick`.
fn pick(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let x = params
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| RpcError::invalid_params("`x` is required"))?;
    let y = params
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| RpcError::invalid_params("`y` is required"))?;
    let pixel = Vec2::new(x as f32, y as f32);
    // Without an adapter (a headless editor, ADR-2412) or on request, the CPU fallback (ADR-7204).
    let cpu = params.get("cpu").and_then(Value::as_bool).unwrap_or(false) || device(world).is_err();
    let hit = if cpu {
        let request = pane_request(world, params)?;
        let view = crate::views::ViewCamera::of(request.kind, &request.camera);
        let (origin, direction) = view.ray(pixel, Vec2::new(request.width as f32, request.height as f32));
        crate::viewport_ops::cpu_pick(world, origin, direction, view.far)
    } else {
        let frame = id_frame(world, params)?;
        crate::viewport::pick_in_frame(world, &frame, pixel)
    };
    // A locked entity (the Outliner lock) is never picked.
    let hit = hit.filter(|(entity, _)| !crate::viewport_ops::is_locked(world, *entity));
    let method = if cpu { "cpu" } else { "gpu" };
    let select = params.get("select").and_then(Value::as_bool).unwrap_or(false);
    Ok(match hit {
        Some((entity, position)) => {
            if select {
                let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
                selection.select(entity, SelectMode::Replace);
                world.insert_resource(selection);
            }
            let mut out = entity_json(world, entity);
            out["position"] = json!(position.to_array());
            out["terrain"] = json!(world.get::<regia_terrain::Terrain>(entity).is_some());
            out["method"] = json!(method);
            out
        }
        None => json!({ "entity": null, "method": method }),
    })
}

/// `editor.pick_hit` (Tincal F7).
fn pick_hit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let coord = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is required")))
    };
    let (x, y) = (coord("x")?, coord("y")?);
    let cull = match str_param(params, "cull") {
        None => crate::picking::FaceCull::None,
        Some(name) => crate::picking::FaceCull::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown cull `{name}` (none, back)")))?,
    };
    let frame = id_frame(world, params)?;
    let hit = crate::viewport::pick_hit_in_frame(world, &frame, Vec2::new(x as f32, y as f32), cull);
    Ok(match hit {
        Some(hit) => {
            let mut out = entity_json(world, hit.entity);
            out["owner"] = json!(hit.owner.to_bits());
            out["owner_name"] = json!(crate::outliner::display_name(world, hit.owner));
            out["position"] = json!(hit.position.to_array());
            out["normal"] = json!(hit.normal.to_array());
            out["smooth_normal"] = json!(hit.smooth_normal.to_array());
            out["triangle"] = json!(hit.triangle);
            out["indices"] = json!(hit.indices);
            out["barycentric"] = json!(hit.barycentric.to_array());
            out["distance"] = json!(hit.distance);
            out["front_face"] = json!(hit.front_face);
            out
        }
        None => json!({ "entity": null }),
    })
}

/// An optional array of `N` numbers.
fn floats<const N: usize>(params: &Value, name: &str) -> Result<Option<[f32; N]>, RpcError> {
    let Some(value) = params.get(name).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params(format!("`{name}` must be an array of {N} numbers"));
    let items = value.as_array().filter(|a| a.len() == N).ok_or_else(bad)?;
    let mut out = [0.0; N];
    for (slot, item) in out.iter_mut().zip(items) {
        *slot = item.as_f64().ok_or_else(bad)? as f32;
        // `1e39` casts to infinity: refuse it rather than store a non-finite override (audit v033b).
        if !slot.is_finite() {
            return Err(bad());
        }
    }
    Ok(Some(out))
}

fn override_json(value: Option<&regia_render::RenderOverride>) -> Value {
    match value {
        Some(o) => json!({
            "tint": o.tint,
            "emissive": o.emissive,
            "alpha": o.alpha,
            "xray": o.xray,
            "unlit": o.unlit,
            "blends": o.blends(),
        }),
        None => Value::Null,
    }
}

/// `editor.set_render_override` (Tincal F5).
fn set_render_override(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let entity = crate::command::entity_from_bits(bits)
        .filter(|e| world.get_entity(*e).is_ok())
        .ok_or_else(|| RpcError::invalid_params(format!("no entity {bits}")))?;
    if params.get("clear").and_then(Value::as_bool).unwrap_or(false) {
        world.entity_mut(entity).remove::<regia_render::RenderOverride>();
    } else {
        let alpha = params.get("alpha").and_then(Value::as_f64).map(|v| v as f32);
        if alpha.is_some_and(|a| !a.is_finite()) {
            return Err(RpcError::invalid_params("`alpha` must be a finite number"));
        }
        let value = regia_render::RenderOverride {
            tint: floats::<4>(params, "tint")?,
            emissive: floats::<3>(params, "emissive")?,
            alpha,
            xray: params.get("xray").and_then(Value::as_bool).unwrap_or(false),
            unlit: params.get("unlit").and_then(Value::as_bool).unwrap_or(false),
        };
        world.entity_mut(entity).insert(value);
    }
    let mut out = entity_json(world, entity);
    out["override"] = override_json(world.get::<regia_render::RenderOverride>(entity));
    out["undoable"] = json!(false);
    Ok(out)
}

/// `editor.box_select`.
fn box_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let coord = |name: &str| {
        params
            .get(name)
            .and_then(Value::as_f64)
            .map(|v| v.max(0.0) as u32)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` is required")))
    };
    let (x0, y0, x1, y1) = (coord("x0")?, coord("y0")?, coord("x1")?, coord("y1")?);
    let frame = id_frame(world, params)?;
    let found = frame
        .ids
        .as_ref()
        .map(|ids| ids.entities_in_rect((x0, y0), (x1, y1)))
        .unwrap_or_default();
    let mode = region_mode(params)?;
    let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
    selection.apply_region(&found, mode);
    world.insert_resource(selection);
    let entities: Vec<Value> = found.iter().map(|e| entity_json(world, *e)).collect();
    Ok(json!({ "entities": entities }))
}

/// The `mode` of `editor.box_select` and `editor.lasso_select` (ADR-7769).
fn region_mode(params: &Value) -> Result<crate::selection::RegionMode, RpcError> {
    let name = str_param(params, "mode").unwrap_or("set");
    crate::selection::RegionMode::parse(name).ok_or_else(|| {
        RpcError::invalid_params(format!("unknown mode `{name}` (set, add, subtract, toggle)"))
    })
}

/// `editor.lasso_select` (P-064, ADR-7769): the entities with an id-buffer pixel inside a
/// freehand polygon, applied like a box select.
fn lasso_select(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points = params
        .get("points")
        .and_then(Value::as_array)
        .ok_or_else(|| RpcError::invalid_params("`points` ([[x, y], ...], pixels) is required"))?;
    let polygon: Vec<[f32; 2]> = points
        .iter()
        .map(|p| {
            let xy = p.as_array().filter(|a| a.len() == 2)?;
            let x = xy[0].as_f64()? as f32;
            let y = xy[1].as_f64()? as f32;
            (x.is_finite() && y.is_finite()).then_some([x, y])
        })
        .collect::<Option<_>>()
        .ok_or_else(|| RpcError::invalid_params("every point is a finite [x, y] pair"))?;
    if polygon.len() < 3 {
        return Err(RpcError::invalid_params("a lasso needs at least 3 points"));
    }
    let mode = region_mode(params)?;
    let frame = id_frame(world, params)?;
    let mut found = frame
        .ids
        .as_ref()
        .map(|ids| crate::selection::entities_in_lasso(ids, &polygon))
        .unwrap_or_default();
    found.retain(|e| !crate::viewport_ops::is_locked(world, *e));
    let mut selection = world.get_resource_or_insert_with(Selection::default).clone();
    selection.apply_region(&found, mode);
    world.insert_resource(selection);
    let entities: Vec<Value> = found.iter().map(|e| entity_json(world, *e)).collect();
    let selected: Vec<u64> = world
        .resource::<Selection>()
        .entities()
        .iter()
        .map(|e| e.to_bits())
        .collect();
    Ok(json!({ "entities": entities, "mode": mode.name(), "selection": selected }))
}

fn camera_json(camera: &EditorCamera) -> Value {
    json!({
        "target": camera.target.to_array(),
        "distance": camera.distance,
        "yaw": camera.yaw,
        "pitch": camera.pitch,
        "eye": camera.eye().to_array(),
    })
}

/// `editor.focus`.
fn focus(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entities: Vec<Entity> = match params.get("entities").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(Value::as_u64)
            .filter_map(crate::command::entity_from_bits)
            .filter(|e| world.get_entity(*e).is_ok())
            .collect(),
        None => world
            .get_resource::<Selection>()
            .map(|s| s.entities().to_vec())
            .unwrap_or_default(),
    };
    if entities.is_empty() {
        return Err(RpcError::invalid_params(
            "nothing to focus: pass `entities` or select something",
        ));
    }
    let mut camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
    crate::viewport::focus_camera(world, &mut camera, &entities);
    world.insert_resource(camera.clone());
    Ok(camera_json(&camera))
}

/// `editor.bookmark`.
fn bookmark(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mut marks = world
        .get_resource_or_insert_with(CameraBookmarks::default)
        .clone();
    let action = str_param(params, "action").unwrap_or("list");
    let slot = params.get("slot").and_then(Value::as_u64).map(|s| s as usize);
    let out = match action {
        "set" => {
            let slot = slot.ok_or_else(|| RpcError::invalid_params("`slot` (0-9) is required"))?;
            let camera = world.get_resource_or_insert_with(EditorCamera::default).clone();
            if !marks.set(slot, camera.clone()) {
                return Err(RpcError::invalid_params("`slot` must be 0-9"));
            }
            json!({ "slot": slot, "camera": camera_json(&camera) })
        }
        "recall" => {
            let slot = slot.ok_or_else(|| RpcError::invalid_params("`slot` (0-9) is required"))?;
            let camera = marks.get(slot).cloned().ok_or_else(|| {
                RpcError::not_found(
                    format!("bookmark {slot} is empty"),
                    "store one with action \"set\"",
                )
            })?;
            world.insert_resource(camera.clone());
            json!({ "slot": slot, "camera": camera_json(&camera) })
        }
        "list" => {
            let slots: Vec<Value> = (0..10)
                .filter_map(|i| {
                    marks
                        .get(i)
                        .map(|c| json!({ "slot": i, "camera": camera_json(c) }))
                })
                .collect();
            json!({ "bookmarks": slots })
        }
        other => {
            return Err(RpcError::invalid_params(format!(
                "unknown action `{other}` (set, recall, list)"
            )));
        }
    };
    world.insert_resource(marks);
    Ok(out)
}

/// `editor.viewport_stats`.
fn viewport_stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let stats = world.get_resource_or_insert_with(ViewportStats::default).clone();
    Ok(json!({
        "fps": stats.fps(),
        "median_frame_ms": stats.median().unwrap_or(0.0) * 1000.0,
        "hitches": stats.hitches,
        "frames": stats.frames,
        "last": stats.last.as_ref().map(info_json),
        "lines": stats.lines(),
        "pane": pane_json(world, &stats),
        "overlay_blur": world.get_resource::<crate::viewport_blur::ViewportBlurState>().map_or(
            json!({ "chips": 0, "shaded_pixels": 0 }),
            |state| json!({ "chips": state.chips, "shaded_pixels": state.shaded_pixels }),
        ),
    }))
}

/// The `pane` object of `editor.viewport_stats`: the path the panel's pictures take to egui and
/// the pane texture counters.
fn pane_json(world: &World, stats: &ViewportStats) -> Value {
    let textures = world
        .get_resource::<crate::pane_texture::PaneTextures>()
        .map(crate::pane_texture::PaneTextures::stats);
    json!({
        // The setting (`gpu` / `cpu`); `none` when the shell installed no pane textures
        // (headless runs always read back).
        "setting": textures.map_or("none", |t| t.path.name()),
        // What the last picture actually used.
        "path": stats.pane_path.map(crate::pane_texture::PanePath::name),
        "pictures_per_second": stats.pane_fps(),
        "pictures": stats.pane_frames,
        "textures": textures.map(|t| json!({
            "live": t.live,
            "registered": t.registered,
            "freed": t.freed,
            "rebound": t.rebound,
            "last_size": t.last_size,
        })),
    })
}

/// `editor.viewport_pane_path`.
fn viewport_pane_path(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(textures) = world.get_resource::<crate::pane_texture::PaneTextures>().cloned() else {
        return Err(RpcError::not_found(
            "pane_textures_unavailable",
            "only the windowed editor shows panes from GPU textures; headless captures always read back",
        ));
    };
    if let Some(text) = str_param(params, "path") {
        let path = crate::pane_texture::PanePath::parse(text)
            .ok_or_else(|| RpcError::invalid_params(format!("unknown path `{text}` (gpu, cpu)")))?;
        textures.set_path(path);
    }
    let stats = textures.stats();
    Ok(json!({ "path": stats.path.name(), "live": stats.live }))
}

/// `editor.view_asset`.
fn view_asset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::asset_viewer::{Channel, Loaded, load_asset, render_model, texture_view, waveform_image};
    let path = str_param(params, "path").ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    crate::mesh_source::agent_asset_path(world, "path", path)?;
    let root = crate::mesh_source::project_root(world);
    let library = world
        .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
        .clone();
    let loaded = load_asset(root.as_deref(), &library, path)
        .map_err(|err| RpcError::not_found(err, "check the path"))?;
    let (width, height) = (dim(params, "width", 320), dim(params, "height", 240));
    // Mesh overlays (ADR-7469): `overlay` is one name or a list.
    let overlays: Vec<crate::asset_viewer::MeshOverlay> = match params.get("overlay") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(name)) => vec![name.as_str()],
        Some(Value::Array(names)) => names.iter().map(|n| n.as_str().unwrap_or("")).collect(),
        Some(_) => vec![""],
    }
    .into_iter()
    .map(|name| {
        crate::asset_viewer::MeshOverlay::from_name(name).ok_or_else(|| {
            crate::asset_ops_agent::reasoned(
                "overlay_unknown",
                format!("unknown overlay `{name}` (uv, uv1, normals, collision)"),
            )
        })
    })
    .collect::<Result<_, _>>()?;
    if !overlays.is_empty() && !matches!(loaded, Loaded::Mesh { .. }) {
        return Err(crate::asset_ops_agent::reasoned(
            "overlay_not_mesh",
            format!("{path}: overlays are for meshes"),
        ));
    }
    let (image, info) = match loaded {
        Loaded::None => return Err(RpcError::internal("nothing loaded")),
        Loaded::Mesh { model, lods } => {
            let lod_index = params.get("lod").and_then(Value::as_u64).unwrap_or(0) as usize;
            let lod = lods
                .get(lod_index)
                .ok_or_else(|| RpcError::invalid_params(format!("`lod` must be below {}", lods.len())))?;
            let yaw = params.get("yaw").and_then(Value::as_f64).unwrap_or(0.6) as f32;
            let pitch = params.get("pitch").and_then(Value::as_f64).unwrap_or(0.35) as f32;
            let mut drawn = Vec::new();
            let mut geometries = Vec::new();
            for overlay in &overlays {
                let geometry =
                    crate::asset_viewer::overlay_geometry(root.as_deref(), path, &model, lod, *overlay)
                        .map_err(|refusal| crate::asset_ops_agent::reasoned(refusal.code, refusal.message))?;
                let mut summary = geometry.summary();
                if let Some(object) = summary.as_object_mut() {
                    object.insert("overlay".into(), json!(overlay.name()));
                }
                drawn.push(summary);
                geometries.push(geometry);
            }
            let device = device(world)?;
            let mut image = render_model(
                &device,
                &lod.parts,
                (model.aabb_min, model.aabb_max),
                yaw,
                pitch,
                [width, height],
            )
            .ok_or_else(|| RpcError::internal("mesh render failed"))?;
            let camera =
                crate::asset_viewer::OrbitCamera::framing((model.aabb_min, model.aabb_max), yaw, pitch);
            for geometry in &geometries {
                crate::asset_viewer::paint_overlay(&mut image, &camera, geometry);
            }
            let info = json!({
                "overlays": drawn,
                "kind": "mesh",
                "parts": model.parts.len(),
                "triangles": model.triangles(),
                "vertices": model.vertices(),
                "bounds": [model.aabb_min.to_array(), model.aabb_max.to_array()],
                "lod": lod_index,
                "lods": lods.iter().map(|l| json!({ "ratio": l.ratio, "triangles": l.triangles })).collect::<Vec<_>>(),
                "materials": model.parts.iter().map(crate::asset_viewer::material_summary).collect::<Vec<_>>(),
            });
            (image, info)
        }
        Loaded::Texture(chain) => {
            let mip_index = params.get("mip").and_then(Value::as_u64).unwrap_or(0) as usize;
            let mip = chain
                .get(mip_index)
                .ok_or_else(|| RpcError::invalid_params(format!("`mip` must be below {}", chain.len())))?;
            let channel = match str_param(params, "channel") {
                Some(name) => Channel::from_name(name).ok_or_else(|| {
                    RpcError::invalid_params(format!("unknown channel `{name}` (rgba, r, g, b, a)"))
                })?,
                None => Channel::Rgba,
            };
            let info = json!({
                "kind": "texture",
                "width": chain[0].width,
                "height": chain[0].height,
                "mips": chain.iter().map(|m| [m.width, m.height]).collect::<Vec<_>>(),
                "mip": mip_index,
                "channel": channel.name(),
            });
            (texture_view(mip, channel), info)
        }
        Loaded::Sound(sound) => {
            let info = json!({
                "kind": "sound",
                "sample_rate": sound.sample_rate,
                "channels": sound.channels,
                "frames": sound.frames(),
                "seconds": sound.seconds(),
            });
            (waveform_image(&sound, width as usize, height as usize), info)
        }
        // An animation (ADR-7788): the pose of `clip` at `time` seconds, looping.
        Loaded::Animation(preview) => {
            let clip = params.get("clip").and_then(Value::as_u64).unwrap_or(0) as usize;
            let time = params.get("time").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let visuals = egui::Visuals::dark();
            let image = crate::asset_viewer::anim_frame_image(
                &preview,
                clip,
                time,
                [width as usize, height as usize],
                (visuals.selection.stroke.color, visuals.extreme_bg_color),
            );
            let info = json!({
                "kind": "animation",
                "joints": preview.skeleton.parents.len(),
                "clips": preview
                    .clips
                    .iter()
                    .map(|c| json!({ "name": c.name, "duration": c.duration }))
                    .collect::<Vec<_>>(),
                "clip": clip,
                "time": time,
            });
            (image, info)
        }
    };
    let png = crate::render::viewport_png_base64(&image).map_err(RpcError::internal)?;
    Ok(json!({
        "path": path,
        "width": image.width(),
        "height": image.height(),
        "mime": "image/png",
        "png_base64": png,
        "info": info,
    }))
}

/// `editor.thumbnails` (Tincal F8): a batch of small renders on one warm renderer.
fn thumbnails(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use crate::thumbnails::{ThumbJob, ThumbSubject, ThumbView, ThumbnailRenderer};
    use std::sync::{Mutex, OnceLock};
    static SERVICE: OnceLock<Mutex<Option<ThumbnailRenderer>>> = OnceLock::new();
    let specs = params
        .get("jobs")
        .and_then(Value::as_array)
        .filter(|jobs| !jobs.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`jobs` (a non-empty array) is required"))?;
    if specs.len() > 256 {
        return Err(RpcError::invalid_params("at most 256 jobs per call"));
    }
    let default_size = params.get("size").and_then(Value::as_u64).unwrap_or(128);
    let root = crate::mesh_source::project_root(world);
    let library = world
        .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
        .clone();
    let mut jobs = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let subject = match (
            str_param(spec, "path"),
            spec.get("entity").and_then(Value::as_u64),
        ) {
            (Some(path), None) => ThumbSubject::Model({
                crate::mesh_source::agent_asset_path(world, "jobs[].path", path)?;
                library
                    .model(root.as_deref(), path)
                    .map_err(|err| RpcError::not_found(format!("job {index}: {err}"), "check the path"))?
            }),
            (None, Some(bits)) => ThumbSubject::Entity(
                crate::command::entity_from_bits(bits)
                    .filter(|e| world.get_entity(*e).is_ok())
                    .ok_or_else(|| RpcError::invalid_params(format!("job {index}: no entity {bits}")))?,
            ),
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "job {index}: give exactly one of `path` or `entity`"
                )));
            }
        };
        let view = match (
            spec.get("yaw").and_then(Value::as_f64),
            spec.get("pitch").and_then(Value::as_f64),
        ) {
            (None, None) => match str_param(spec, "view") {
                Some(name) => ThumbView::Kind(ViewKind::from_name(name).ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "job {index}: unknown view `{name}` (perspective, top, front, side)"
                    ))
                })?),
                None => ThumbView::Kind(ViewKind::Perspective),
            },
            (yaw, pitch) => ThumbView::Angles {
                yaw: yaw.unwrap_or(0.0) as f32,
                pitch: pitch.unwrap_or(0.0) as f32,
            },
        };
        let debug = match str_param(spec, "debug") {
            Some(name) => regia_render::DebugView::mode_from_name(name).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "job {index}: unknown debug mode `{name}` (lit, wireframe)"
                ))
            })?,
            None => regia_render::DebugViewMode::Lit,
        };
        let size = spec
            .get("size")
            .and_then(Value::as_u64)
            .unwrap_or(default_size)
            .clamp(
                u64::from(crate::thumbnails::MIN_SIZE),
                u64::from(crate::thumbnails::MAX_SIZE),
            );
        jobs.push(ThumbJob {
            model: subject,
            view,
            size: size as u32,
            debug,
        });
    }
    let device = device(world)?;
    let mut service = SERVICE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if service
        .as_ref()
        .is_none_or(|renderer| renderer.device().device_id() != device.device_id())
    {
        *service = Some(ThumbnailRenderer::new(&device));
    }
    let renderer = service
        .as_mut()
        .ok_or_else(|| RpcError::internal("thumbnail renderer missing"))?;
    let images = renderer.render_in(world, &jobs);
    let stats = renderer.stats();
    let mut out = Vec::with_capacity(images.len());
    for (index, image) in images.iter().enumerate() {
        out.push(json!({
            "index": index,
            "width": image.width(),
            "height": image.height(),
            "mime": "image/png",
            "png_base64": crate::render::viewport_png_base64(image).map_err(RpcError::internal)?,
        }));
    }
    Ok(json!({
        "images": out,
        "stats": {
            "jobs": stats.jobs,
            "rendered": stats.rendered,
            "failed": stats.failed,
            "sizes": stats.sizes,
            "cpu_ms": stats.cpu_ms,
            "pipelines_created": stats.counters.pipelines,
            "bind_groups_created": stats.counters.bind_groups,
            "textures_created": stats.counters.textures,
            "buffers_created": stats.counters.buffers,
        },
    }))
}

/// `editor.spawn_gltf_hierarchy`.
fn spawn_gltf_hierarchy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path").ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let flag = |name: &str, default: bool| params.get(name).and_then(Value::as_bool).unwrap_or(default);
    let opts = crate::gltf_hierarchy::GltfSpawnOptions {
        editor_only: flag("editor_only", false),
        keep_sockets: flag("keep_sockets", true),
        skins: flag("skins", true),
        root_name: str_param(params, "root_name").map(str::to_string),
        show_collision: flag("show_collision", false),
        lights: flag("lights", true),
        ..crate::gltf_hierarchy::GltfSpawnOptions::default()
    };
    let full = crate::mesh_source::agent_asset_path(world, "path", path)?;
    let spawned = crate::gltf_hierarchy::spawn_gltf_hierarchy(world, &full, &opts)
        .map_err(|err| RpcError::not_found(err, "check the path; glTF/GLB only"))?;
    let nodes: Vec<Value> = spawned
        .nodes
        .iter()
        .map(|node| {
            json!({
                "name": node.name,
                "gltf_index": node.gltf_index,
                "entity": node.entity.to_bits(),
                "parent": node.parent.map(Entity::to_bits),
                "triangles": node.triangles,
                "materials": node.materials,
                "socket": node.is_socket,
                "collision": node.is_collision,
                "skin": node.skin,
            })
        })
        .collect();
    let report = &spawned.report;
    Ok(json!({
        "path": path,
        "root": spawned.root.to_bits(),
        "nodes": nodes,
        "report": {
            "nodes": report.nodes,
            "mesh_entities": report.mesh_entities,
            "instances": report.instances,
            "lights": report.lights,
            "skinned_meshes": report.skinned_meshes,
            "sockets": report.sockets,
            "collision_nodes": report.collision_nodes,
            "hidden_collision_meshes": report.hidden_collision_meshes,
            "skeletons": report.skeletons,
            "joints": report.joints,
            "triangles": report.triangles,
            "duplicate_names": report.duplicate_names,
            "warnings": report.warnings,
        },
        "undoable": false,
    }))
}

/// `editor.collision_layer` (ADR-1658): the `collision_*` meshes of a glTF, kept by the loader option.
fn collision_layer(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path").ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let full = crate::mesh_source::agent_asset_path(world, "path", path)?;
    let library = world
        .get_resource_or_insert_with(crate::mesh_source::MeshLibrary::default)
        .clone();
    let options = crate::mesh_source::GltfLoadOptions {
        keep_collision_nodes: true,
    };
    let model = crate::mesh_source::load_gltf_model_with_options(
        &full,
        &library.default_material(),
        &|_| true,
        options,
    )
    .map_err(|err| RpcError::not_found(err, "check the path; glTF/GLB with at least one drawn mesh"))?;
    let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
    for part in &model.collision_parts {
        for vertex in &part.mesh.vertices {
            let p = Vec3::from_array(vertex.position);
            min = min.min(p);
            max = max.max(p);
        }
    }
    let triangles: usize = model
        .collision_parts
        .iter()
        .map(|p| p.mesh.indices.len() / 3)
        .sum();
    let vertices: usize = model.collision_parts.iter().map(|p| p.mesh.vertices.len()).sum();
    let bounds = if triangles == 0 {
        Value::Null
    } else {
        json!({ "min": min.to_array(), "max": max.to_array() })
    };
    Ok(json!({
        "path": path,
        "has_collision": triangles > 0,
        "collision_triangles": triangles,
        "collision_vertices": vertices,
        "collision_bounds": bounds,
        "render_triangles": model.triangles(),
        "undoable": false,
    }))
}

/// `editor.play_sound`.
fn play_sound(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let path = str_param(params, "path").ok_or_else(|| RpcError::invalid_params("`path` is required"))?;
    let full = crate::mesh_source::agent_asset_path(world, "path", path)?;
    let bytes = std::fs::read(&full)
        .map_err(|err| RpcError::not_found(format!("{}: {err}", full.display()), "check the path"))?;
    let sound = crate::asset_viewer::decode_sound(&bytes, path).map_err(RpcError::invalid_params)?;
    crate::asset_viewer::play_sound(&sound).map_err(RpcError::internal)?;
    Ok(json!({ "path": path, "seconds": sound.seconds(), "queued": true }))
}

/// The viewport and asset-viewer methods.
#[must_use]
#[allow(clippy::too_many_lines)] // a schema table
pub fn methods() -> Vec<(MethodSchema, Handler)> {
    vec![
        (
            MethodSchema {
                name: "editor.capture_viewport",
                tool: "capture_viewport",
                description: "Render a viewport pane through the runtime frame path (real meshes, \
                    materials, lights, sky, post at the chosen quality preset) with the editor \
                    overlays (grid, icons, bounds, selection outline, gizmo) and return a PNG, \
                    measurements and the frame's statistics. Headless.",
                params: &[
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    P_QUALITY,
                    ParamSchema {
                        name: "game_view",
                        kind: "boolean",
                        required: false,
                        description: "Hide every editor helper for this call (what the game shows).",
                    },
                    ParamSchema {
                        name: "warm_frames",
                        kind: "number",
                        required: false,
                        description: "Frames rendered before the kept one (temporal presets converge). 0..=16.",
                    },
                    ParamSchema {
                        name: "debug_view",
                        kind: "string",
                        required: false,
                        description: "Debug view mode: lit (default), wireframe (mesh edges over the lit frame), \
                            uv_checker, texel_density (blue under, green within, red over), material_id \
                            (false colour per material; the legend is in info.debug_legend), image_light \
                            (the image-based light alone), image_light_occlusion (what occludes it), lightmap_density (a checker per lightmap texel, colored by its density scale), lightmap_only (the lightmap's light alone), lightmap_invalid (the texels the bake filled), or (ADR-7211) unlit, normals, base_color, roughness, metallic, ambient_occlusion, overdraw, light_complexity, shadow_cascades, lod_color, motion_vectors, mip_level.",
                    },
                    ParamSchema {
                        name: "tile_m",
                        kind: "number",
                        required: false,
                        description: "uv_checker: edge of one checker cell in meters of surface (default 0.25; 0 or less lays the checker out in UV space).",
                    },
                    ParamSchema {
                        name: "target_px_per_m",
                        kind: "number",
                        required: false,
                        description: "texel_density: target texture pixels per meter (default 512).",
                    },
                    ParamSchema {
                        name: "tolerance",
                        kind: "number",
                        required: false,
                        description: "texel_density: allowed relative deviation from the target (default 0.1 = 10 percent).",
                    },
                    crate::pie_worlds::P_CLIENT,
                ],
                result: "{ width, height, mime, png_base64, view, sky_luminance, sky_blue_bias, lit_peak, \
                    shading_range, selection_highlight, grid_pixels, gizmo_axes, taau, profile, quality, info, reference } \
                    (with client: { client, width, height, mime, png_base64, view, possessed, render_us })",
                mutating: false,
                permission: Permission::Read,
            },
            capture_viewport as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_reference",
                tool: "set_reference",
                description: "Draw a reference image (concept ghost) in a viewport pane, in the live \
                    panel and in every capture of that pane. Modes: behind (replaces the background \
                    under the scene), over (alpha blend), difference (|scene - reference|), \
                    silhouette (the reference subject's outline). Unnamed fields keep the pane's \
                    current reference values.",
                params: &[
                    P_VIEW,
                    ParamSchema {
                        name: "png_base64",
                        kind: "string",
                        required: false,
                        description: "The image (PNG or JPEG) as standard base64. Alpha masks it.",
                    },
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: false,
                        description: "An image file on the machine running the editor (used when png_base64 is absent).",
                    },
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "behind, over (default), difference or silhouette.",
                    },
                    ParamSchema {
                        name: "opacity",
                        kind: "number",
                        required: false,
                        description: "0..1, default 0.5.",
                    },
                    ParamSchema {
                        name: "rect_uv",
                        kind: "array",
                        required: false,
                        description: "[u0, v0, u1, v1]: the pane rectangle (0..1, origin top-left) the image is stretched into. Default [0,0,1,1].",
                    },
                ],
                result: "{ mode, opacity, rect_uv, width, height }",
                mutating: true,
                permission: Permission::EditScene,
            },
            set_reference as Handler,
        ),
        (
            MethodSchema {
                name: "editor.clear_reference",
                tool: "clear_reference",
                description: "Remove the reference image of a pane (all panes when `view` is absent).",
                params: &[P_VIEW],
                result: "{ cleared }",
                mutating: true,
                permission: Permission::EditScene,
            },
            clear_reference as Handler,
        ),
        (
            MethodSchema {
                name: "editor.get_reference",
                tool: "get_reference",
                description: "The reference image settings of a pane (`reference: null` when none).",
                params: &[P_VIEW],
                result: "{ reference: { mode, opacity, rect_uv, width, height } | null }",
                mutating: false,
                permission: Permission::Read,
            },
            get_reference as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_viewport",
                tool: "set_viewport",
                description: "Set viewport display: show flags, quality preset, layout (1-4 panes), \
                    game view, play-mode possession, and camera navigation (invert, speed, orbit around selection, \
                    zoom toward cursor, navigation preset, field of view, clip planes).",
                params: &[
                    ParamSchema {
                        name: "grid",
                        kind: "boolean",
                        required: false,
                        description: "Reference grid.",
                    },
                    ParamSchema {
                        name: "labels",
                        kind: "boolean",
                        required: false,
                        description: "Selection name labels.",
                    },
                    ParamSchema {
                        name: "realtime",
                        kind: "boolean",
                        required: false,
                        description: "Render every frame.",
                    },
                    ParamSchema {
                        name: "overlay_blur",
                        kind: "number",
                        required: false,
                        description: "Frosted backing under the overlay chips (stats block, view label): Gaussian sigma in points, 0 to 64; 0 is off, the default.",
                    },
                    ParamSchema {
                        name: "icons",
                        kind: "boolean",
                        required: false,
                        description: "Light/camera/decal/audio icons.",
                    },
                    ParamSchema {
                        name: "bounds",
                        kind: "boolean",
                        required: false,
                        description: "Selection bounds, light ranges, frustums.",
                    },
                    ParamSchema {
                        name: "outline",
                        kind: "boolean",
                        required: false,
                        description: "Selection outline.",
                    },
                    ParamSchema {
                        name: "proxies",
                        kind: "boolean",
                        required: false,
                        description: "Proxy cubes for entities with nothing to draw.",
                    },
                    ParamSchema {
                        name: "stats",
                        kind: "boolean",
                        required: false,
                        description: "Stats overlay.",
                    },
                    ParamSchema {
                        name: "preview_light",
                        kind: "boolean",
                        required: false,
                        description: "Preview sun when the level has none.",
                    },
                    ParamSchema {
                        name: "colliders",
                        kind: "boolean",
                        required: false,
                        description: "Every collider's outline (off by default; ADR-7217).",
                    },
                    ParamSchema {
                        name: "light_radii",
                        kind: "boolean",
                        required: false,
                        description: "Every point light's radius and spot light's cone (off by default; ADR-7217).",
                    },
                    ParamSchema {
                        name: "paths",
                        kind: "boolean",
                        required: false,
                        description: "Draw remaining AI navigation paths, including in play views.",
                    },
                    ParamSchema {
                        name: "lockstep",
                        kind: "boolean",
                        required: false,
                        description: "Draw play's deterministic-index debug frame (Show ▸ Lockstep; what is drawn is sim.det_debug_draw's switch; off by default; ADR-8562).",
                    },
                    ParamSchema {
                        name: "navigation",
                        kind: "boolean",
                        required: false,
                        description: "The baked nav mesh's polygon outlines and off-mesh links (Show ▸ Navigation; off by default; ADR-7468).",
                    },
                    ParamSchema {
                        name: "post_volumes",
                        kind: "boolean",
                        required: false,
                        description: "Every post volume's shape and blend band (Show ▸ Post volumes; off by default; ADR-7593).",
                    },
                    ParamSchema {
                        name: "audio",
                        kind: "boolean",
                        required: false,
                        description: "Audio gizmos: emitter markers; ranges, cones and reverb zones of the selection.",
                    },
                    ParamSchema {
                        name: "fog",
                        kind: "boolean",
                        required: false,
                        description: "The fog of war debug view: watchers' sight circles, the fog grid outline, the texture and the viewer team picker (Show ▸ Fog of war; off by default).",
                    },
                    ParamSchema {
                        name: "game_view",
                        kind: "boolean",
                        required: false,
                        description: "Hide every editor helper.",
                    },
                    ParamSchema {
                        name: "possessed",
                        kind: "boolean",
                        required: false,
                        description: "In play, look through the game camera.",
                    },
                    ParamSchema {
                        name: "quality",
                        kind: "string",
                        required: false,
                        description: "Quality preset name.",
                    },
                    ParamSchema {
                        name: "layout",
                        kind: "string",
                        required: false,
                        description: "single, quad, side_by_side, stacked or one_large_two (ADR-7755).",
                    },
                    // Camera navigation (ADR-7755, `crate::camera_nav_agent`).
                    ParamSchema {
                        name: "invert",
                        kind: "object",
                        required: false,
                        description: "Invert axes: any of orbit_x, orbit_y, pan_x, pan_y, fly_y, wheel as booleans.",
                    },
                    ParamSchema {
                        name: "speed",
                        kind: "number",
                        required: false,
                        description: "Fly speed, m/s (0.5 to 500).",
                    },
                    ParamSchema {
                        name: "speed_step",
                        kind: "integer",
                        required: false,
                        description: "Fly speed by the toolbar slider's step, 1 to 8 (0.75 to 96 m/s, doubling).",
                    },
                    ParamSchema {
                        name: "speed_with_distance",
                        kind: "boolean",
                        required: false,
                        description: "The fly speed scales with the distance to the orbit target.",
                    },
                    ParamSchema {
                        name: "orbit_selection",
                        kind: "boolean",
                        required: false,
                        description: "Orbit around the selection's center without framing it.",
                    },
                    ParamSchema {
                        name: "zoom_to_cursor",
                        kind: "boolean",
                        required: false,
                        description: "The wheel zooms toward the point under the pointer (default true).",
                    },
                    ParamSchema {
                        name: "navigation_preset",
                        kind: "string",
                        required: false,
                        description: "regia_default, three_button or numpad.",
                    },
                    ParamSchema {
                        name: "fov",
                        kind: "number",
                        required: false,
                        description: "The perspective camera's vertical field of view, degrees (20 to 120).",
                    },
                    ParamSchema {
                        name: "near",
                        kind: "number",
                        required: false,
                        description: "The perspective camera's near clip plane, m (0.001 to 100; far at least ten times near).",
                    },
                    ParamSchema {
                        name: "far",
                        kind: "number",
                        required: false,
                        description: "The perspective camera's far clip plane, m (1 to 100000).",
                    },
                    ParamSchema {
                        name: "guides",
                        kind: "object",
                        required: false,
                        description: "Composition guides over the perspective pane (ADR-7768): { thirds, center, safe_areas (booleans), letterbox (2.39:1, 1.85:1, 16:9, 4:3, 1:1, 9:16, an aspect number, or null) }; keys left out keep their value.",
                    },
                    ParamSchema {
                        name: "pane",
                        kind: "integer",
                        required: false,
                        description: "The pane (0-3) the display settings below apply to; default the active pane (ADR-7770).",
                    },
                    ParamSchema {
                        name: "view_mode",
                        kind: "string",
                        required: false,
                        description: "The pane's view mode: lit, unlit, wireframe_only, lit_wireframe, lighting_only, light_complexity, overdraw, or a renderer buffer view name (base_color, normals, ...).",
                    },
                    ParamSchema {
                        name: "icon_filters",
                        kind: "object",
                        required: false,
                        description: "Per-kind icon switches of the pane: { lights, cameras, audio, decals, player_starts, match_rules, colliders, captures, volumes, nav }: bool.",
                    },
                    ParamSchema {
                        name: "icon_size",
                        kind: "number",
                        required: false,
                        description: "Icon size of the pane, percent (50-200).",
                    },
                    ParamSchema {
                        name: "icon_opacity",
                        kind: "number",
                        required: false,
                        description: "Icon opacity of the pane, percent (10-100).",
                    },
                    ParamSchema {
                        name: "exposure",
                        kind: "string",
                        required: false,
                        description: "The pane's exposure: auto, fixed (with exposure_ev) or level_band.",
                    },
                    ParamSchema {
                        name: "exposure_ev",
                        kind: "number",
                        required: false,
                        description: "The fixed EV100 (-4 to 20); alone it also selects fixed.",
                    },
                    ParamSchema {
                        name: "screen_percentage",
                        kind: "number",
                        required: false,
                        description: "The pane's internal render scale, percent (25-200); the upscaler brings it to the pane size.",
                    },
                    ParamSchema {
                        name: "ai_bt",
                        kind: "boolean",
                        required: false,
                        description: "Show behavior-tree leaf names in the world (ADR-8463). Default false.",
                    },
                    ParamSchema {
                        name: "ai_state_tree",
                        kind: "boolean",
                        required: false,
                        description: "Show the active state-tree state in the world (ADR-8463). Default false.",
                    },
                    ParamSchema {
                        name: "ai_goap",
                        kind: "boolean",
                        required: false,
                        description: "Show the GOAP plan step and goal in the world (ADR-8463). Default false.",
                    },
                    ParamSchema {
                        name: "ai_squad",
                        kind: "boolean",
                        required: false,
                        description: "Show squad lines and role letters in the world (ADR-8463). Default false.",
                    },
                    ParamSchema {
                        name: "ai_formation",
                        kind: "boolean",
                        required: false,
                        description: "Show formation slots in the world (ADR-8463). Default false.",
                    },
                    ParamSchema {
                        name: "ai_move_to",
                        kind: "boolean",
                        required: false,
                        description: "Show MoveTo targets and the remaining path in the world (ADR-8463). Default false.",
                    },
                    ParamSchema {
                        name: "ai_blackboard",
                        kind: "boolean",
                        required: false,
                        description: "Show watched blackboard keys in the world (ADR-8463). Default false.",
                    },
                ],
                result: "{ grid, labels, realtime, quality, layout, icons, bounds, outline, proxies, stats, \
                    preview_light, colliders, light_radii, navigation, paths, lockstep, post_volumes, audio, overlay_blur, game_view, possessed, \
                    invert, speed, speed_step, speed_with_distance, orbit_selection, zoom_to_cursor, navigation_preset, \
                    fov, near, far, guides, \
                    pane, view_mode, icon_filters, icon_size, icon_opacity, exposure, exposure_ev, screen_percentage, \
                    ai_bt, ai_state_tree, ai_goap, ai_squad, ai_formation, ai_move_to, ai_blackboard }",
                mutating: false,
                permission: Permission::Read,
            },
            set_viewport as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pick",
                tool: "pick",
                description: "The entity under a pixel of a pane, from the GPU entity-id buffer \
                    (pixel-exact on the rendered meshes) and the terrain heightfield; without a GPU (or \
                    with `cpu`) a ray against each mesh's BVH and the terrain. Locked entities are not picked.",
                params: &[
                    ParamSchema {
                        name: "x",
                        kind: "number",
                        required: true,
                        description: "Pixel x in the width×height frame.",
                    },
                    ParamSchema {
                        name: "y",
                        kind: "number",
                        required: true,
                        description: "Pixel y.",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    ParamSchema {
                        name: "select",
                        kind: "boolean",
                        required: false,
                        description: "Also select the hit.",
                    },
                    ParamSchema {
                        name: "cpu",
                        kind: "boolean",
                        required: false,
                        description: "Use the CPU ray pick (mesh BVH) even with a GPU.",
                    },
                ],
                result: "{ entity, name, position, terrain, method } or { entity: null, method }",
                mutating: false,
                permission: Permission::Read,
            },
            pick as Handler,
        ),
        (
            MethodSchema {
                name: "editor.pick_hit",
                tool: "pick_hit",
                description: "The surface under a pixel of a pane: the mesh entity (and the owner a click \
                    selects), the triangle index and its vertex indices, barycentric weights, the world \
                    hit point, the face normal (front side, world space), the interpolated vertex \
                    normal, the distance and whether the front face was hit. The GPU id buffer names \
                    the entity; a CPU ray cast against its triangles (skinned meshes with their current \
                    palette) refines it. Terrain is not included (use editor.pick).",
                params: &[
                    ParamSchema {
                        name: "x",
                        kind: "number",
                        required: true,
                        description: "Pixel x in the width×height frame.",
                    },
                    ParamSchema {
                        name: "y",
                        kind: "number",
                        required: true,
                        description: "Pixel y.",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    ParamSchema {
                        name: "cull",
                        kind: "string",
                        required: false,
                        description: "none (default: both faces, like the id buffer) or back (front faces only).",
                    },
                ],
                result: "{ entity, name, owner, owner_name, position, normal, smooth_normal, triangle, indices, \
                    barycentric, distance, front_face } or { entity: null }",
                mutating: false,
                permission: Permission::Read,
            },
            pick_hit as Handler,
        ),
        (
            MethodSchema {
                name: "editor.set_render_override",
                tool: "set_render_override",
                description: "Set or clear an entity's RenderOverride: tint (base colour multiplier), \
                    emissive (display-referred glow), alpha (ghost, drawn blended), xray (drawn through \
                    what is in front) and unlit. Applies to the entity's descendants and model parts \
                    without their own. Materials are not cloned. Not undoable.",
                params: &[
                    ParamSchema {
                        name: "entity",
                        kind: "number",
                        required: true,
                        description: "Entity bits.",
                    },
                    ParamSchema {
                        name: "tint",
                        kind: "array",
                        required: false,
                        description: "[r, g, b, a] linear 0-1; a multiplies the coverage.",
                    },
                    ParamSchema {
                        name: "emissive",
                        kind: "array",
                        required: false,
                        description: "[r, g, b] display-referred 0-1, added on top of the lit colour.",
                    },
                    ParamSchema {
                        name: "alpha",
                        kind: "number",
                        required: false,
                        description: "Coverage 0-1 (ghost). Below 1 the entity draws alpha-blended.",
                    },
                    ParamSchema {
                        name: "xray",
                        kind: "boolean",
                        required: false,
                        description: "Draw on top of everything (depth test off), blended.",
                    },
                    ParamSchema {
                        name: "unlit",
                        kind: "boolean",
                        required: false,
                        description: "Show the (tinted) base colour without lighting.",
                    },
                    ParamSchema {
                        name: "clear",
                        kind: "boolean",
                        required: false,
                        description: "Remove the override (the other fields are ignored).",
                    },
                ],
                result: "{ entity, name, override: { tint, emissive, alpha, xray, unlit, blends } | null, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            set_render_override as Handler,
        ),
        (
            MethodSchema {
                name: "editor.box_select",
                tool: "box_select",
                description: "Select every entity visible inside a pixel rectangle of a pane (GPU id buffer).",
                params: &[
                    ParamSchema {
                        name: "x0",
                        kind: "number",
                        required: true,
                        description: "Corner x.",
                    },
                    ParamSchema {
                        name: "y0",
                        kind: "number",
                        required: true,
                        description: "Corner y.",
                    },
                    ParamSchema {
                        name: "x1",
                        kind: "number",
                        required: true,
                        description: "Opposite corner x.",
                    },
                    ParamSchema {
                        name: "y1",
                        kind: "number",
                        required: true,
                        description: "Opposite corner y.",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "set (default), add, subtract or toggle.",
                    },
                ],
                result: "{ entities: [{entity, name}] }",
                mutating: false,
                permission: Permission::Read,
            },
            box_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.lasso_select",
                tool: "lasso_select",
                description: "Select every entity visible inside a freehand polygon of a pane (GPU id buffer, \
                    pixel centers, even-odd rule); the viewport's lasso (hold L while dragging, or the \
                    selection.toggle_lasso command). Locked entities are skipped.",
                params: &[
                    ParamSchema {
                        name: "points",
                        kind: "array",
                        required: true,
                        description: "The outline, [[x, y], ...] in pane pixels (3 or more; closed implicitly).",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    P_VIEW,
                    ParamSchema {
                        name: "mode",
                        kind: "string",
                        required: false,
                        description: "set (default), add, subtract or toggle (Shift, Ctrl+Shift and Ctrl in the viewport).",
                    },
                ],
                result: "{ entities: [{entity, name}], mode, selection: [entity bits] }",
                mutating: false,
                permission: Permission::Read,
            },
            lasso_select as Handler,
        ),
        (
            MethodSchema {
                name: "editor.focus",
                tool: "focus",
                description: "Frame entities (default: the selection) in the perspective camera by their bounds.",
                params: &[ParamSchema {
                    name: "entities",
                    kind: "array",
                    required: false,
                    description: "Entity ids.",
                }],
                result: "{ target, distance, yaw, pitch, eye }",
                mutating: false,
                permission: Permission::Read,
            },
            focus as Handler,
        ),
        (
            MethodSchema {
                name: "editor.bookmark",
                tool: "bookmark",
                description: "Camera bookmarks (Ctrl+digit stores, Shift+digit recalls in the panel; bare 1-4 are view presets).",
                params: &[
                    ParamSchema {
                        name: "action",
                        kind: "string",
                        required: false,
                        description: "set, recall or list (default).",
                    },
                    ParamSchema {
                        name: "slot",
                        kind: "number",
                        required: false,
                        description: "0-9.",
                    },
                ],
                result: "{ slot, camera } or { bookmarks }",
                mutating: false,
                permission: Permission::Read,
            },
            bookmark as Handler,
        ),
        (
            MethodSchema {
                name: "editor.viewport_stats",
                tool: "viewport_stats",
                description: "Live viewport statistics: fps, median frame time, hitch count, and the last \
                    pane render's draws, GPU time per pass and frame-path counters.",
                params: &[],
                result: "{ fps, median_frame_ms, hitches, frames, last, lines, pane }",
                mutating: false,
                permission: Permission::Read,
            },
            viewport_stats as Handler,
        ),
        (
            MethodSchema {
                name: "editor.viewport_pane_path",
                tool: "viewport_pane_path",
                description: "How the live viewport panes reach egui: `gpu` (the renderer's texture is \
                    registered as a native egui texture, no readback) or `cpu` (read back and uploaded \
                    each frame, the fallback). Without `path` it reports the setting.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: false,
                    description: "gpu or cpu.",
                }],
                result: "{ path, live }",
                mutating: true,
                permission: Permission::EditScene,
            },
            viewport_pane_path as Handler,
        ),
        (
            MethodSchema {
                name: "editor.view_asset",
                tool: "view_asset",
                description: "The asset viewer as a PNG plus details: a mesh (LOD, parts, materials, \
                    overlays: UV0/UV1 layout, normals, collision), a texture (mip, channel) or a WAV \
                    sound (waveform).",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path or builtin:cube/plane/sphere.",
                    },
                    P_WIDTH,
                    P_HEIGHT,
                    ParamSchema {
                        name: "lod",
                        kind: "number",
                        required: false,
                        description: "Mesh LOD (0 = full detail).",
                    },
                    ParamSchema {
                        name: "yaw",
                        kind: "number",
                        required: false,
                        description: "Mesh orbit yaw, radians.",
                    },
                    ParamSchema {
                        name: "pitch",
                        kind: "number",
                        required: false,
                        description: "Mesh orbit pitch, radians.",
                    },
                    ParamSchema {
                        name: "mip",
                        kind: "number",
                        required: false,
                        description: "Texture mip.",
                    },
                    ParamSchema {
                        name: "channel",
                        kind: "string",
                        required: false,
                        description: "Texture channel: rgba, r, g, b, a.",
                    },
                    ParamSchema {
                        name: "overlay",
                        kind: "string|array",
                        required: false,
                        description: "Mesh overlay (or a list): uv, uv1, normals, collision. Refusals: \
                            overlay_unknown, overlay_not_mesh, uv_set_missing, no_collision, \
                            collision_not_cooked (ADR-7469).",
                    },
                ],
                result: "{ path, width, height, mime, png_base64, info }",
                mutating: false,
                permission: Permission::Read,
            },
            view_asset as Handler,
        ),
        (
            MethodSchema {
                name: "editor.play_sound",
                tool: "play_sound",
                description: "Play a WAV through the editor's audio output (the offline mixer when there is no device).",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative path.",
                }],
                result: "{ path, seconds, queued }",
                mutating: false,
                permission: Permission::Read,
            },
            play_sound as Handler,
        ),
        (
            MethodSchema {
                name: "editor.thumbnails",
                tool: "thumbnails",
                description: "Batch thumbnails: many small square renders (models by path or entities in the \
                    open world) on one device with one warm renderer, grouped by size, each on the asset \
                    viewer's stage so identical jobs give identical pixels. A job picks its subject \
                    (`path` or `entity`), its look (`view` or `yaw`/`pitch`), its `size` and a `debug` mode. \
                    Failed jobs come back as blank pictures and count in `stats.failed`. After the first \
                    call `stats.pipelines_created` is 0.",
                params: &[
                    ParamSchema {
                        name: "jobs",
                        kind: "array",
                        required: true,
                        description: "Up to 256 jobs: { path | entity, view?: perspective|top|front|side, \
                            yaw?, pitch? (radians, win over view), size?, debug?: lit|wireframe }.",
                    },
                    ParamSchema {
                        name: "size",
                        kind: "number",
                        required: false,
                        description: "Default picture size in pixels for jobs without one (default 128, clamped to 16..=2048).",
                    },
                ],
                result: "{ images: [{ index, width, height, mime, png_base64 }], stats: { jobs, rendered, failed, \
                    sizes, cpu_ms, pipelines_created, bind_groups_created, textures_created, buffers_created } }",
                mutating: false,
                permission: Permission::Read,
            },
            thumbnails as Handler,
        ),
        (
            MethodSchema {
                name: "editor.spawn_gltf_hierarchy",
                tool: "spawn_gltf_hierarchy",
                description: "Spawn a glTF/GLB as a named entity hierarchy: one entity per scene node \
                    (Name, ChildOf, local Transform), meshes in node-local space with their materials, \
                    empty nodes and socket_* nodes as empty entities, skins as Skeleton + SkinnedMesh. \
                    collision_* nodes keep their entity (GltfCollisionNode) but are not drawn unless \
                    show_collision is set. KHR_lights_punctual nodes carry their light unless lights \
                    is false. Returns the root, every node (name, entity, parent, \
                    triangles, materials) and a report. Not undoable: delete the root to remove it.",
                params: &[
                    ParamSchema {
                        name: "path",
                        kind: "string",
                        required: true,
                        description: "Project-relative path of the .glb/.gltf.",
                    },
                    ParamSchema {
                        name: "editor_only",
                        kind: "boolean",
                        required: false,
                        description: "Tag every spawned entity GltfEditorOnly (skipped by saves and exports). Default false.",
                    },
                    ParamSchema {
                        name: "keep_sockets",
                        kind: "boolean",
                        required: false,
                        description: "Keep socket_* nodes as empty entities. Default true.",
                    },
                    ParamSchema {
                        name: "skins",
                        kind: "boolean",
                        required: false,
                        description: "Import skins (Skeleton + SkinnedMesh + bind-pose SkinJoints). Default true.",
                    },
                    ParamSchema {
                        name: "root_name",
                        kind: "string",
                        required: false,
                        description: "Name of the root entity (default: the file stem).",
                    },
                    ParamSchema {
                        name: "show_collision",
                        kind: "boolean",
                        required: false,
                        description: "Draw the meshes of collision_* nodes too (a debugging aid). Default false: collision hulls are hidden.",
                    },
                    ParamSchema {
                        name: "lights",
                        kind: "boolean",
                        required: false,
                        description: "Give KHR_lights_punctual nodes their PointLight, SpotLight or DirectionalLight. Default true.",
                    },
                ],
                result: "{ path, root, nodes: [{ name, gltf_index, entity, parent, triangles, materials, socket, collision, skin }], report, undoable }",
                mutating: true,
                permission: Permission::EditScene,
            },
            spawn_gltf_hierarchy as Handler,
        ),
        (
            MethodSchema {
                name: "editor.collision_layer",
                tool: "collision_layer",
                description: "Load a glTF/GLB with the keep_collision_nodes loader option and report its \
                    collision layer: the merged meshes of collision_* nodes (ADR-1658). Read-only; the \
                    drawn mesh is unchanged by the option. To see the hulls, spawn the model with \
                    editor.spawn_gltf_hierarchy { show_collision: true }.",
                params: &[ParamSchema {
                    name: "path",
                    kind: "string",
                    required: true,
                    description: "Project-relative path of the .glb/.gltf.",
                }],
                result: "{ path, has_collision, collision_triangles, collision_vertices, collision_bounds: { min, max } | null, render_triangles, undoable }",
                mutating: false,
                permission: Permission::Read,
            },
            collision_layer as Handler,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_capture_side_past_u32_is_the_default_not_the_largest() {
        let p = json!({ "width": 5_000_000_000_u64, "height": 100_000 });
        assert_eq!(dim(&p, "width", 320), 320, "an integer past u32: the default");
        assert_eq!(
            dim(&p, "height", 240),
            crate::screenshot::MAX_SIDE,
            "in u32: clamped"
        );
        assert_eq!(dim(&json!({ "width": 10 }), "width", 320), 64);
        assert_eq!(dim(&json!({}), "width", 320), 320);
    }

    /// w10-editor-agent-smalls (4): `overlay` takes one name or a list, and its declared kind says
    /// so (a plain `string` kind would make a client send only one name).
    #[test]
    fn the_view_asset_overlay_param_is_declared_as_a_string_or_a_list() {
        let (schema, _) = methods()
            .into_iter()
            .find(|(m, _)| m.name == "editor.view_asset")
            .expect("registered");
        let overlay = schema
            .params
            .iter()
            .find(|p| p.name == "overlay")
            .expect("overlay param");
        let kinds: Vec<&str> = overlay.kind.split('|').collect();
        assert!(
            kinds.contains(&"string") && kinds.contains(&"array"),
            "{}",
            overlay.kind
        );
    }
}
