// SPDX-License-Identifier: MPL-2.0
//! Post volumes and the physical camera on the agent protocol (`book/agents/post-volumes.md`,
//! ADR-1464, `docs/specs/30-post-volumes-and-physical-camera.md`):
//!
//! - `render.post_volumes { camera?, point? }` lists every volume, the ones that reach the camera's
//!   eye (or `point`) with their weights in tier order, and the blended values there.
//! - `render.set_post_volume { entity, ... }` adds or edits a volume, validated before any field
//!   is applied.
//! - `render.get_physical_camera { entity? }` / `render.set_physical_camera { entity?, ... }` read
//!   and set a camera's lens: the exposure mode, ISO, shutter, aperture, focal length and focus.
//!
//! - Format v3 (ADR-2323): `shape` may be `{ kind: "convex_hull", planes }` and a volume may name a
//!   `lut`; `render.set_post_volume_luts` and `render.post_volume_gizmos` live in
//!   `post_volume_extras`.
//!
//! Error reasons (`data.reason`): `camera_missing`, `not_a_camera`, `entity_missing`,
//! `camera_and_point`, `invalid_point`, `invalid_shape`, `unknown_field`, `unknown_override`,
//! `invalid_override`, `invalid_value`, `invalid_mode`, `manual_needs_ev`, `newer_format`,
//! `invalid_lut`, `not_a_volume`.

use regia_app::prelude::*;
use regia_render::post_volume_shapes::MAX_HULL_PLANES;
use regia_render::post_volumes::{
    EyeRange, POST_VOLUME_FORMAT_VERSION, PostOverrides, PostVolume, PostVolumeShape, VolumeContribution,
    blend_post_volumes, gather_contributions,
};
use regia_render::{
    Camera, CameraExposureMode, DepthOfFieldMode, PhysicalCamera, PostSettings, RenderSettings, RenderTarget,
    TonemapOperator,
};
use regia_scene::GlobalTransform;
use serde_json::{Map, Value, json};

use crate::protocol::{self, RpcError};

/// The fields `render.set_post_volume` accepts besides `overrides`.
pub(crate) const VOLUME_FIELDS: &[&str] = &[
    "entity",
    "enabled",
    "shape",
    "priority",
    "weight",
    "blend_distance",
    "overrides",
    "lut",
];

/// The fields `render.set_physical_camera` accepts.
pub(crate) const LENS_FIELDS: &[&str] = &[
    "entity",
    "exposure_mode",
    "manual_ev100",
    "iso",
    "shutter_seconds",
    "shutter_reference_fps",
    "aperture",
    "focal_length_mm",
    "sensor_height_mm",
    "focus_distance",
    "depth_of_field",
    "shutter_angle",
    "motion_blur_samples",
];

/// The exposure mode names.
pub(crate) const EXPOSURE_MODES: &[&str] = &["inherit", "manual", "physical"];

/// The depth-of-field kernel names (`off` = none).
pub(crate) const DOF_MODES: &[&str] = &["off", "gaussian", "bokeh"];

pub(crate) fn error(
    code: i64,
    regia_code: &str,
    reason: &str,
    message: impl Into<String>,
    hint: &str,
) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: Some(json!({ "code": regia_code, "reason": reason, "hint": hint })),
    }
}

pub(crate) fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(
        protocol::INVALID_PARAMS,
        "F0002",
        reason,
        message,
        "see book/agents/post-volumes.md",
    )
}

pub(crate) fn missing(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    error(protocol::NOT_FOUND, "F0003", reason, message, hint)
}

pub(crate) fn object(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    match params {
        Value::Object(map) => Ok(map),
        Value::Null => {
            static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
            Ok(EMPTY.get_or_init(Map::new))
        }
        _ => Err(invalid("invalid_value", "params must be an object")),
    }
}

pub(crate) fn check_fields(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), RpcError> {
    match map.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(invalid(
            "unknown_field",
            format!("unknown field `{key}`; one of {}", allowed.join(", ")),
        )),
        None => Ok(()),
    }
}

/// An entity id parameter (`None` when absent or null).
pub(crate) fn entity_param(map: &Map<String, Value>, key: &str) -> Result<Option<Entity>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(Entity::try_from_bits)
            .map(Some)
            .ok_or_else(|| invalid("invalid_value", format!("`{key}` must be an entity id"))),
    }
}

/// A finite number in `lo..=hi` (`None` when absent or null).
pub(crate) fn number(map: &Map<String, Value>, key: &str, lo: f64, hi: f64) -> Result<Option<f32>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(v) if v.is_finite() && (lo..=hi).contains(&v) => Ok(Some(v as f32)),
            _ => Err(invalid(
                "invalid_value",
                format!("`{key}` must be a number from {lo} to {hi}"),
            )),
        },
    }
}

fn floats3(value: &Value) -> Option<[f32; 3]> {
    let array = value.as_array().filter(|a| a.len() == 3)?;
    let mut out = [0.0f32; 3];
    for (slot, v) in out.iter_mut().zip(array) {
        let v = v.as_f64().filter(|v| v.is_finite())?;
        *slot = v as f32;
    }
    Some(out)
}

/// The window camera with the lowest `order` (ties: the first in query order), the renderer's
/// main view and the exposure-zone eye.
pub(crate) fn active_camera(world: &mut World) -> Option<Entity> {
    let mut query = world.query::<(Entity, &Camera)>();
    query
        .iter(world)
        .filter(|(_, camera)| matches!(camera.target, RenderTarget::Window))
        .min_by_key(|(_, camera)| camera.order)
        .map(|(entity, _)| entity)
}

pub(crate) fn camera_missing() -> RpcError {
    missing(
        "camera_missing",
        "no window camera",
        "spawn a Camera with a GlobalTransform, or pass `point` (render.post_volumes) / `entity`",
    )
}

fn tonemap_name(op: TonemapOperator) -> &'static str {
    match op {
        TonemapOperator::AgX => "agx",
        TonemapOperator::AcesFitted => "aces_fitted",
        TonemapOperator::Neutral => "neutral",
    }
}

fn parse_tonemap(name: &str) -> Option<TonemapOperator> {
    match name.to_ascii_lowercase().replace('-', "_").as_str() {
        "agx" => Some(TonemapOperator::AgX),
        "aces_fitted" | "aces" => Some(TonemapOperator::AcesFitted),
        "neutral" => Some(TonemapOperator::Neutral),
        _ => None,
    }
}

pub(crate) fn shape_json(shape: &PostVolumeShape) -> Value {
    match shape {
        PostVolumeShape::Box { half_extents } => json!({ "kind": "box", "half_extents": half_extents }),
        PostVolumeShape::Sphere { radius } => json!({ "kind": "sphere", "radius": radius }),
        PostVolumeShape::Unbound => json!({ "kind": "unbound" }),
        PostVolumeShape::ConvexHull { planes } => {
            json!({ "kind": "convex_hull", "planes": planes, "valid": shape.is_valid() })
        }
    }
}

fn eye_json(eye: Option<EyeRange>) -> Value {
    eye.map_or(Value::Null, |e| {
        json!({
            "ev_min": e.ev_min,
            "ev_max": e.ev_max,
            "adapt_up_speed": e.adapt_up_speed,
            "adapt_down_speed": e.adapt_down_speed,
        })
    })
}

/// Every override field by wire name (`null` when the volume does not set it).
fn overrides_json(o: &PostOverrides) -> Value {
    json!({
        "exposure_compensation": o.exposure_compensation,
        "exposure_min_ev100": o.exposure_min_ev100,
        "exposure_max_ev100": o.exposure_max_ev100,
        "meter_center_weight": o.meter_center_weight,
        "tonemap": o.tonemap.map(tonemap_name),
        "contrast": o.contrast,
        "saturation": o.saturation,
        "bloom_intensity": o.bloom_intensity,
        "white_balance_kelvin": o.white_balance_kelvin,
        "tint": o.tint,
        "lift": o.lift,
        "gamma": o.gamma,
        "gain": o.gain,
        "vignette": o.vignette,
        "film_grain": o.film_grain,
        "local_shadow_contrast": o.local_shadow_contrast,
        "local_highlight_contrast": o.local_highlight_contrast,
        "local_detail_strength": o.local_detail_strength,
        "ao_intensity": o.ao_intensity,
        "fog_tint": o.fog_tint,
        "eye": eye_json(o.eye),
        "lens_flare_intensity": o.lens_flare_intensity,
        "lens_flare_threshold": o.lens_flare_threshold,
        "lens_dirt_intensity": o.lens_dirt_intensity,
        "chromatic_aberration": o.chromatic_aberration,
    })
}

fn volume_json(entity: Entity, volume: &PostVolume) -> Value {
    json!({
        "entity": entity.to_bits(),
        "format_version": volume.format_version,
        "supported": volume.is_supported(),
        "enabled": volume.enabled,
        "shape": shape_json(&volume.shape),
        "priority": volume.priority,
        "weight": volume.weight,
        "blend_distance": volume.blend_distance,
        "overrides": overrides_json(&volume.overrides),
        "sets": volume.overrides.set_fields(),
        "lut": volume.lut,
    })
}

/// The blended value of every overridable field (the eye excepted: see `render.get_exposure`).
fn resolved_json(post: &PostSettings) -> Value {
    let local = post.local_exposure;
    json!({
        "exposure_compensation": post.exposure_compensation,
        "exposure_min_ev100": post.auto_exposure_min_ev100,
        "exposure_max_ev100": post.auto_exposure_max_ev100,
        "meter_center_weight": post.meter.center_weight,
        "tonemap": tonemap_name(post.tonemap),
        "contrast": post.contrast,
        "saturation": post.saturation,
        "bloom_intensity": post.bloom_intensity,
        "white_balance_kelvin": post.grade.white_balance_kelvin,
        "tint": post.grade.tint,
        "lift": post.grade.lift,
        "gamma": post.grade.gamma,
        "gain": post.grade.gain,
        "vignette": post.vignette,
        "film_grain": post.film_grain,
        "local_shadow_contrast": local.map(|l| l.shadow_contrast),
        "local_highlight_contrast": local.map(|l| l.highlight_contrast),
        "local_detail_strength": local.map(|l| l.detail_strength),
        "ao_intensity": post.ao_intensity,
        "fog_tint": post.fog_tint,
        "lens_flare_intensity": post.lens_effects.flare_intensity,
        "lens_flare_threshold": post.lens_effects.flare_threshold,
        "lens_dirt_intensity": post.lens_effects.dirt_intensity,
        "chromatic_aberration": post.lens_effects.chromatic_aberration,
    })
}

/// The view's post settings before volumes: the project's (`RenderSettings::resolve_post`, or
/// the defaults without a renderer) with the level look on top, as `extract_frame` builds them.
pub(crate) fn base_post(world: &World) -> PostSettings {
    let mut post = world
        .get_resource::<RenderSettings>()
        .map_or_else(PostSettings::default, RenderSettings::resolve_post);
    if let Some(level) = world.get_resource::<regia_scene::level::LevelSettings>() {
        regia_render::apply_level_look(&mut post, &level.look);
    }
    post
}

/// `render.post_volumes { camera?, point? }`.
pub(crate) fn post_volumes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, &["camera", "point"])?;
    let camera = entity_param(map, "camera")?;
    let point = match map.get("point") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            floats3(value)
                .ok_or_else(|| invalid("invalid_point", "`point` must be an array of 3 finite numbers"))?,
        ),
    };
    let (camera, eye) = match (camera, point) {
        (Some(_), Some(_)) => {
            return Err(invalid("camera_and_point", "give `camera` or `point`, not both"));
        }
        (None, Some(point)) => (None, point),
        (camera, None) => {
            let entity = match camera {
                Some(entity) => entity,
                None => active_camera(world).ok_or_else(camera_missing)?,
            };
            let eye = world
                .get::<GlobalTransform>(entity)
                .filter(|_| world.get::<Camera>(entity).is_some())
                .map(|t| t.0.translation.to_array())
                .ok_or_else(|| {
                    missing(
                        "not_a_camera",
                        format!(
                            "entity {} is not a camera with a GlobalTransform",
                            entity.to_bits()
                        ),
                        "pass a camera entity, or omit `camera` for the active window camera",
                    )
                })?;
            (Some(entity), eye)
        }
    };

    let mut query = world.query::<(Entity, &PostVolume, &GlobalTransform)>();
    let mut rows: Vec<(Entity, PostVolume, GlobalTransform)> =
        query.iter(world).map(|(e, v, t)| (e, v.clone(), *t)).collect();
    rows.sort_by_key(|(entity, _, _)| entity.to_bits());
    let volumes: Vec<Value> = rows
        .iter()
        .map(|(entity, volume, world_from_local)| {
            let mut row = volume_json(*entity, volume);
            row["weight_here"] = json!(volume.weight_at(world_from_local, eye));
            row
        })
        .collect();
    let mut contributions: Vec<VolumeContribution> = Vec::new();
    gather_contributions(rows.iter().map(|(e, v, t)| (*e, v, t)), eye, &mut contributions);
    let mut post = base_post(world);
    blend_post_volumes(&mut post, &mut contributions);
    let lut_blend = crate::post_volume_extras::lut_blend_json(world, &rows, eye);
    // `blend_post_volumes` sorted the list into tier order.
    let contributing: Vec<Value> = contributions
        .iter()
        .map(|c| json!({ "entity": c.key, "priority": c.priority, "weight": c.weight }))
        .collect();
    let eye_ranges: Vec<Value> = contributions
        .iter()
        .filter_map(|c| {
            c.overrides.eye.map(|e| {
                json!({ "entity": c.key, "priority": c.priority, "weight": c.weight, "ev_min": e.ev_min, "ev_max": e.ev_max })
            })
        })
        .collect();
    Ok(json!({
        "camera": camera.map(Entity::to_bits),
        "eye": eye,
        "volumes": volumes,
        "contributing": contributing,
        "resolved": resolved_json(&post),
        "eye_ranges": eye_ranges,
        "lut_blend": lut_blend,
    }))
}

/// Parses `shape` (`{ kind: "box", half_extents }`, `{ kind: "sphere", radius }`,
/// `{ kind: "convex_hull", planes: [[nx, ny, nz, d], ...] }`, `{ kind: "unbound" }`, or the bare
/// string `"unbound"`).
fn parse_shape(value: &Value) -> Result<PostVolumeShape, RpcError> {
    let bad = |message: &str| invalid("invalid_shape", message.to_string());
    if value.as_str() == Some("unbound") {
        return Ok(PostVolumeShape::Unbound);
    }
    let map = value
        .as_object()
        .ok_or_else(|| bad("`shape` must be an object { kind, half_extents | radius }"))?;
    match crate::params::opt_str(map, "kind")? {
        Some("box") => {
            let half_extents = map
                .get("half_extents")
                .and_then(floats3)
                .filter(|h| h.iter().all(|v| *v >= 0.0 && *v <= 1.0e5))
                .ok_or_else(|| bad("a box needs `half_extents`: 3 numbers from 0 to 100000"))?;
            Ok(PostVolumeShape::Box { half_extents })
        }
        Some("sphere") => {
            let radius = crate::params::opt_f64(map, "radius")?
                .filter(|r| r.is_finite() && (0.0..=1.0e5).contains(r))
                .ok_or_else(|| bad("a sphere needs `radius`: a number from 0 to 100000"))?;
            Ok(PostVolumeShape::Sphere {
                radius: radius as f32,
            })
        }
        Some("unbound") => Ok(PostVolumeShape::Unbound),
        Some("convex_hull") => {
            let planes = map
                .get("planes")
                .and_then(Value::as_array)
                .ok_or_else(|| bad("a convex hull needs `planes`: an array of [nx, ny, nz, d]"))?;
            if planes.is_empty() || planes.len() > MAX_HULL_PLANES {
                return Err(bad(&format!(
                    "a convex hull needs 1 to {MAX_HULL_PLANES} planes, got {}",
                    planes.len()
                )));
            }
            let mut out = Vec::with_capacity(planes.len());
            for plane in planes {
                let values = plane.as_array().filter(|a| a.len() == 4).and_then(|a| {
                    a.iter()
                        .map(|v| {
                            v.as_f64()
                                .filter(|v| v.is_finite() && v.abs() <= 1.0e5)
                                .map(|v| v as f32)
                        })
                        .collect::<Option<Vec<f32>>>()
                });
                let Some([nx, ny, nz, d]) = values.as_deref().and_then(|v| <[f32; 4]>::try_from(v).ok())
                else {
                    return Err(bad(
                        "each plane is [nx, ny, nz, d]: 4 numbers from -100000 to 100000",
                    ));
                };
                out.push([nx, ny, nz, d]);
            }
            let shape = PostVolumeShape::ConvexHull { planes: out };
            if !shape.is_valid() {
                return Err(bad("every hull plane needs a non-zero normal"));
            }
            Ok(shape)
        }
        _ => Err(bad("`shape.kind` must be box, sphere, convex_hull or unbound")),
    }
}

/// Applies a partial `overrides` object onto `o` (`null` clears a field).
fn apply_overrides(o: &mut PostOverrides, value: &Value) -> Result<(), RpcError> {
    let map = value
        .as_object()
        .ok_or_else(|| invalid("invalid_override", "`overrides` must be an object"))?;
    for key in map.keys() {
        if !PostOverrides::FIELDS.contains(&key.as_str()) {
            return Err(invalid(
                "unknown_override",
                format!(
                    "unknown override `{key}`; one of {}",
                    PostOverrides::FIELDS.join(", ")
                ),
            ));
        }
    }
    let scalar = |key: &str, lo: f64, hi: f64| -> Result<Option<Option<f32>>, RpcError> {
        match map.get(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(v) => match v.as_f64() {
                Some(x) if x.is_finite() && (lo..=hi).contains(&x) => Ok(Some(Some(x as f32))),
                _ => Err(invalid(
                    "invalid_override",
                    format!("override `{key}` must be a number from {lo} to {hi}, or null"),
                )),
            },
        }
    };
    let rgb = |key: &str| -> Result<Option<Option<[f32; 3]>>, RpcError> {
        match map.get(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(v) => floats3(v).map(|c| Some(Some(c))).ok_or_else(|| {
                invalid(
                    "invalid_override",
                    format!("override `{key}` must be 3 finite numbers, or null"),
                )
            }),
        }
    };
    let tonemap = match map.get("tonemap") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => Some(Some(v.as_str().and_then(parse_tonemap).ok_or_else(|| {
            invalid(
                "invalid_override",
                "override `tonemap` must be agx, aces_fitted or neutral, or null",
            )
        })?)),
    };
    let eye = match map.get("eye") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::Object(e)) => {
            let ev = |k: &str| {
                e.get(k)
                    .and_then(Value::as_f64)
                    .filter(|v| v.is_finite() && (-20.0..=30.0).contains(v))
            };
            let speed = |k: &str, default: f32| match e.get(k) {
                None | Some(Value::Null) => Some(default),
                Some(v) => v
                    .as_f64()
                    .filter(|s| s.is_finite() && (0.0..=100.0).contains(s))
                    .map(|s| s as f32),
            };
            let defaults = EyeRange::default();
            match (
                ev("ev_min"),
                ev("ev_max"),
                speed("adapt_up_speed", defaults.adapt_up_speed),
                speed("adapt_down_speed", defaults.adapt_down_speed),
            ) {
                (Some(lo), Some(hi), Some(up), Some(down)) if lo <= hi => Some(Some(EyeRange {
                    ev_min: lo as f32,
                    ev_max: hi as f32,
                    adapt_up_speed: up,
                    adapt_down_speed: down,
                })),
                _ => {
                    return Err(invalid(
                        "invalid_override",
                        "override `eye` needs ev_min <= ev_max (EV100 -20..30) and speeds 0..100",
                    ));
                }
            }
        }
        Some(_) => {
            return Err(invalid(
                "invalid_override",
                "override `eye` must be an object, or null",
            ));
        }
    };
    // Every value is parsed before any is written: a bad field leaves the volume as it was.
    let parsed = [
        scalar("exposure_compensation", -16.0, 16.0)?,
        scalar("exposure_min_ev100", -20.0, 30.0)?,
        scalar("exposure_max_ev100", -20.0, 30.0)?,
        scalar("meter_center_weight", 0.0, 1.0)?,
        scalar("contrast", 0.05, 8.0)?,
        scalar("saturation", 0.0, 8.0)?,
        scalar("bloom_intensity", 0.0, 1.0)?,
        scalar("white_balance_kelvin", 1667.0, 25_000.0)?,
        scalar("tint", -1.0, 1.0)?,
        scalar("vignette", 0.0, 1.0)?,
        scalar("film_grain", 0.0, 1.0)?,
        scalar("local_shadow_contrast", 0.0, 1.0)?,
        scalar("local_highlight_contrast", 0.0, 1.0)?,
        scalar("local_detail_strength", 0.0, 4.0)?,
        scalar("ao_intensity", 0.0, 4.0)?,
        scalar("lens_flare_intensity", 0.0, 1000.0)?,
        scalar("lens_flare_threshold", 0.0, 65_504.0)?,
        scalar("lens_dirt_intensity", 0.0, 10.0)?,
        scalar("chromatic_aberration", 0.0, 0.05)?,
    ];
    let colors = [rgb("lift")?, rgb("gamma")?, rgb("gain")?, rgb("fog_tint")?];
    let scalars = [
        &mut o.exposure_compensation,
        &mut o.exposure_min_ev100,
        &mut o.exposure_max_ev100,
        &mut o.meter_center_weight,
        &mut o.contrast,
        &mut o.saturation,
        &mut o.bloom_intensity,
        &mut o.white_balance_kelvin,
        &mut o.tint,
        &mut o.vignette,
        &mut o.film_grain,
        &mut o.local_shadow_contrast,
        &mut o.local_highlight_contrast,
        &mut o.local_detail_strength,
        &mut o.ao_intensity,
        &mut o.lens_flare_intensity,
        &mut o.lens_flare_threshold,
        &mut o.lens_dirt_intensity,
        &mut o.chromatic_aberration,
    ];
    for (slot, value) in scalars.into_iter().zip(parsed) {
        if let Some(value) = value {
            *slot = value;
        }
    }
    for (slot, value) in [&mut o.lift, &mut o.gamma, &mut o.gain, &mut o.fog_tint]
        .into_iter()
        .zip(colors)
    {
        if let Some(value) = value {
            *slot = value;
        }
    }
    if let Some(value) = tonemap {
        o.tonemap = value;
    }
    if let Some(value) = eye {
        o.eye = value;
    }
    Ok(())
}

/// `render.set_post_volume { entity, enabled?, shape?, priority?, weight?, blend_distance?,
/// overrides? }`: inserts the default volume when the entity has none, then applies the given
/// fields (all validated first). Returns `{ entity, created, before, after }`.
pub(crate) fn set_post_volume(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, VOLUME_FIELDS)?;
    let entity =
        entity_param(map, "entity")?.ok_or_else(|| invalid("invalid_value", "`entity` is required"))?;
    if world.get_entity(entity).is_err() {
        return Err(missing(
            "entity_missing",
            format!("no entity {}", entity.to_bits()),
            "spawn an entity (scene.spawn) and pass its id",
        ));
    }
    let existing = world.get::<PostVolume>(entity).cloned();
    if let Some(volume) = &existing
        && !volume.is_supported()
    {
        return Err(invalid(
            "newer_format",
            format!(
                "the volume has format_version {}, newer than this engine's {POST_VOLUME_FORMAT_VERSION}",
                volume.format_version
            ),
        ));
    }
    let mut volume = existing.clone().map_or_else(PostVolume::default, |v| {
        let fallback = v.clone();
        v.migrate().unwrap_or(fallback)
    });
    let enabled = match map.get("enabled") {
        None | Some(Value::Null) => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(invalid("invalid_value", "`enabled` must be a boolean")),
    };
    let shape = match map.get("shape") {
        None | Some(Value::Null) => None,
        Some(value) => Some(parse_shape(value)?),
    };
    let priority = match map.get("priority") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_i64()
                .and_then(|p| i32::try_from(p).ok())
                .ok_or_else(|| invalid("invalid_value", "`priority` must be a 32-bit integer"))?,
        ),
    };
    let weight = number(map, "weight", 0.0, 1.0)?;
    let blend_distance = number(map, "blend_distance", 0.0, 1.0e5)?;
    let mut overrides = volume.overrides;
    if let Some(value) = map.get("overrides").filter(|v| !v.is_null()) {
        apply_overrides(&mut overrides, value)?;
    }
    let lut = match map.get("lut") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(value) => Some(Some(crate::post_volume_extras::parse_lut_path(value)?)),
    };

    let before = existing.as_ref().map(|v| volume_json(entity, v));
    if let Some(v) = enabled {
        volume.enabled = v;
    }
    if let Some(v) = shape {
        volume.shape = v;
    }
    if let Some(v) = priority {
        volume.priority = v;
    }
    if let Some(v) = weight {
        volume.weight = v;
    }
    if let Some(v) = blend_distance {
        volume.blend_distance = v;
    }
    volume.overrides = overrides;
    if let Some(v) = lut {
        volume.lut = v;
    }
    let after = volume_json(entity, &volume);
    let mut entity_mut = world.entity_mut(entity);
    entity_mut.insert(volume);
    if !entity_mut.contains::<GlobalTransform>() {
        entity_mut.insert((regia_scene::Transform::default(), GlobalTransform::default()));
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "created": existing.is_none(),
        "before": before,
        "after": after,
    }))
}

fn dof_name(mode: Option<DepthOfFieldMode>) -> &'static str {
    match mode {
        None => "off",
        Some(DepthOfFieldMode::Gaussian) => "gaussian",
        Some(DepthOfFieldMode::Bokeh) => "bokeh",
    }
}

fn lens_json(lens: &PhysicalCamera) -> Value {
    let manual_ev100 = match lens.exposure {
        CameraExposureMode::Manual { ev100 } => Some(ev100),
        _ => None,
    };
    json!({
        "exposure_mode": lens.exposure.name(),
        "manual_ev100": manual_ev100,
        "iso": lens.iso,
        "shutter_seconds": lens.shutter_seconds,
        "shutter_reference_fps": lens.shutter_reference_fps,
        "aperture": lens.aperture_f_stops,
        "focal_length_mm": lens.focal_length.map(|f| f * 1000.0),
        "sensor_height_mm": lens.sensor_height * 1000.0,
        "focus_distance": lens.focal_distance,
        "depth_of_field": dof_name(lens.depth_of_field),
        "shutter_angle": lens.shutter_angle,
        "motion_blur_samples": lens.motion_blur_samples,
    })
}

/// What the lens gives: its EV100, the EV100 the view uses (`null` for `inherit`), the motion
/// blur's shutter fraction and the field of view from the focal length.
fn derived_json(lens: &PhysicalCamera) -> Value {
    let view_ev100 = match lens.exposure {
        CameraExposureMode::Inherit => None,
        CameraExposureMode::Manual { ev100 } => Some(ev100),
        CameraExposureMode::Physical => Some(lens.ev100()),
    };
    json!({
        "ev100": lens.ev100(),
        "view_ev100": view_ev100,
        "shutter_fraction": lens.shutter_fraction(),
        "vertical_fov_deg": lens.vertical_fov().map(f32::to_degrees),
    })
}

/// The camera a lens call targets: `entity`, else the active window camera.
pub(crate) fn lens_target(world: &mut World, map: &Map<String, Value>) -> Result<Entity, RpcError> {
    let entity = match entity_param(map, "entity")? {
        Some(entity) => entity,
        None => active_camera(world).ok_or_else(camera_missing)?,
    };
    if world.get::<Camera>(entity).is_none() {
        return Err(missing(
            "not_a_camera",
            format!("entity {} has no Camera", entity.to_bits()),
            "pass a camera entity, or omit `entity` for the active window camera",
        ));
    }
    Ok(entity)
}

/// `render.get_physical_camera { entity? }`: the camera's lens (the defaults when it has none,
/// with `has_lens: false`) and what it gives.
pub(crate) fn get_physical_camera(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, &["entity"])?;
    let entity = lens_target(world, map)?;
    let lens = world.get::<PhysicalCamera>(entity).copied();
    let shown = lens.unwrap_or_default();
    Ok(json!({
        "entity": entity.to_bits(),
        "has_lens": lens.is_some(),
        "lens": lens_json(&shown),
        "derived": derived_json(&shown),
        "exposure_modes": EXPOSURE_MODES,
        "depth_of_field_modes": DOF_MODES,
    }))
}

/// `render.set_physical_camera { entity?, ... }`: inserts the default lens when the camera has
/// none, then applies the given fields (all validated first). Returns
/// `{ entity, created, before, after, derived }`.
pub(crate) fn set_physical_camera(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, LENS_FIELDS)?;
    let entity = lens_target(world, map)?;
    let existing = world.get::<PhysicalCamera>(entity).copied();
    let mut lens = existing.unwrap_or_default();

    let manual_ev = number(map, "manual_ev100", -20.0, 30.0)?;
    let mode = match map.get("exposure_mode") {
        None | Some(Value::Null) => None,
        Some(value) => match value.as_str() {
            Some("inherit") => Some(CameraExposureMode::Inherit),
            Some("physical") => Some(CameraExposureMode::Physical),
            Some("manual") => {
                let ev100 = match (manual_ev, lens.exposure) {
                    (Some(ev), _) => ev,
                    (None, CameraExposureMode::Manual { ev100 }) => ev100,
                    (None, _) => {
                        return Err(invalid(
                            "manual_needs_ev",
                            "`exposure_mode: manual` needs `manual_ev100` (the lens is not manual yet)",
                        ));
                    }
                };
                Some(CameraExposureMode::Manual { ev100 })
            }
            _ => {
                return Err(invalid(
                    "invalid_mode",
                    format!("`exposure_mode` must be one of {}", EXPOSURE_MODES.join(", ")),
                ));
            }
        },
    };
    // `manual_ev100` alone retunes a manual lens; on another mode it is an error, not a silent no-op.
    let mode = match (mode, manual_ev, lens.exposure) {
        (None, Some(ev100), CameraExposureMode::Manual { .. }) => Some(CameraExposureMode::Manual { ev100 }),
        (None, Some(_), _) => {
            return Err(invalid(
                "manual_needs_ev",
                "`manual_ev100` applies to a manual lens: also pass `exposure_mode: manual`",
            ));
        }
        (mode, _, _) => mode,
    };
    let iso = number(map, "iso", 1.0, 1.0e6)?;
    let shutter = number(map, "shutter_seconds", 1.0e-6, 3600.0)?;
    let reference_fps = number(map, "shutter_reference_fps", 1.0, 1000.0)?;
    let aperture = number(map, "aperture", 0.5, 128.0)?;
    let focal = match map.get("focal_length_mm") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(_) => Some(number(map, "focal_length_mm", 1.0, 5000.0)?.map(|mm| mm / 1000.0)),
    };
    let sensor = number(map, "sensor_height_mm", 1.0, 200.0)?;
    let focus = number(map, "focus_distance", 0.01, 1.0e5)?;
    let dof = match map.get("depth_of_field") {
        None | Some(Value::Null) => None,
        Some(value) => Some(match value.as_str() {
            Some("off") => None,
            Some("gaussian") => Some(DepthOfFieldMode::Gaussian),
            Some("bokeh") => Some(DepthOfFieldMode::Bokeh),
            _ => {
                return Err(invalid(
                    "invalid_mode",
                    format!("`depth_of_field` must be one of {}", DOF_MODES.join(", ")),
                ));
            }
        }),
    };
    let shutter_angle = number(map, "shutter_angle", 0.0, 4.0)?;
    let samples = match map.get("motion_blur_samples") {
        None | Some(Value::Null) => None,
        Some(value) => Some(
            value
                .as_u64()
                .filter(|s| *s <= 16)
                .map(|s| s as u32)
                .ok_or_else(|| invalid("invalid_value", "`motion_blur_samples` must be an integer 0..16"))?,
        ),
    };

    let before = existing.map(|l| lens_json(&l));
    if let Some(v) = mode {
        lens.exposure = v;
    }
    if let Some(v) = iso {
        lens.iso = v;
    }
    if let Some(v) = shutter {
        lens.shutter_seconds = v;
    }
    if let Some(v) = reference_fps {
        lens.shutter_reference_fps = v;
    }
    if let Some(v) = aperture {
        lens.aperture_f_stops = v;
    }
    if let Some(v) = focal {
        lens.focal_length = v;
    }
    if let Some(v) = sensor {
        lens.sensor_height = v / 1000.0;
    }
    if let Some(v) = focus {
        lens.focal_distance = v;
    }
    if let Some(v) = dof {
        lens.depth_of_field = v;
    }
    if let Some(v) = shutter_angle {
        lens.shutter_angle = v;
    }
    if let Some(v) = samples {
        lens.motion_blur_samples = v;
    }
    world.entity_mut(entity).insert(lens);
    Ok(json!({
        "entity": entity.to_bits(),
        "created": existing.is_none(),
        "before": before,
        "after": lens_json(&lens),
        "derived": derived_json(&lens),
    }))
}

#[cfg(test)]
mod tests {
    use regia_core::math::Vec3;
    use regia_scene::Transform;

    use super::*;

    fn at(x: f32) -> GlobalTransform {
        GlobalTransform(Transform::from_translation(Vec3::new(x, 0.0, 0.0)).compute_affine())
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    fn world_with_camera() -> (World, Entity) {
        let mut world = World::new();
        let camera = world.spawn((at(0.0), Camera::default())).id();
        (world, camera)
    }

    #[test]
    fn post_volumes_lists_contributions_and_resolved_values() -> Result<(), RpcError> {
        let (mut world, camera) = world_with_camera();
        let room = world
            .spawn((
                at(0.0),
                PostVolume {
                    priority: 2,
                    overrides: PostOverrides {
                        saturation: Some(0.5),
                        eye: Some(EyeRange::default()),
                        ..PostOverrides::default()
                    },
                    ..PostVolume::default()
                },
            ))
            .id();
        let far = world.spawn((at(100.0), PostVolume::default())).id();
        let out = post_volumes(&mut world, &json!({}))?;
        assert_eq!(out["camera"], camera.to_bits());
        assert_eq!(out["volumes"].as_array().map(Vec::len), Some(2));
        assert_eq!(out["contributing"].as_array().map(Vec::len), Some(1));
        assert_eq!(out["contributing"][0]["entity"], room.to_bits());
        assert_eq!(out["resolved"]["saturation"], 0.5);
        assert_eq!(out["eye_ranges"].as_array().map(Vec::len), Some(1));
        let far_row = out["volumes"]
            .as_array()
            .and_then(|rows| rows.iter().find(|r| r["entity"] == far.to_bits()))
            .cloned()
            .unwrap_or_default();
        assert_eq!(far_row["weight_here"], 0.0);
        assert_eq!(far_row["shape"]["kind"], "box");
        // At a point inside the far volume instead: it contributes, the room does not.
        let at_far = post_volumes(&mut world, &json!({ "point": [100.0, 0.0, 0.0] }))?;
        assert_eq!(at_far["camera"], Value::Null);
        assert_eq!(at_far["contributing"][0]["entity"], far.to_bits());
        Ok(())
    }

    #[test]
    fn post_volumes_errors_carry_reasons() {
        let mut empty = World::new();
        let err = post_volumes(&mut empty, &json!({})).expect_err("no camera");
        assert_eq!(reason(&err), "camera_missing");
        let (mut world, camera) = world_with_camera();
        let not_camera = world.spawn(at(1.0)).id();
        for (params, why) in [
            (json!({ "camera": not_camera.to_bits() }), "not_a_camera"),
            (
                json!({ "camera": camera.to_bits(), "point": [0, 0, 0] }),
                "camera_and_point",
            ),
            (json!({ "point": [0, 0] }), "invalid_point"),
            (json!({ "bogus": 1 }), "unknown_field"),
        ] {
            let err = post_volumes(&mut world, &params).expect_err("bad params");
            assert_eq!(reason(&err), why, "{params}");
        }
    }

    #[test]
    fn set_post_volume_creates_edits_and_validates() -> Result<(), RpcError> {
        let (mut world, _) = world_with_camera();
        let entity = world.spawn_empty().id();
        let out = set_post_volume(
            &mut world,
            &json!({
                "entity": entity.to_bits(),
                "shape": { "kind": "sphere", "radius": 4.0 },
                "priority": 3,
                "blend_distance": 2.0,
                "overrides": { "exposure_compensation": 1.0, "tonemap": "neutral", "fog_tint": [1.0, 0.8, 0.6],
                               "eye": { "ev_min": 5.0, "ev_max": 7.0 } },
            }),
        )?;
        assert_eq!(out["created"], true);
        assert_eq!(out["before"], Value::Null);
        let volume = world.get::<PostVolume>(entity).cloned().unwrap_or_default();
        assert_eq!(volume.shape, PostVolumeShape::Sphere { radius: 4.0 });
        assert_eq!(volume.priority, 3);
        assert_eq!(volume.overrides.tonemap, Some(TonemapOperator::Neutral));
        assert_eq!(
            volume.overrides.eye.map(|e| e.adapt_up_speed),
            Some(EyeRange::default().adapt_up_speed)
        );
        assert!(
            world.get::<GlobalTransform>(entity).is_some(),
            "placed at the origin"
        );
        // Edit: clear one override, set another; the rest stay.
        let out = set_post_volume(
            &mut world,
            &json!({ "entity": entity.to_bits(), "overrides": { "tonemap": null, "saturation": 0.8 }, "shape": "unbound" }),
        )?;
        assert_eq!(out["created"], false);
        let volume = world.get::<PostVolume>(entity).cloned().unwrap_or_default();
        assert_eq!(volume.overrides.tonemap, None);
        assert_eq!(volume.overrides.saturation, Some(0.8));
        assert_eq!(volume.overrides.exposure_compensation, Some(1.0));
        assert_eq!(volume.shape, PostVolumeShape::Unbound);

        // Bad input changes nothing.
        let before = world.get::<PostVolume>(entity).cloned().expect("volume"); // invariant: test-only
        for (params, why) in [
            (
                json!({ "entity": entity.to_bits(), "overrides": { "bogus": 1 } }),
                "unknown_override",
            ),
            (
                json!({ "entity": entity.to_bits(), "overrides": { "saturation": 0.5, "contrast": "x" } }),
                "invalid_override",
            ),
            (
                json!({ "entity": entity.to_bits(), "overrides": { "eye": { "ev_min": 9.0, "ev_max": 2.0 } } }),
                "invalid_override",
            ),
            (
                json!({ "entity": entity.to_bits(), "shape": { "kind": "cone" } }),
                "invalid_shape",
            ),
            (
                json!({ "entity": entity.to_bits(), "weight": 2.0 }),
                "invalid_value",
            ),
            (json!({ "entity": entity.to_bits(), "nope": 1 }), "unknown_field"),
            (json!({ "entity": 999_999_999_u64 }), "entity_missing"),
        ] {
            let err = set_post_volume(&mut world, &params).expect_err("bad input");
            assert_eq!(reason(&err), why, "{params}");
        }
        assert_eq!(world.get::<PostVolume>(entity).cloned(), Some(before));

        // A volume from a newer engine is not edited.
        world.entity_mut(entity).insert(PostVolume {
            format_version: POST_VOLUME_FORMAT_VERSION + 1,
            ..PostVolume::default()
        });
        let err = set_post_volume(&mut world, &json!({ "entity": entity.to_bits(), "weight": 0.5 }))
            .expect_err("newer");
        assert_eq!(reason(&err), "newer_format");
        Ok(())
    }

    #[test]
    fn physical_camera_get_and_set() -> Result<(), RpcError> {
        let (mut world, camera) = world_with_camera();
        let got = get_physical_camera(&mut world, &json!({}))?;
        assert_eq!(got["entity"], camera.to_bits());
        assert_eq!(got["has_lens"], false);
        assert_eq!(got["lens"]["exposure_mode"], "inherit");
        assert_eq!(got["derived"]["view_ev100"], Value::Null);

        let out = set_physical_camera(
            &mut world,
            &json!({ "exposure_mode": "physical", "aperture": 16.0, "shutter_seconds": 0.008, "iso": 100.0,
                     "focal_length_mm": 35.0, "sensor_height_mm": 24.0, "depth_of_field": "bokeh" }),
        )?;
        assert_eq!(out["created"], true);
        let ev = out["derived"]["view_ev100"].as_f64().unwrap_or_default();
        assert!((ev - 14.97).abs() < 0.01, "{ev}");
        let lens = world.get::<PhysicalCamera>(camera).copied().unwrap_or_default();
        assert_eq!(lens.exposure, CameraExposureMode::Physical);
        assert_eq!(lens.depth_of_field, Some(DepthOfFieldMode::Bokeh));
        assert!((lens.focal_length.unwrap_or_default() - 0.035).abs() < 1.0e-6);
        assert!(out["derived"]["vertical_fov_deg"].as_f64().is_some());

        set_physical_camera(
            &mut world,
            &json!({ "exposure_mode": "manual", "manual_ev100": 9.0 }),
        )?;
        let lens = world.get::<PhysicalCamera>(camera).copied().unwrap_or_default();
        assert_eq!(lens.exposure, CameraExposureMode::Manual { ev100: 9.0 });
        // manual_ev100 alone retunes a manual lens.
        set_physical_camera(&mut world, &json!({ "manual_ev100": 10.0 }))?;
        let lens = world.get::<PhysicalCamera>(camera).copied().unwrap_or_default();
        assert_eq!(lens.exposure, CameraExposureMode::Manual { ev100: 10.0 });
        set_physical_camera(
            &mut world,
            &json!({ "focal_length_mm": null, "exposure_mode": "inherit" }),
        )?;
        let lens = world.get::<PhysicalCamera>(camera).copied().unwrap_or_default();
        assert_eq!(lens.focal_length, None);

        let not_camera = world.spawn(at(2.0)).id();
        for (params, why) in [
            (json!({ "exposure_mode": "auto" }), "invalid_mode"),
            (json!({ "exposure_mode": "manual" }), "manual_needs_ev"),
            (json!({ "manual_ev100": 3.0 }), "manual_needs_ev"),
            (json!({ "depth_of_field": "tilt" }), "invalid_mode"),
            (json!({ "iso": 0.0 }), "invalid_value"),
            (json!({ "entity": not_camera.to_bits() }), "not_a_camera"),
            (json!({ "zoom": 2 }), "unknown_field"),
        ] {
            let err = set_physical_camera(&mut world, &params).expect_err("bad input");
            assert_eq!(reason(&err), why, "{params}");
        }
        let err = get_physical_camera(&mut World::new(), &json!({})).expect_err("no camera");
        assert_eq!(reason(&err), "camera_missing");
        Ok(())
    }

    #[test]
    fn book_and_guide_name_every_field_and_reason() {
        let book = include_str!("../../../book/agents/post-volumes.md");
        for name in PostOverrides::FIELDS
            .iter()
            .chain(LENS_FIELDS)
            .chain(VOLUME_FIELDS)
        {
            assert!(
                book.contains(&format!("`{name}`")),
                "book/agents/post-volumes.md lacks `{name}`"
            );
        }
        for reason in [
            "camera_missing",
            "not_a_camera",
            "entity_missing",
            "camera_and_point",
            "invalid_point",
            "invalid_shape",
            "unknown_field",
            "unknown_override",
            "invalid_override",
            "invalid_value",
            "invalid_mode",
            "manual_needs_ev",
            "newer_format",
            "invalid_lut",
            "not_a_volume",
        ] {
            assert!(
                book.contains(&format!("`{reason}`")),
                "book page lacks reason `{reason}`"
            );
        }
        let guide = crate::discovery::guide(&json!({ "topic": "post-volumes" }))
            .expect("post-volumes topic exists") // invariant: test-only (cfg(test))
            .to_string();
        for method in [
            "render.post_volumes",
            "render.set_post_volume",
            "render.get_physical_camera",
            "render.set_physical_camera",
            "render.set_post_volume_luts",
            "render.post_volume_gizmos",
        ] {
            assert!(guide.contains(method), "guide lacks {method}");
            assert!(book.contains(method), "book lacks {method}");
        }
    }
}
