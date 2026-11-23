// SPDX-License-Identifier: MPL-2.0
//! `render.get_valley_mist` / `render.set_valley_mist`: the terrain-hugging valley mist
//! ([`ValleyMist`], ADR-1394, book `agents/valley-mist.md`, guide topic `valley_mist`).
//!
//! The renderer adds the mist to the froxel fog when the app has a [`ValleyMist`] resource.
//! `set` creates it (from the defaults) or changes the given fields; `enabled: false` removes it.
//! `terrain: true` samples the first [`Terrain`] into the mist's height grid
//! (`regia_terrain::mist_grid_from_map`); `frame` and `fps` set the deterministic clock.

use std::sync::Arc;

use regia_ecs::prelude::*;
use regia_render::{MistQuality, ValleyMist, mist_quality};
use regia_terrain::{Terrain, mist_grid_from_map};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn mist_json(mist: Option<&ValleyMist>) -> Value {
    let Some(mist) = mist else {
        return json!({ "enabled": false });
    };
    let tier = |volumetric_fog: bool| match mist_quality(mist, volumetric_fog) {
        MistQuality::Off => "off",
        MistQuality::Reduced => "reduced",
        MistQuality::Full => "full",
    };
    json!({
        "enabled": true,
        "density": mist.density,
        "scale_height_m": mist.scale_height_m,
        "max_height_m": mist.max_height_m,
        "open_ground_fraction": mist.open_ground_fraction,
        "hollow_depth_m": mist.hollow_depth_m,
        "noise_scale_m": mist.noise_scale_m,
        "noise_strength": mist.noise_strength,
        "wind_m_per_s": mist.wind_m_per_s,
        "time_s": mist.time_s,
        "sun_steps": mist.sun_steps,
        "reduced_on_low_tiers": mist.reduced_on_low_tiers,
        "flat_ground_y": mist.flat_ground_y,
        "grid": mist.grid.as_ref().map(|grid| json!({
            "width": grid.width(),
            "depth": grid.depth(),
            "cell_m": grid.cell_m(),
            "origin": grid.origin(),
            "key": format!("{:016x}", grid.key()),
        })),
        "active": mist.is_active(),
        "tier_with_volumetric_fog": tier(true),
        "tier_without_volumetric_fog": tier(false),
    })
}

/// `render.get_valley_mist`: the mist settings, the terrain grid it holds and the tier each
/// kind of profile draws it at; `{ "enabled": false }` without a resource.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    Ok(mist_json(world.get_resource::<ValleyMist>()))
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite f32 number"))),
    }
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be true or false"))),
    }
}

fn pair(params: &Value, name: &str) -> Result<Option<[f32; 2]>, RpcError> {
    let Some(v) = params.get(name).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let bad = || RpcError::invalid_params(format!("`{name}` must be [x, z], two finite f32 numbers"));
    let items = v.as_array().filter(|a| a.len() == 2).ok_or_else(bad)?;
    let x = items[0].as_f64().filter(|f| f.is_finite()).ok_or_else(bad)?;
    let z = items[1].as_f64().filter(|f| f.is_finite()).ok_or_else(bad)?;
    let pair = [x as f32, z as f32];
    if !pair.iter().all(|v| v.is_finite()) {
        return Err(bad());
    }
    Ok(Some(pair))
}

/// `render.set_valley_mist`: changes the given fields of the mist (creating it from the defaults
/// when the app has none) and writes the [`ValleyMist`] resource. Values are clamped as the
/// renderer clamps them; wrong types answer F0002; `terrain: true` with no terrain answers F0003.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let before = world.get_resource::<ValleyMist>().cloned();
    if flag(params, "enabled")? == Some(false) {
        world.remove_resource::<ValleyMist>();
        return Ok(json!({ "before": mist_json(before.as_ref()), "after": mist_json(None) }));
    }
    let mut after = before.clone().unwrap_or_default();
    if let Some(v) = number(params, "density")? {
        after.density = v.max(0.0);
    }
    if let Some(v) = number(params, "scale_height_m")? {
        after.scale_height_m = v.max(0.1);
    }
    if let Some(v) = number(params, "max_height_m")? {
        after.max_height_m = v.max(0.1);
    }
    if let Some(v) = number(params, "open_ground_fraction")? {
        after.open_ground_fraction = v.clamp(0.0, 1.0);
    }
    if let Some(v) = number(params, "hollow_depth_m")? {
        after.hollow_depth_m = v.max(0.01);
    }
    if let Some(v) = number(params, "noise_scale_m")? {
        after.noise_scale_m = v.max(1.0);
    }
    if let Some(v) = number(params, "noise_strength")? {
        after.noise_strength = v.clamp(0.0, 1.0);
    }
    if let Some(v) = pair(params, "wind_m_per_s")? {
        after.wind_m_per_s = v;
    }
    if let Some(v) = number(params, "time_s")? {
        after.time_s = v;
    }
    if let Some(frame) = number(params, "frame")? {
        let fps = number(params, "fps")?.unwrap_or(60.0);
        if fps <= 0.0 {
            return Err(RpcError::invalid_params("`fps` must be positive"));
        }
        after.time_s = ValleyMist::time_for_frame(frame.max(0.0) as u64, fps);
    }
    if let Some(v) = number(params, "sun_steps")? {
        after.sun_steps = (v.max(0.0) as u32).min(regia_render::MIST_MAX_SUN_STEPS);
    }
    if let Some(on) = flag(params, "reduced_on_low_tiers")? {
        after.reduced_on_low_tiers = on;
    }
    if let Some(v) = number(params, "flat_ground_y")? {
        after.flat_ground_y = v;
    }
    match flag(params, "terrain")? {
        Some(true) => {
            let cell = number(params, "grid_cell_m")?.unwrap_or(32.0);
            let radius = number(params, "concavity_radius_m")?.unwrap_or(300.0);
            let mut query = world.query::<&Terrain>();
            let terrain = query.iter(world).next().ok_or_else(|| {
                RpcError::not_found(
                    "no Terrain in the world",
                    "spawn an entity with regia_terrain::Terrain, or pass terrain false for flat ground",
                )
            })?;
            let grid = mist_grid_from_map(&terrain.map, cell, radius)
                .ok_or_else(|| RpcError::invalid_params("`grid_cell_m` must be a positive finite number"))?;
            after.grid = Some(Arc::new(grid));
        }
        Some(false) => after.grid = None,
        None => {}
    }
    world.insert_resource(after.clone());
    Ok(json!({ "before": mist_json(before.as_ref()), "after": mist_json(Some(&after)) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_mist_density_is_rejected_and_leaves_the_resource() {
        let mut world = World::new();
        world.insert_resource(ValleyMist::default());
        let before = get(&mut world).expect("before");
        for name in [
            "density",
            "scale_height_m",
            "max_height_m",
            "open_ground_fraction",
            "hollow_depth_m",
            "noise_scale_m",
            "noise_strength",
            "time_s",
            "frame",
            "fps",
            "flat_ground_y",
            "sun_steps",
            "grid_cell_m",
            "concavity_radius_m",
        ] {
            // Grid controls are parsed only when terrain is requested.
            for value in [1.0e39, -1.0e39] {
                let mut params = json!({ "density": 0.5 });
                params[name] = json!(value);
                if name == "fps" {
                    params["frame"] = json!(90);
                }
                if matches!(name, "grid_cell_m" | "concavity_radius_m") {
                    params["terrain"] = json!(true);
                }
                let err = set(&mut world, &params).expect_err(name);
                assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}: {err:?}");
                assert_eq!(get(&mut world).expect("after"), before, "{params}");
            }
        }
        for wind in [[1.0e39, 0.0], [0.0, -1.0e39]] {
            assert!(set(&mut world, &json!({ "density": 0.5, "wind_m_per_s": wind })).is_err());
            assert_eq!(get(&mut world).expect("after wind"), before);
        }
        let mut empty = World::new();
        assert!(set(&mut empty, &json!({ "density": 1.0e39 })).is_err());
        assert!(!empty.contains_resource::<ValleyMist>());
    }

    #[test]
    fn frame_past_24_bits_matches_the_renderer_clock() {
        let mut world = World::new();
        for frame in [16_777_216u64, 16_777_217, 16_777_218, 33_554_433] {
            let got = set(&mut world, &json!({ "frame": frame, "fps": 60 })).expect("clock");
            let expected = ValleyMist::time_for_frame(frame, 60.0);
            assert_eq!(world.resource::<ValleyMist>().time_s, expected, "frame {frame}");
            assert_eq!(got["after"]["time_s"], json!(expected));
        }
        // The renderer clock rounds the integer before dividing, too.
        assert_eq!(
            ValleyMist::time_for_frame(16_777_217, 60.0),
            ValleyMist::time_for_frame(16_777_216, 60.0)
        );
    }

    #[test]
    fn get_without_a_resource_reports_disabled() {
        let mut world = World::new();
        assert_eq!(get(&mut world).expect("get")["enabled"], false);
    }

    #[test]
    fn set_creates_the_mist_from_defaults_and_changes_fields() {
        let mut world = World::new();
        let got = set(
            &mut world,
            &json!({ "density": 0.02, "wind_m_per_s": [1.0, -2.0], "sun_steps": 99 }),
        )
        .expect("set");
        assert_eq!(got["before"]["enabled"], false);
        let after = &got["after"];
        assert_eq!(after["enabled"], true);
        assert!((after["density"].as_f64().expect("density") - 0.02).abs() < 1.0e-6);
        assert_eq!(after["sun_steps"], regia_render::MIST_MAX_SUN_STEPS);
        assert_eq!(after["tier_with_volumetric_fog"], "full");
        assert_eq!(after["tier_without_volumetric_fog"], "reduced");
        let again = set(&mut world, &json!({ "reduced_on_low_tiers": false })).expect("set");
        assert_eq!(again["after"]["tier_without_volumetric_fog"], "off");
        assert!((again["after"]["density"].as_f64().expect("density") - 0.02).abs() < 1.0e-6);
    }

    #[test]
    fn the_frame_number_sets_the_clock() {
        let mut world = World::new();
        let got = set(&mut world, &json!({ "frame": 90, "fps": 60 })).expect("set");
        assert!((got["after"]["time_s"].as_f64().expect("time") - 1.5).abs() < 1.0e-6);
        assert!(set(&mut world, &json!({ "frame": 1, "fps": 0 })).is_err());
    }

    #[test]
    fn enabled_false_removes_the_resource() {
        let mut world = World::new();
        set(&mut world, &json!({})).expect("create");
        let got = set(&mut world, &json!({ "enabled": false })).expect("remove");
        assert_eq!(got["before"]["enabled"], true);
        assert_eq!(got["after"]["enabled"], false);
        assert!(world.get_resource::<ValleyMist>().is_none());
    }

    #[test]
    fn bad_values_answer_invalid_params() {
        let mut world = World::new();
        assert!(set(&mut world, &json!({ "density": "thick" })).is_err());
        assert!(set(&mut world, &json!({ "wind_m_per_s": [1.0] })).is_err());
        assert!(set(&mut world, &json!({ "terrain": "yes" })).is_err());
    }

    #[test]
    fn terrain_true_without_a_terrain_is_not_found() {
        let mut world = World::new();
        let err = set(&mut world, &json!({ "terrain": true })).expect_err("no terrain");
        assert_eq!(err.data.as_ref().expect("data")["code"], "F0003");
        assert!(world.get_resource::<ValleyMist>().is_none());
    }
}
