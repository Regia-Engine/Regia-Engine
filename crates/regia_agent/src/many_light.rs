// SPDX-License-Identifier: MPL-2.0
//! Stochastic many-light sampling on the agent protocol (ADR-2470,
//! `book/agents/many-light-sampling.md`):
//!
//! - `render.set_many_light_sampling { mode, view?, initial_candidates?, temporal?,
//!   spatial_taps?, spatial_radius_px?, trace_steps? }` turns the evaluation pass on (`debug`:
//!   it registers `restir_di_debug` at `after_opaque` and snapshots the world's point and spot
//!   lights into it) or off (`off`: it removes the pass). The pass draws the sampler's estimate,
//!   or the brute-force reference with `view: "reference"`, over the view: an evaluation, not
//!   lighting. `production` (ADR-2652) removes the evaluation pass and turns the profile's
//!   `many_light_sampling` on: the opaque forward pass then shades one resampled light per pixel
//!   from the frame's own lights (the settings apply to that sampler); `off` turns it off again.
//! - `render.many_light_stats { reference?, lights?, seeds?, frames? }` reads the mode, the
//!   pass's graph row and counters, and with `reference: true` runs the CPU reference on the
//!   synthetic scene: bias against the brute force in standard errors, and the error of one
//!   uniform sample, of resampling alone and of the spatiotemporal estimator.
//!
//! Error reasons (`data.reason`): `no_renderer`, `no_render_settings` (production without a
//! `RenderSettings` resource), `unknown_field`, `invalid_value`.

use regia_app::prelude::*;
use regia_render::restir_di::cpu::{self, Candidates, ClusterLists};
use regia_render::restir_di::scene::SyntheticScene;
use regia_render::restir_di::{
    MAX_CANDIDATES, MAX_SPATIAL_TAPS, ManyLight, ManyLightFeed, ManyLightMode, RESTIR_DI_PASS_NAME,
    RestirDebugView, RestirDiDebugPass, RestirDiSettings, restir_di_counters,
};
use regia_render::{LightSourceSize, PointLight, RenderSettings, RuntimeRenderer, SpotLight};
use regia_scene::GlobalTransform;
use serde_json::{Map, Value, json};
use std::sync::atomic::Ordering;

use crate::post_volumes::{check_fields, error, object};
use crate::protocol::{self, RpcError};

/// The agent's handle on the evaluation pass: its mode and the feed it reads.
#[derive(Resource, Default)]
pub(crate) struct ManyLightSampling {
    mode: ManyLightMode,
    feed: ManyLightFeed,
}

/// The fields `render.set_many_light_sampling` accepts.
pub(crate) const SET_FIELDS: &[&str] = &[
    "mode",
    "view",
    "initial_candidates",
    "temporal",
    "spatial_taps",
    "spatial_radius_px",
    "trace_steps",
];

/// The fields `render.many_light_stats` accepts.
pub(crate) const STATS_FIELDS: &[&str] = &["reference", "lights", "seeds", "frames"];

fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(
        protocol::INVALID_PARAMS,
        "F0002",
        reason,
        message,
        "see book/agents/many-light-sampling.md",
    )
}

fn no_renderer() -> RpcError {
    error(
        protocol::NOT_FOUND,
        "F0003",
        "no_renderer",
        "no RuntimeRenderer resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin",
    )
}

fn uint(map: &Map<String, Value>, key: &str, range: (u32, u32)) -> Result<Option<u32>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) if (u64::from(range.0)..=u64::from(range.1)).contains(&n) => Ok(Some(n as u32)),
            _ => Err(invalid(
                "invalid_value",
                format!("`{key}` must be an integer from {} to {}", range.0, range.1),
            )),
        },
    }
}

fn boolean(map: &Map<String, Value>, key: &str) -> Result<Option<bool>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid("invalid_value", format!("`{key}` must be true or false"))),
    }
}

fn settings_json(s: &RestirDiSettings) -> Value {
    json!({
        "initial_candidates": s.initial_candidates,
        "temporal": s.temporal,
        "history_cap": s.history_cap,
        "spatial_taps": s.spatial_taps,
        "spatial_radius_px": s.spatial_radius_px,
        "depth_tolerance": s.depth_tolerance,
        "normal_tolerance": s.normal_tolerance,
        "trace_steps": s.trace_steps,
    })
}

/// The world's point and spot lights in the frame's record layout.
fn world_lights(world: &mut World) -> Vec<ManyLight> {
    let mut out = Vec::new();
    let mut points = world.query::<(&PointLight, &GlobalTransform, Option<&LightSourceSize>)>();
    for (light, t, size) in points.iter(world) {
        let source = size.and_then(|s| s.sanitized());
        out.push(ManyLight::from_point_light(
            light,
            t.0.translation.to_array(),
            source,
        ));
    }
    let mut spots = world.query::<(&SpotLight, &GlobalTransform, Option<&LightSourceSize>)>();
    for (light, t, size) in spots.iter(world) {
        let source = size.and_then(|s| s.sanitized());
        let dir = t.0.transform_vector3(regia_core::glam::Vec3::NEG_Z).to_array();
        out.push(ManyLight::from_spot_light(
            light,
            t.0.translation.to_array(),
            dir,
            source,
        ));
    }
    out
}

/// `render.set_many_light_sampling`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, SET_FIELDS)?;
    let name = map.get("mode").and_then(Value::as_str).ok_or_else(|| {
        invalid(
            "invalid_value",
            "`mode` (\"off\", \"debug\" or \"production\") is required",
        )
    })?;
    let mode = ManyLightMode::parse(name).ok_or_else(|| {
        invalid(
            "invalid_value",
            "`mode` must be \"off\", \"debug\" or \"production\"",
        )
    })?;
    let view = match map.get("view") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(RestirDebugView::parse)
                .ok_or_else(|| invalid("invalid_value", "`view` must be \"estimate\" or \"reference\""))?,
        ),
    };
    let candidates = uint(map, "initial_candidates", (1, MAX_CANDIDATES))?;
    let temporal = boolean(map, "temporal")?;
    let taps = uint(map, "spatial_taps", (0, MAX_SPATIAL_TAPS))?;
    let radius = uint(map, "spatial_radius_px", (1, 64))?;
    let trace = uint(map, "trace_steps", (0, 64))?;
    if world.get_resource::<RuntimeRenderer>().is_none() {
        return Err(no_renderer());
    }
    world.init_resource::<ManyLightSampling>();
    let feed = world.resource::<ManyLightSampling>().feed.clone();
    let mut settings = feed.settings();
    if let Some(v) = candidates {
        settings.initial_candidates = v;
    }
    if let Some(v) = temporal {
        settings.temporal = v;
    }
    if let Some(v) = taps {
        settings.spatial_taps = v;
    }
    if let Some(v) = radius {
        settings.spatial_radius_px = v as f32;
    }
    if let Some(v) = trace {
        settings.trace_steps = v;
    }
    if settings != feed.settings() {
        feed.set_settings(settings.clone())
            .map_err(|e| invalid("invalid_value", e.to_string()))?;
    }
    if let Some(v) = view {
        feed.set_view(v);
    }
    let lights = if mode == ManyLightMode::Debug {
        let lights = world_lights(world);
        let n = lights.len();
        feed.set_lights(lights);
        n
    } else {
        0
    };
    let production = mode == ManyLightMode::Production;
    let profile_changed = match world.get_resource_mut::<RenderSettings>() {
        Some(mut render) if render.profile.many_light_sampling != production => {
            render.profile.many_light_sampling = production;
            true
        }
        Some(_) => false,
        None if production => {
            return Err(error(
                protocol::NOT_FOUND,
                "F0003",
                "no_render_settings",
                "no RenderSettings resource: the production sampler follows the render profile",
                "add regia_render::RenderPlugin",
            ));
        }
        None => false,
    };
    let Some(mut renderer) = world.get_resource_mut::<RuntimeRenderer>() else {
        return Err(no_renderer());
    };
    if renderer.0.many_light_settings() != settings {
        renderer
            .0
            .set_many_light_settings(settings.clone())
            .map_err(|e| invalid("invalid_value", e.to_string()))?;
    }
    let registered = renderer
        .0
        .registered_passes()
        .iter()
        .any(|n| n == RESTIR_DI_PASS_NAME);
    let mut changed = false;
    match mode {
        ManyLightMode::Debug if !registered => {
            renderer
                .0
                .register_pass(Box::new(RestirDiDebugPass::new(feed.clone())))
                .map_err(|e| invalid(e.code(), e.to_string()))?;
            changed = true;
        }
        ManyLightMode::Off | ManyLightMode::Production if registered => {
            renderer.0.remove_pass(RESTIR_DI_PASS_NAME);
            changed = true;
        }
        _ => {}
    }
    world.resource_mut::<ManyLightSampling>().mode = mode;
    Ok(json!({
        "mode": mode.as_str(),
        "view": feed.view().as_str(),
        "pass_changed": changed,
        "profile_changed": profile_changed,
        "lights": lights,
        "settings": settings_json(&settings),
        "note": "debug draws the sampler over the view at after_opaque (evaluation only); the \
                 lights are a snapshot: set debug again after moving them. production shades the \
                 frame's own lights in the opaque forward pass",
    }))
}

/// `render.many_light_stats`.
pub(crate) fn stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, STATS_FIELDS)?;
    let reference = boolean(map, "reference")?.unwrap_or(false);
    let light_count = uint(map, "lights", (1, 4000))?.unwrap_or(1000);
    let seeds = uint(map, "seeds", (2, 256))?.unwrap_or(32);
    let frames = uint(map, "frames", (1, 16))?.unwrap_or(6);
    let (mode, feed) = match world.get_resource::<ManyLightSampling>() {
        Some(s) => (s.mode, Some(s.feed.clone())),
        None => (ManyLightMode::Off, None),
    };
    let pass = world.get_resource::<RuntimeRenderer>().map(|r| {
        let report = r.0.graph_report();
        match report.extensions.iter().find(|p| p.name == RESTIR_DI_PASS_NAME) {
            Some(info) => json!({
                "registered": true,
                "runs": info.runs,
                "failures": info.failures,
                "last_frame": info.last_frame,
                "last_error": info.last_error,
                "last_cpu_ms": info.last_cpu_ms,
            }),
            None => json!({ "registered": false }),
        }
    });
    let production = world.get_resource::<RuntimeRenderer>().map(|r| {
        let p = r.0.many_light_report();
        json!({
            "profile": world
                .get_resource::<RenderSettings>()
                .is_some_and(|s| s.profile.many_light_sampling),
            "active": p.active,
            "skipped": if p.active { Value::Null } else { json!(p.skipped.as_str()) },
            "frames": p.frames,
            "resets": p.resets,
            "resource_builds": p.resource_builds,
            "bind_group_builds": p.bind_group_builds,
            "lights": p.lights,
            "width": p.size.0,
            "height": p.size.1,
            "last_error": p.last_error,
        })
    });
    let c = restir_di_counters();
    let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let settings = feed
        .as_ref()
        .map_or_else(RestirDiSettings::default, ManyLightFeed::settings);
    let reference_json = if reference {
        // The synthetic scene at 48 × 27 with 256-slot cluster lists (`restir_di::scene`).
        let scene = SyntheticScene::new(48, 27, light_count, 7);
        let lists = ClusterLists::build(&scene.camera, 48.0 / 27.0, &scene.lights, 256);
        let r = cpu::evaluate(
            &scene.gbuffer,
            &scene.camera,
            &scene.lights,
            Candidates::Clusters(&lists),
            &settings,
            frames,
            seeds,
        )
        .map_err(|e| invalid("invalid_value", e.to_string()))?;
        json!({
            "scene": { "width": 48, "height": 27, "lights": r.lights, "surface_pixels": r.surface_pixels,
                       "cluster_mean": r.cluster_mean, "cluster_max": r.cluster_max,
                       "cluster_overflowed": r.cluster_overflowed },
            "seeds": r.seeds,
            "frames": r.frames,
            "truth_mean": r.truth_mean,
            "estimate_mean": r.estimate_mean,
            "relative_bias": r.relative_bias,
            "bias_sigma": r.bias_sigma,
            "unbiased": r.bias_sigma < 4.0,
            "rmse_uniform": r.rmse_uniform,
            "rmse_candidates": r.rmse_candidates,
            "rmse_restir": r.rmse_restir,
            "variance_reduction": r.variance_reduction,
            "convergence": r.convergence,
        })
    } else {
        Value::Null
    };
    Ok(json!({
        "mode": mode.as_str(),
        "view": feed.as_ref().map_or("estimate", |f| f.view().as_str()),
        "lights_fed": feed.as_ref().map_or(0, ManyLightFeed::light_count),
        "settings": settings_json(&settings),
        "pass": pass,
        "production": production,
        "counters": {
            "frames_recorded": load(&c.frames_recorded),
            "frames_skipped": load(&c.frames_skipped),
            "light_uploads": load(&c.light_uploads),
            "cluster_builds": load(&c.cluster_builds),
            "resource_builds": load(&c.resource_builds),
            "last_lights": load(&c.last_lights),
            "last_width": load(&c.last_width),
            "last_height": load(&c.last_height),
            "last_cluster_max": load(&c.last_cluster_max),
            "last_cluster_overflowed": load(&c.last_cluster_overflowed),
        },
        "reference": reference_json,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_scene::Transform;

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(RuntimeRenderer::default());
        world.spawn((
            PointLight {
                position: [0.0; 3],
                color: [1.0; 3],
                intensity: 800.0,
                radius: 5.0,
                cast_shadows: false,
            },
            GlobalTransform(
                Transform {
                    translation: regia_core::glam::Vec3::new(1.0, 2.0, 3.0),
                    ..Transform::default()
                }
                .compute_affine(),
            ),
        ));
        world.spawn((
            SpotLight {
                position: [0.0; 3],
                direction: [0.0, -1.0, 0.0],
                color: [1.0, 0.5, 0.2],
                intensity: 200.0,
                outer_angle: 0.6,
                inner_angle: 0.3,
                range: 8.0,
                cast_shadows: false,
            },
            GlobalTransform(Transform::default().compute_affine()),
        ));
        world
    }

    fn registered(world: &World) -> bool {
        world
            .resource::<RuntimeRenderer>()
            .0
            .registered_passes()
            .iter()
            .any(|n| n == RESTIR_DI_PASS_NAME)
    }

    #[test]
    fn debug_registers_the_pass_with_the_world_lights_and_off_removes_it() -> Result<(), RpcError> {
        let mut world = world();
        let before = stats(&mut world, &json!({}))?;
        assert_eq!(before["mode"], "off");
        assert_eq!(before["pass"]["registered"], false);
        let on = set(
            &mut world,
            &json!({ "mode": "debug", "view": "reference", "spatial_taps": 2 }),
        )?;
        assert_eq!(on["pass_changed"], true);
        assert_eq!(on["lights"], 2);
        assert_eq!(on["view"], "reference");
        assert_eq!(on["settings"]["spatial_taps"], 2);
        assert!(registered(&world));
        let again = set(&mut world, &json!({ "mode": "debug" }))?;
        assert_eq!(again["pass_changed"], false, "one pass only");
        let read = stats(&mut world, &json!({}))?;
        assert_eq!(read["mode"], "debug");
        assert_eq!(read["lights_fed"], 2);
        assert_eq!(read["pass"]["registered"], true);
        assert!(read["reference"].is_null());
        let off = set(&mut world, &json!({ "mode": "off" }))?;
        assert_eq!(off["pass_changed"], true);
        assert!(!registered(&world));
        Ok(())
    }

    #[test]
    fn production_turns_the_profile_on_and_removes_the_evaluation_pass() -> Result<(), RpcError> {
        let mut no_settings = world();
        let err = set(&mut no_settings, &json!({ "mode": "production" })).expect_err("no settings");
        assert_eq!(reason(&err), "no_render_settings");
        let mut world = world();
        world.insert_resource(RenderSettings::default());
        set(&mut world, &json!({ "mode": "debug" }))?;
        assert!(registered(&world));
        let on = set(&mut world, &json!({ "mode": "production", "spatial_taps": 2 }))?;
        assert_eq!(on["mode"], "production");
        assert_eq!(on["profile_changed"], true);
        assert_eq!(on["pass_changed"], true, "the evaluation pass leaves");
        assert!(!registered(&world));
        assert!(world.resource::<RenderSettings>().profile.many_light_sampling);
        assert_eq!(
            world
                .resource::<RuntimeRenderer>()
                .0
                .many_light_settings()
                .spatial_taps,
            2
        );
        let read = stats(&mut world, &json!({}))?;
        assert_eq!(read["mode"], "production");
        assert_eq!(read["production"]["profile"], true);
        assert_eq!(read["production"]["active"], false, "no frame yet");
        assert_eq!(read["production"]["frames"], 0);
        let again = set(&mut world, &json!({ "mode": "production" }))?;
        assert_eq!(again["profile_changed"], false);
        let off = set(&mut world, &json!({ "mode": "off" }))?;
        assert_eq!(off["profile_changed"], true);
        assert!(!world.resource::<RenderSettings>().profile.many_light_sampling);
        Ok(())
    }

    #[test]
    fn the_reference_run_reports_an_unbiased_estimator() -> Result<(), RpcError> {
        let mut world = world();
        let r = stats(
            &mut world,
            &json!({ "reference": true, "lights": 300, "seeds": 16, "frames": 3 }),
        )?;
        let rep = &r["reference"];
        assert_eq!(rep["scene"]["lights"], 300);
        assert_eq!(rep["seeds"], 16);
        assert_eq!(rep["convergence"].as_array().map(Vec::len), Some(3));
        assert!(rep["truth_mean"].as_f64().unwrap_or(0.0) > 0.0);
        assert!(
            rep["rmse_restir"].as_f64().unwrap_or(f64::MAX) < rep["rmse_uniform"].as_f64().unwrap_or(0.0)
        );
        Ok(())
    }

    #[test]
    fn errors_carry_reasons() {
        let mut world = world();
        let cases = [
            (json!({ "mode": "on" }), "invalid_value"),
            (json!({}), "invalid_value"),
            (json!({ "mode": "debug", "view": "split" }), "invalid_value"),
            (
                json!({ "mode": "debug", "initial_candidates": 0 }),
                "invalid_value",
            ),
            (json!({ "mode": "debug", "spatial_taps": 9 }), "invalid_value"),
            (json!({ "mode": "debug", "temporal": "yes" }), "invalid_value"),
            (json!({ "mode": "debug", "bogus": 1 }), "unknown_field"),
        ];
        for (params, expected) in cases {
            let err = set(&mut world, &params).expect_err("refused");
            assert_eq!(reason(&err), expected, "{params}");
        }
        assert!(!registered(&world), "nothing applied");
        let err = stats(&mut world, &json!({ "seeds": 1 })).expect_err("seeds");
        assert_eq!(reason(&err), "invalid_value");
        let mut bare = World::new();
        let err = set(&mut bare, &json!({ "mode": "debug" })).expect_err("no renderer");
        assert_eq!(reason(&err), "no_renderer");
    }
}
