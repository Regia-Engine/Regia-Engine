// SPDX-License-Identifier: MPL-2.0
//! The lighting option on the agent protocol (`book/agents/gi-probes.md`, ADR-133 gi-look).
//! `render.get_gi_mode` reads it and `render.set_gi_mode` switches it. The option has three
//! looks: `"balanced"` (the analytic sky and bounce terms), `"depth"` (GPU irradiance probes,
//! desktop only) and `"baked"` (the map's baked GI volume, `book/agents/baked-gi.md`; a map
//! with a volume uses it under `depth` and the default too). The choice is written to
//! [`RenderSettings::gi_look`] for the next extracted frame. When the app has [`GameSettings`],
//! `balanced` and `depth` are also written to `graphics.lighting`, which a game's options menu
//! shows and which persists with the other graphics settings (`baked` is not a menu choice: it
//! leaves the persisted setting alone).
//!
//! `render.set_gi_occluders` replaces the capsule occluders that darken the baked light next to
//! dynamic units (`book/agents/baked-gi.md`), for inspection and tests, and can draw them as
//! gizmos.
//!
//! `render.sample_baked_gi` looks the installed volume up at given points on the CPU, with the
//! ADR-861 quality switches optionally overridden (`book/agents/baked-gi-quality.md`).
//!
//! `render.set_baked_gi_ao` switches the probe-relative SSAO over the baked volume (ADR-1104,
//! `book/agents/baked-gi-ao.md`) off and on, for a side-by-side of the double-counted look.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::baked_gi::{GI_COMPONENTS, GiComponent};
use regia_core::math::{Color, Vec3};
use regia_framework::settings::{GameSettings, LightingMode};
use regia_render::baked_gi::{
    BakedGiSettings, GiCapsule, GiVisibilityFilter, MAX_GI_OCCLUDERS, MAX_LEAK_REDUCTION, SunRelight,
    sample_static_gi_relit,
};
use regia_render::gizmos::{Gizmo, GizmoAsset};
use regia_render::{GiLook, RenderSettings, RuntimeRenderer};
use serde_json::{Value, json};

use crate::protocol::RpcError;

const MODES: &str = "\"balanced\" (analytic sky + bounce light, every platform), \"depth\" (GPU-traced \
                     bounce light, desktop only), \"baked\" (the map's baked GI volume, desktop only) or \
                     \"default\" (the quality preset's choice)";

fn missing() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (it inserts RenderSettings)",
    )
}

fn state(world: &World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(missing)?;
    let effective = settings.effective_gi_look();
    let baked = world.get_resource::<RuntimeRenderer>().map(|r| {
        let s = r.0.baked_gi_status();
        let lookup = r.0.baked_gi_settings();
        json!({
            "installed": s.installed,
            "active": s.active,
            "probes": s.probes,
            "gpu_bytes": s.gpu_bytes,
            "error": s.error,
            "scales": s.scales,
            "sun_mismatch_deg": s.sun_mismatch_deg,
            "occluders": s.occluders,
            "occluder_cap": MAX_GI_OCCLUDERS,
            "occluders_dropped": s.occluders_dropped,
            "interior_bricks": s.interior_bricks,
            "wall_probes": s.wall_probes,
            "brick_grid": { "cells": s.brick_grid_cells, "entries": s.brick_grid_entries },
            "probe_relative_ao": s.probe_relative_ao,
            "sun_relight": s.sun_relight.ground_factor(),
            "lookup": {
                "deringing": lookup.deringing,
                "leak_guard": lookup.leak_guard,
                "sun_relight": lookup.sun_relight,
                "zh3": lookup.zh3,
            },
            // ADR-1386: emitters the bake recorded (null = a v1/v2 volume that names none).
            "emitters": s.emitters,
            // ADR-2618: the volumes the last frame blended (split parts, storage slots) and the
            // brick-loop probe's histogram.
            "volumes": crate::baked_gi_volumes::volumes_json(&s),
            "volume_slots": s.volume_slots,
            "brick_visits": crate::baked_gi_volumes::brick_visits_json(&s),
        })
    });
    Ok(json!({
        "baked": baked,
        // ADR-2656: the installed lightmap and its receivers (book/agents/lightmaps.md).
        "lightmaps": crate::lightmaps::gi_summary(world),
        "uses_bake": settings.quality().baked_gi,
        "mode": effective.name(),
        "probes": effective.runs_probes(),
        "setting": settings.gi_look.map_or("default", GiLook::name),
        "preset_default": GiLook::of_profile(&settings.profile).name(),
        "depth_available": GiLook::depth_available(),
        "persisted": world
            .get_resource::<GameSettings>()
            .map(|s| s.graphics.lighting.map_or("default", |m| m.gi_look().name())),
    }))
}

/// `render.get_gi_mode`: the lighting option in effect, the player's setting, the preset's
/// default and whether `depth` can run on this platform.
pub(crate) fn get_gi_mode(world: &mut World) -> Result<Value, RpcError> {
    state(world)
}

/// `render.set_gi_mode`: `mode` is `"balanced"`, `"depth"`, `"baked"` (`"static"`) or
/// `"default"` (`"terms"` and `"probes"` alias balanced and depth). `"baked"` sets
/// `GiLook::Baked` and does not write `GameSettings.graphics.lighting`. On Android, `"depth"`
/// and `"baked"` are refused. Returns the state before and after.
pub(crate) fn set_gi_mode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match crate::params::opt_str(params, "mode")? {
        Some("balanced" | "terms") => Some(LightingMode::Balanced),
        Some("depth" | "probes") => Some(LightingMode::Depth),
        Some("baked" | "static") => {
            if !GiLook::depth_available() {
                return Err(RpcError::invalid_params(
                    "\"baked\" (the baked GI volume) is not available on this platform; use \"balanced\"",
                ));
            }
            let before = state(world)?;
            if let Some(mut settings) = world.get_resource_mut::<RenderSettings>() {
                settings.gi_look = Some(GiLook::Baked);
            }
            let after = state(world)?;
            return Ok(json!({
                "before": before,
                "after": after,
                "note": "applies from the next extracted frame; the map needs an installed volume \
                         (FrameRenderer::set_baked_gi), else it shades as balanced. graphics.lighting \
                         is left as it was",
            }));
        }
        Some("default") => None,
        Some(other) => {
            return Err(RpcError::invalid_params(format!(
                "unknown GI mode {other:?}: use {MODES}"
            )));
        }
        None => return Err(RpcError::invalid_params(format!("`mode` is required: {MODES}"))),
    };
    if mode == Some(LightingMode::Depth) && !GiLook::depth_available() {
        return Err(RpcError::invalid_params(
            "\"depth\" (GPU irradiance probes) is not available on this platform; use \"balanced\"",
        ));
    }
    let before = state(world)?;
    if let Some(mut settings) = world.get_resource_mut::<RenderSettings>() {
        settings.gi_look = mode.map(LightingMode::gi_look);
    }
    if let Some(mut game) = world.get_resource_mut::<GameSettings>()
        && game.graphics.lighting != mode
    {
        game.graphics.lighting = mode;
    }
    let after = state(world)?;
    Ok(json!({
        "before": before,
        "after": after,
        "note": "applies from the next extracted frame; depth needs a linear-HDR (post stack) \
                 frame and about 30 frames to settle after a switch or a camera cut",
    }))
}

/// The gizmo entity `render.set_gi_occluders {gizmos: true}` draws the capsules with.
#[derive(Component)]
struct GiOccluderGizmo;

fn bad_capsule(i: usize, what: &str) -> RpcError {
    RpcError::invalid_params(format!(
        "capsules[{i}]: {what}; a capsule is {{ a: [x, y, z], b: [x, y, z] (optional, default a), \
         radius: metres > 0, strength: 0..1 (optional, default 1) }}"
    ))
}

fn capsule_from(v: &Value, i: usize) -> Result<GiCapsule, RpcError> {
    let point = |key: &str| -> Result<Option<[f32; 3]>, RpcError> {
        let Some(raw) = v.get(key) else {
            return Ok(None);
        };
        let xyz = raw
            .as_array()
            .filter(|a| a.len() == 3)
            .ok_or_else(|| bad_capsule(i, &format!("`{key}` must be an array of 3 numbers")))?;
        let mut out = [0.0f32; 3];
        for (o, x) in out.iter_mut().zip(xyz) {
            *o = x
                .as_f64()
                .ok_or_else(|| bad_capsule(i, &format!("`{key}` must be an array of 3 numbers")))?
                as f32;
        }
        Ok(Some(out))
    };
    let a = point("a")?.ok_or_else(|| bad_capsule(i, "`a` is required"))?;
    let b = point("b")?.unwrap_or(a);
    let radius = v
        .get("radius")
        .and_then(Value::as_f64)
        .ok_or_else(|| bad_capsule(i, "`radius` is required"))? as f32;
    let strength = match v.get("strength") {
        None => 1.0,
        Some(s) => s
            .as_f64()
            .ok_or_else(|| bad_capsule(i, "`strength` must be a number"))? as f32,
    };
    let capsule = GiCapsule {
        a,
        b,
        radius,
        strength,
    };
    if !capsule.is_usable() || strength > 1.0 {
        return Err(bad_capsule(
            i,
            "ends must be finite, radius > 0 and strength in (0, 1]",
        ));
    }
    Ok(capsule)
}

/// A wire capsule: a sphere at each end and four lines along the sides.
fn capsule_gizmos(capsules: &[GiCapsule]) -> GizmoAsset {
    let mut asset = GizmoAsset::new();
    let color = Color::YELLOW;
    for c in capsules {
        let (a, b) = (Vec3::from_array(c.a), Vec3::from_array(c.b));
        asset.sphere(a, c.radius, color);
        let axis = b - a;
        if axis.length_squared() <= 1e-12 {
            continue;
        }
        asset.sphere(b, c.radius, color);
        let axis = axis.normalize();
        let helper = if axis.y.abs() < 0.9 { Vec3::Y } else { Vec3::X };
        let u = axis.cross(helper).normalize() * c.radius;
        let w = axis.cross(u);
        for side in [u, -u, w, -w] {
            asset.line(a + side, b + side, color);
        }
    }
    asset
}

/// `render.set_gi_occluders`: `capsules` = `[{ a, b, radius, strength }]` replaces the capsule
/// occluders over the baked GI volume (`[]` clears them); `gizmos: true` also draws them. The
/// host's own calls (`FrameRenderer::set_gi_occluders`, usually every frame) replace an agent's
/// list. Returns the count in use, the cap and how many were dropped past it.
pub(crate) fn set_gi_occluders(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let list = params.get("capsules").and_then(Value::as_array).ok_or_else(|| {
        RpcError::invalid_params(
            "`capsules` is required: a list of { a, b, radius, strength } (world metres); [] clears",
        )
    })?;
    let capsules = list
        .iter()
        .enumerate()
        .map(|(i, v)| capsule_from(v, i))
        .collect::<Result<Vec<_>, _>>()?;
    let gizmos = match params.get("gizmos") {
        None => false,
        Some(g) => g
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`gizmos` must be true or false"))?,
    };
    let (used, kept, status) = {
        let mut renderer = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(|| {
            RpcError::not_found(
                "no RuntimeRenderer resource: this app has no runtime renderer",
                "add regia_render::RenderPlugin",
            )
        })?;
        let used = renderer.set_gi_occluders(&capsules);
        (
            used,
            renderer.0.gi_occluders().to_vec(),
            renderer.0.baked_gi_status(),
        )
    };
    let old: Vec<Entity> = world
        .query_filtered::<Entity, With<GiOccluderGizmo>>()
        .iter(world)
        .collect();
    for e in old {
        world.despawn(e);
    }
    let drawn = gizmos && !kept.is_empty();
    if drawn {
        world.spawn((Gizmo::new(Arc::new(capsule_gizmos(&kept))), GiOccluderGizmo));
    }
    Ok(json!({
        "count": used,
        "cap": MAX_GI_OCCLUDERS,
        "dropped": status.occluders_dropped,
        "gizmos": drawn,
        "volume_installed": status.installed,
        "note": "the baked screen pass applies them from the next frame, only while a baked GI \
                 volume lights it; the host's own set_gi_occluders calls replace this list",
    }))
}

/// Most receivers one `render.sample_baked_gi` call takes.
const MAX_SAMPLE_POINTS: usize = 1024;

/// A unit vector from `v[key]` (3 numbers, not all 0), or `default` when the key is absent.
fn unit_param(v: &Value, key: &str, i: usize, default: Option<[f32; 3]>) -> Result<[f32; 3], RpcError> {
    let bad = || {
        RpcError::invalid_params(format!(
            "points[{i}].{key} must be an array of 3 finite numbers (n and v not all 0); a point is \
             {{ p: [x, y, z], n: [x, y, z] (optional, default up), v: [x, y, z] (optional, default n) }}"
        ))
    };
    let Some(raw) = v.get(key) else {
        return default.ok_or_else(bad);
    };
    let xyz = raw.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(xyz) {
        *o = x.as_f64().filter(|x| x.is_finite()).ok_or_else(bad)? as f32;
    }
    if key == "p" {
        return Ok(out);
    }
    let len = (out[0] * out[0] + out[1] * out[1] + out[2] * out[2]).sqrt();
    if len <= 1e-6 {
        return Err(bad());
    }
    Ok(out.map(|c| c / len))
}

/// The `visibility_filter` override of `render.sample_baked_gi` (ADR-1105), `default` when absent.
fn filter_param(params: &Value, default: GiVisibilityFilter) -> Result<GiVisibilityFilter, RpcError> {
    match params.get("visibility_filter") {
        None => Ok(default),
        Some(v) => v.as_str().and_then(GiVisibilityFilter::from_name).ok_or_else(|| {
            RpcError::invalid_params("`visibility_filter` must be \"per_tap\" or \"moments\"")
        }),
    }
}

/// The `leak_reduction` override of `render.sample_baked_gi` (ADR-1105), `default` when absent.
fn leak_param(params: &Value, default: f32) -> Result<f32, RpcError> {
    match params.get("leak_reduction") {
        None => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|k| k as f32)
            .filter(|k| (0.0..=MAX_LEAK_REDUCTION).contains(k))
            .ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "`leak_reduction` must be a number in 0..={MAX_LEAK_REDUCTION} (0 = off)"
                ))
            }),
    }
}

fn switch(params: &Value, key: &str, default: bool) -> Result<bool, RpcError> {
    match params.get(key) {
        None => Ok(default),
        Some(b) => b
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params(format!("`{key}` must be true or false"))),
    }
}

/// `render.sample_baked_gi`: the installed baked GI volume looked up on the CPU at `points`
/// (`sample_static_gi_relit`, the shader's mirror) with the renderer's settings, the last frame's
/// light factors and its sun relight. `deringing`, `leak_guard` and `sun_relight` override the
/// ADR-861 quality switches for this call only, so an agent can see what each one changes at a
/// point; `visibility_filter` and `leak_reduction` do the same for the ADR-1105 leak control,
/// and `zh3` for the evaluation of the SH (ADR-1377).
/// Before a frame used the volume the factors are the baked light levels (1) and the sun
/// is as baked.
pub(crate) fn sample_baked_gi(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let list = params.get("points").and_then(Value::as_array).ok_or_else(|| {
        RpcError::invalid_params(
            "`points` is required: a list of { p: [x, y, z], n: [x, y, z], v: [x, y, z] }",
        )
    })?;
    if list.is_empty() || list.len() > MAX_SAMPLE_POINTS {
        return Err(RpcError::invalid_params(format!(
            "`points` must hold 1 to {MAX_SAMPLE_POINTS} receivers ({} given)",
            list.len()
        )));
    }
    let mut points = Vec::with_capacity(list.len());
    for (i, v) in list.iter().enumerate() {
        let p = unit_param(v, "p", i, None)?;
        let n = unit_param(v, "n", i, Some([0.0, 1.0, 0.0]))?;
        let view = unit_param(v, "v", i, Some(n))?;
        points.push((p, n, view));
    }
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(|| {
        RpcError::not_found(
            "no RuntimeRenderer resource: this app has no runtime renderer",
            "add regia_render::RenderPlugin",
        )
    })?;
    let volume = renderer.0.baked_gi().cloned().ok_or_else(|| {
        RpcError::not_found(
            "no baked GI volume is installed",
            "install one with FrameRenderer::set_baked_gi or a level's baked_gi (render.get_gi_mode shows baked.installed)",
        )
    })?;
    let base = renderer.0.baked_gi_settings();
    let status = renderer.0.baked_gi_status();
    let settings = BakedGiSettings {
        deringing: switch(params, "deringing", base.deringing)?,
        leak_guard: switch(params, "leak_guard", base.leak_guard)?,
        sun_relight: switch(params, "sun_relight", base.sun_relight)?,
        visibility_filter: filter_param(params, base.visibility_filter)?,
        leak_reduction: leak_param(params, base.leak_reduction)?,
        zh3: switch(params, "zh3", base.zh3)?,
        ..base
    };
    let scales = if status.active {
        status.scales
    } else {
        [[1.0; 3]; GI_COMPONENTS]
    };
    // The last frame's relight, which the renderer computed under its own switch: an override can
    // turn it off, not on when the renderer had it off.
    let relight = if settings.sun_relight && status.active {
        status.sun_relight
    } else {
        SunRelight::NONE
    };
    let samples: Vec<Value> = points
        .iter()
        .map(|&(p, n, v)| {
            let s = sample_static_gi_relit(&volume, &settings, &scales, &relight, p, n, v);
            let components: serde_json::Map<String, Value> = GiComponent::ALL
                .iter()
                .map(|c| (c.name().to_owned(), json!(s.components[*c as usize])))
                .collect();
            json!({
                "irradiance_over_pi": s.irradiance_over_pi,
                "components": components,
                "coverage": s.coverage,
                "visibility": s.visibility,
            })
        })
        .collect();
    Ok(json!({
        "samples": samples,
        "lookup": {
            "deringing": settings.deringing,
            "leak_guard": settings.leak_guard,
            "sun_relight": settings.sun_relight,
            "visibility_filter": settings.visibility_filter.name(),
            "leak_reduction": settings.effective_leak_reduction(),
            "zh3": settings.zh3,
        },
        "scales": scales,
        "sun_relight": relight.ground_factor(),
        "from_frame": status.active,
    }))
}

/// `render.set_baked_gi_ao`: `probe_relative` = `true` (the default) lets the SSAO count, where the
/// baked volume lights a pixel, only the occlusion its probes do not already hold, and the
/// capsules leave the SSAO's share of theirs to it; `false` multiplies both in whole (the old,
/// double-counted look). Returns the setting before and after and whether the split ran last
/// frame.
pub(crate) fn set_baked_gi_ao(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let on = params
        .get("probe_relative")
        .and_then(Value::as_bool)
        .ok_or_else(|| RpcError::invalid_params("`probe_relative` is required: true or false"))?;
    let mut renderer = world.get_resource_mut::<RuntimeRenderer>().ok_or_else(|| {
        RpcError::not_found(
            "no RuntimeRenderer resource: this app has no runtime renderer",
            "add regia_render::RenderPlugin",
        )
    })?;
    let mut settings = renderer.0.baked_gi_settings();
    let before = settings.probe_relative_ao;
    settings.probe_relative_ao = on;
    renderer.0.set_baked_gi_settings(settings);
    let status = renderer.0.baked_gi_status();
    Ok(json!({
        "before": before,
        "probe_relative": on,
        "ran_last_frame": status.probe_relative_ao,
        "volume_installed": status.installed,
        "note": "applies from the next frame, only while a baked GI volume lights it and the SSAO \
                 runs; render.get_gi_mode reports baked.probe_relative_ao",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_between_balanced_and_depth() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        let got = get_gi_mode(&mut world).expect("get works");
        assert_eq!(got["lightmaps"]["installed"], false, "{got}");
        assert_eq!(got["lightmaps"]["stale_instances"], 0, "{got}");
        // The default settings' preset (Desktop High) ships Depth.
        assert_eq!(got["mode"], "depth", "Desktop High ships the Depth look");
        assert_eq!(got["setting"], "default");
        let set = set_gi_mode(&mut world, &json!({ "mode": "balanced" })).expect("set works");
        assert_eq!(set["before"]["mode"], "depth");
        assert_eq!(set["after"]["mode"], "balanced");
        assert!(!world.resource::<RenderSettings>().quality().gi_probes);
        let set = set_gi_mode(&mut world, &json!({ "mode": "depth" })).expect("set works");
        assert_eq!(set["after"]["mode"], "depth");
        assert!(world.resource::<RenderSettings>().quality().gi_probes);
        set_gi_mode(&mut world, &json!({ "mode": "terms" })).expect("alias works");
        assert!(!world.resource::<RenderSettings>().quality().gi_probes);
        set_gi_mode(&mut world, &json!({ "mode": "default" })).expect("default works");
        assert_eq!(world.resource::<RenderSettings>().gi_look, None);
    }

    #[test]
    fn writes_the_persisted_graphics_setting() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        world.insert_resource(GameSettings::default());
        set_gi_mode(&mut world, &json!({ "mode": "depth" })).expect("set works");
        assert_eq!(
            world.resource::<GameSettings>().graphics.lighting,
            Some(LightingMode::Depth)
        );
        let got = get_gi_mode(&mut world).expect("get works");
        assert_eq!(got["persisted"], "depth");
    }

    #[test]
    fn baked_is_a_look_that_leaves_the_persisted_setting_alone() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        world.insert_resource(GameSettings::default());
        set_gi_mode(&mut world, &json!({ "mode": "balanced" })).expect("set works");
        assert!(
            !world.resource::<RenderSettings>().quality().baked_gi,
            "balanced ignores a bake"
        );
        let set = set_gi_mode(&mut world, &json!({ "mode": "baked" })).expect("set works");
        assert_eq!(set["after"]["mode"], "baked");
        assert_eq!(set["after"]["uses_bake"], true);
        assert_eq!(set["after"]["probes"], false);
        assert_eq!(world.resource::<RenderSettings>().gi_look, Some(GiLook::Baked));
        assert_eq!(
            world.resource::<GameSettings>().graphics.lighting,
            Some(LightingMode::Balanced),
            "the menu setting is untouched"
        );
        // Depth and the default use a map's bake too.
        set_gi_mode(&mut world, &json!({ "mode": "depth" })).expect("set works");
        assert!(world.resource::<RenderSettings>().quality().baked_gi);
        set_gi_mode(&mut world, &json!({ "mode": "default" })).expect("set works");
        assert!(world.resource::<RenderSettings>().quality().baked_gi);
        // Without a runtime renderer there is no volume status.
        assert!(get_gi_mode(&mut world).expect("get")["baked"].is_null());
        world.insert_resource(RuntimeRenderer::default());
        let got = get_gi_mode(&mut world).expect("get");
        assert_eq!(got["baked"]["installed"], false);
        assert_eq!(got["baked"]["active"], false);
        assert_eq!(got["baked"]["occluders"], 0);
        assert_eq!(got["baked"]["occluder_cap"], MAX_GI_OCCLUDERS);
        assert_eq!(got["baked"]["interior_bricks"], 0);
        assert_eq!(got["baked"]["wall_probes"], 0);
        assert_eq!(got["baked"]["brick_grid"]["cells"], 0);
        assert_eq!(got["baked"]["brick_grid"]["entries"], 0);
        // ADR-861: the ground factor is 1 (as baked) before a frame used a volume; the quality
        // switches default on.
        assert_eq!(got["baked"]["sun_relight"], 1.0);
        assert_eq!(got["baked"]["lookup"]["deringing"], true);
        assert_eq!(got["baked"]["lookup"]["leak_guard"], true);
        assert_eq!(got["baked"]["lookup"]["sun_relight"], true);
        assert!(got["baked"]["emitters"].is_null(), "no volume: no emitter list");
    }

    #[test]
    fn sets_counts_and_draws_the_occluders() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        assert!(
            set_gi_occluders(&mut world, &json!({ "capsules": [] })).is_err(),
            "no renderer"
        );
        world.insert_resource(RuntimeRenderer::default());
        let set = set_gi_occluders(
            &mut world,
            &json!({
                "capsules": [
                    { "a": [0.0, 1.0, 0.0], "b": [2.0, 1.0, 0.0], "radius": 0.5 },
                    { "a": [4.0, 1.0, 0.0], "radius": 1.0, "strength": 0.5 },
                ],
                "gizmos": true,
            }),
        )
        .expect("set works");
        assert_eq!(set["count"], 2);
        assert_eq!(set["cap"], MAX_GI_OCCLUDERS);
        assert_eq!(set["dropped"], 0);
        assert_eq!(set["gizmos"], true);
        let drawn = |world: &mut World| world.query::<&GiOccluderGizmo>().iter(world).count();
        assert_eq!(drawn(&mut world), 1);
        let got = get_gi_mode(&mut world).expect("get");
        assert_eq!(got["baked"]["occluders"], 2);
        let r = world.resource::<RuntimeRenderer>();
        assert_eq!(r.0.gi_occluders()[1].b, [4.0, 1.0, 0.0], "b defaults to a");
        assert_eq!(r.0.gi_occluders()[1].strength, 0.5);
        // Past the cap: the rest are dropped and counted.
        let many: Vec<Value> = (0..MAX_GI_OCCLUDERS + 2)
            .map(|k| json!({ "a": [k as f64, 0.0, 0.0], "radius": 0.25 }))
            .collect();
        let set = set_gi_occluders(&mut world, &json!({ "capsules": many })).expect("set works");
        assert_eq!(set["count"], MAX_GI_OCCLUDERS);
        assert_eq!(set["dropped"], 2);
        assert_eq!(drawn(&mut world), 0, "gizmos off removes the old drawing");
        // An empty list clears them.
        let set = set_gi_occluders(&mut world, &json!({ "capsules": [] })).expect("set works");
        assert_eq!(set["count"], 0);
        assert_eq!(get_gi_mode(&mut world).expect("get")["baked"]["occluders"], 0);
    }

    #[test]
    fn rejects_malformed_occluders() {
        let mut world = World::new();
        world.insert_resource(RenderSettings::default());
        world.insert_resource(RuntimeRenderer::default());
        for bad in [
            json!({}),
            json!({ "capsules": [{ "radius": 1.0 }] }),
            json!({ "capsules": [{ "a": [0.0, 1.0], "radius": 1.0 }] }),
            json!({ "capsules": [{ "a": [0.0, 1.0, 0.0] }] }),
            json!({ "capsules": [{ "a": [0.0, 1.0, 0.0], "radius": -1.0 }] }),
            json!({ "capsules": [{ "a": [0.0, 1.0, 0.0], "radius": 1.0, "strength": 2.0 }] }),
            json!({ "capsules": [], "gizmos": "yes" }),
        ] {
            assert!(set_gi_occluders(&mut world, &bad).is_err(), "{bad}");
        }
        assert!(world.resource::<RuntimeRenderer>().0.gi_occluders().is_empty());
    }

    #[test]
    fn rejects_unknown_modes_and_missing_renderer() {
        let mut world = World::new();
        assert!(get_gi_mode(&mut world).is_err());
        world.insert_resource(RenderSettings::default());
        assert!(set_gi_mode(&mut world, &json!({ "mode": "lumen" })).is_err());
        assert!(set_gi_mode(&mut world, &json!({})).is_err());
        assert_eq!(world.resource::<RenderSettings>().gi_look, None);
    }

    /// A terrain layer (columns 0..12 m, ground at 10 m, levels 1, 2, 4 m) whose every probe
    /// holds a uniform sky of radiance (2, 1, 0.5).
    fn uniform_volume() -> Arc<regia_asset::baked_gi::BakedGiVolume> {
        use regia_asset::baked_gi::{
            BakeInfo, BakeLights, BakedGiVolume, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, TerrainLayer,
            VolumeLayout,
        };
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [4, 4],
                levels: vec![1.0, 2.0, 4.0],
                ground: vec![10.0; 16],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let k = 4.0 * std::f32::consts::PI * SH_Y00;
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| {
                let mut sh = [[[0.0f32; 4]; 3]; GI_COMPONENTS];
                sh[GiComponent::Sky as usize] = [
                    [2.0 * k, 0.0, 0.0, 0.0],
                    [k, 0.0, 0.0, 0.0],
                    [0.5 * k, 0.0, 0.0, 0.0],
                ];
                ProbeData {
                    sh,
                    moments: vec![[16.0, 256.0]; 16],
                    meta: ProbeMeta {
                        offset: [0.0; 3],
                        flags: ProbeFlags::VALID,
                    },
                }
            })
            .collect();
        let volume = BakedGiVolume::from_probes(
            &layout,
            BakeLights {
                sky_scale: 1.0,
                ..BakeLights::default()
            },
            BakeInfo::default(),
            &probes,
        )
        .expect("volume");
        Arc::new(volume)
    }

    #[test]
    fn samples_the_installed_volume_with_the_quality_switches() {
        let one = json!({ "points": [{ "p": [6.0, 11.5, 6.0] }] });
        let mut world = World::new();
        assert!(sample_baked_gi(&mut world, &one).is_err(), "no renderer");
        world.insert_resource(RuntimeRenderer::default());
        let err = sample_baked_gi(&mut world, &one).expect_err("no volume");
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0003")));
        world
            .resource_mut::<RuntimeRenderer>()
            .set_baked_gi(Some(uniform_volume()));
        let got = sample_baked_gi(
            &mut world,
            &json!({
                "points": [
                    { "p": [6.0, 11.5, 6.0] },
                    { "p": [6.0, 11.5, 6.0], "n": [2.0, 0.0, 0.0] },
                    { "p": [-20.0, 11.5, 6.0] },
                ],
                "deringing": false,
            }),
        )
        .expect("sample");
        // Before a frame used the volume: the baked light levels, the sun as baked.
        assert_eq!(got["from_frame"], false);
        assert_eq!(got["sun_relight"], 1.0);
        assert_eq!(got["lookup"]["deringing"], false, "overridden for this call");
        assert_eq!(got["lookup"]["leak_guard"], true, "the renderer's setting");
        assert_eq!(
            got["lookup"]["visibility_filter"], "per_tap",
            "the default (ADR-1105)"
        );
        let samples = got["samples"].as_array().expect("samples");
        assert_eq!(samples.len(), 3);
        for s in &samples[..2] {
            let e = s["irradiance_over_pi"].as_array().expect("rgb");
            let rgb: Vec<f64> = e.iter().map(|v| v.as_f64().unwrap_or(-1.0)).collect();
            for (got, want) in rgb.iter().zip([2.0, 1.0, 0.5]) {
                assert!((got - want).abs() < 0.02 * want, "{s}");
            }
            assert!((s["coverage"].as_f64().unwrap_or(0.0) - 1.0).abs() < 1e-5, "{s}");
            assert!(
                (s["components"]["sky"][0].as_f64().unwrap_or(0.0) - 2.0).abs() < 0.04,
                "{s}"
            );
            assert_eq!(s["components"]["sun_bounce"], json!([0.0, 0.0, 0.0]));
        }
        assert_eq!(samples[2]["coverage"], 0.0, "outside the volume");
        // The published example is a point object and succeeds once a volume is installed.
        // The bare array `level.sample_baked_gi` accepts is invalid params here.
        let catalog = crate::contract::contract("render.sample_baked_gi").expect("contract");
        assert!(catalog.errors.contains(&"F0003"));
        let sampled = sample_baked_gi(&mut world, &catalog.example_json()).expect("catalog example");
        assert_eq!(sampled["samples"].as_array().map(Vec::len), Some(1));
        let bare = sample_baked_gi(&mut world, &json!({ "points": [[0.0, 1.0, 0.0]] }))
            .expect_err("a bare array has no p");
        assert_eq!(
            bare.data.as_ref().map(|d| d["code"].clone()),
            Some(json!("F0002"))
        );
        // The leak control overridden for one call: nothing is hidden here, so the light agrees.
        let legacy = sample_baked_gi(
            &mut world,
            &json!({
                "points": [{ "p": [6.0, 11.5, 6.0] }],
                "visibility_filter": "moments",
                "leak_reduction": 0.0,
            }),
        )
        .expect("legacy sample");
        assert_eq!(legacy["lookup"]["visibility_filter"], "moments");
        assert_eq!(legacy["lookup"]["leak_reduction"], 0.0);
        assert_eq!(
            legacy["samples"][0]["irradiance_over_pi"], samples[0]["irradiance_over_pi"],
            "open moments: per-tap and legacy agree"
        );
        // Bad receivers and switches.
        for bad in [
            json!({}),
            json!({ "points": [] }),
            json!({ "points": [{ "n": [0.0, 1.0, 0.0] }] }),
            json!({ "points": [{ "p": [0.0, 0.0, 0.0], "n": [0.0, 0.0, 0.0] }] }),
            json!({ "points": [{ "p": [0.0, 0.0] }] }),
            json!({ "points": [{ "p": [0.0, 0.0, 0.0] }], "leak_guard": "yes" }),
            json!({ "points": [{ "p": [0.0, 0.0, 0.0] }], "visibility_filter": "pcf" }),
            json!({ "points": [{ "p": [0.0, 0.0, 0.0] }], "leak_reduction": 1.0 }),
        ] {
            let err = sample_baked_gi(&mut world, &bad).expect_err("rejected");
            assert_eq!(
                err.data.as_ref().map(|d| d["code"].clone()),
                Some(json!("F0002")),
                "{bad}"
            );
        }
    }

    #[test]
    fn switches_the_probe_relative_ao() {
        let mut world = World::new();
        assert!(
            set_baked_gi_ao(&mut world, &json!({ "probe_relative": false })).is_err(),
            "no renderer"
        );
        world.insert_resource(RenderSettings::default());
        world.insert_resource(RuntimeRenderer::default());
        assert!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .baked_gi_settings()
                .probe_relative_ao
        );
        let set = set_baked_gi_ao(&mut world, &json!({ "probe_relative": false })).expect("set works");
        assert_eq!(set["before"], true);
        assert_eq!(set["probe_relative"], false);
        assert_eq!(set["ran_last_frame"], false, "no frame rendered");
        assert!(
            !world
                .resource::<RuntimeRenderer>()
                .0
                .baked_gi_settings()
                .probe_relative_ao
        );
        let got = get_gi_mode(&mut world).expect("get");
        assert_eq!(got["baked"]["probe_relative_ao"], false);
        let set = set_baked_gi_ao(&mut world, &json!({ "probe_relative": true })).expect("set works");
        assert_eq!(set["before"], false);
        for bad in [json!({}), json!({ "probe_relative": "yes" })] {
            assert!(set_baked_gi_ao(&mut world, &bad).is_err(), "{bad}");
        }
    }
}
