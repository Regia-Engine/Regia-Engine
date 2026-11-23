// SPDX-License-Identifier: MPL-2.0
//! Several baked GI volumes and their seams (ADR-1097, `book/agents/baked-gi-seams.md`):
//!
//! - `level.get_baked_gi_stream`: the streamed levels' volumes (`regia_framework::baked_gi_stream`),
//!   their fade weights and errors, the stream settings and the renderer's blend.
//! - `level.set_baked_gi_stream`: the fade time, blend distance and priority of streamed volumes.
//! - `level.sample_baked_gi`: the blended baked light at points (a line across a seam), and the
//!   largest change between neighboring points, so a seam can be checked without a screenshot.

use regia_app::prelude::*;
use regia_framework::baked_gi_stream::{BakedGiStreamSettings, BakedGiStreamState};
use regia_render::RuntimeRenderer;
use regia_render::baked_gi::MAX_BAKED_GI_VOLUMES;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most points one `level.sample_baked_gi` call takes.
const MAX_POINTS: usize = 4096;

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource: this app has no runtime renderer",
        "add regia_render::RenderPlugin (DefaultPlugins has it)",
    )
}

fn settings_json(s: &BakedGiStreamSettings) -> Value {
    json!({
        "fade_seconds": s.fade_seconds,
        "blend_distance": s.blend_distance,
        "priority": s.priority,
    })
}

/// `level.get_baked_gi_stream`: every streamed volume, the settings and the renderer's blend.
pub(crate) fn get_stream(world: &mut World) -> Result<Value, RpcError> {
    let state = world.get_resource::<BakedGiStreamState>();
    let tiles: Vec<Value> = state.map_or_else(Vec::new, |s| {
        s.tiles
            .iter()
            .map(|t| {
                json!({
                    "level": t.level.0.to_string(),
                    "volume": t.volume,
                    "weight": t.weight,
                    "loaded": t.loaded,
                    "installed": t.installed(),
                    "probes": t.probes,
                    "error": t.error,
                    "error_code": t.error_code,
                    "failed_parts": t
                        .failed_parts
                        .iter()
                        .map(|f| json!({ "index": f.index, "path": f.path, "code": f.code, "error": f.error }))
                        .collect::<Vec<Value>>(),
                })
            })
            .collect()
    });
    let settings = world.get_resource::<BakedGiStreamSettings>().copied();
    let renderer = world.get_resource::<RuntimeRenderer>().map(|r| {
        let s = r.0.baked_gi_status();
        let layers: Vec<Value> =
            r.0.baked_gi_layers()
                .iter()
                .map(|l| {
                    json!({
                        "probes": l.volume.probe_count(),
                        "priority": l.priority,
                        "blend_distance": l.blend_distance,
                        "weight": l.weight,
                    })
                })
                .collect();
        json!({
            "installed": r.0.baked_gi().is_some(),
            "layers": layers,
            "volumes": s.volumes,
            "volumes_dropped": s.volumes_dropped,
            "max_volumes": MAX_BAKED_GI_VOLUMES,
            "active": s.active,
        })
    });
    Ok(json!({
        "plugin": state.is_some(),
        "tiles": tiles,
        "settings": settings.as_ref().map(settings_json),
        "renderer": renderer,
    }))
}

fn finite(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    let Some(v) = params.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let x = v.as_f64().filter(|x| x.is_finite() && *x >= 0.0).ok_or_else(|| {
        RpcError::invalid_params(format!("`{key}` must be a number ≥ 0 (seconds or meters)"))
    })?;
    Ok(Some(x as f32))
}

/// `level.set_baked_gi_stream`: changes the stream settings; unset fields keep theirs.
pub(crate) fn set_stream(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let fade = finite(params, "fade_seconds")?;
    let blend = finite(params, "blend_distance")?;
    let priority = match params.get("priority").filter(|v| !v.is_null()) {
        None => None,
        Some(v) => Some(
            v.as_i64()
                .and_then(|p| i32::try_from(p).ok())
                .ok_or_else(|| RpcError::invalid_params("`priority` must be an integer"))?,
        ),
    };
    let before = world
        .get_resource::<BakedGiStreamSettings>()
        .copied()
        .ok_or_else(|| {
            RpcError::not_found(
                "no BakedGiStreamSettings resource: the baked GI level plugin is not installed",
                "add regia_framework::baked_gi_level::BakedGiLevelPlugin (DefaultPlugins has it)",
            )
        })?;
    let after = BakedGiStreamSettings {
        fade_seconds: fade.unwrap_or(before.fade_seconds),
        blend_distance: blend.unwrap_or(before.blend_distance),
        priority: priority.unwrap_or(before.priority),
    };
    world.insert_resource(after);
    Ok(json!({ "before": settings_json(&before), "after": settings_json(&after) }))
}

fn vec3(v: &Value, what: &str) -> Result<[f32; 3], RpcError> {
    let bad = || RpcError::invalid_params(format!("{what} must be an array of 3 finite numbers"));
    let a = v.as_array().filter(|a| a.len() == 3).ok_or_else(bad)?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64().filter(|x| x.is_finite()).ok_or_else(bad)? as f32;
    }
    Ok(out)
}

fn unit(v: [f32; 3], what: &str) -> Result<[f32; 3], RpcError> {
    let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    if l <= 1e-6 {
        return Err(RpcError::invalid_params(format!("{what} must not be zero")));
    }
    Ok([v[0] / l, v[1] / l, v[2] / l])
}

/// `level.sample_baked_gi`: the blended baked light at each point, and the largest change
/// between neighboring points.
pub(crate) fn sample(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points = crate::params::opt_array(params, "points")?
        .filter(|a| !a.is_empty() && a.len() <= MAX_POINTS)
        .ok_or_else(|| {
            RpcError::invalid_params(format!(
                "`points` must be an array of 1..={MAX_POINTS} world points [x, y, z]"
            ))
        })?;
    let points: Vec<[f32; 3]> = points
        .iter()
        .enumerate()
        .map(|(i, p)| vec3(p, &format!("points[{i}]")))
        .collect::<Result<_, _>>()?;
    let normal = match params.get("normal").filter(|v| !v.is_null()) {
        Some(v) => unit(vec3(v, "`normal`")?, "`normal`")?,
        None => [0.0, 1.0, 0.0],
    };
    let view = match params.get("view").filter(|v| !v.is_null()) {
        Some(v) => unit(vec3(v, "`view`")?, "`view`")?,
        None => normal,
    };
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(no_renderer)?;
    let mut samples = Vec::with_capacity(points.len());
    let mut prev: Option<([f32; 3], f32)> = None;
    let (mut step_e, mut step_c) = (0.0f32, 0.0f32);
    for p in &points {
        let s = renderer.0.sample_baked_gi(*p, normal, view);
        if let Some((e, c)) = prev {
            for (a, b) in e.iter().zip(&s.irradiance_over_pi) {
                step_e = step_e.max((a - b).abs());
            }
            step_c = step_c.max((c - s.coverage).abs());
        }
        prev = Some((s.irradiance_over_pi, s.coverage));
        samples.push(json!({
            "p": p,
            "irradiance_over_pi": s.irradiance_over_pi,
            "coverage": s.coverage,
        }));
    }
    let status = renderer.0.baked_gi_status();
    Ok(json!({
        "samples": samples,
        "max_step": { "irradiance_over_pi": step_e, "coverage": step_c },
        "volumes": usize::from(renderer.0.baked_gi().is_some()) + renderer.0.baked_gi_layers().len(),
        "installed": status.installed,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, BakedGiVolume, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, TerrainLayer,
        VolumeLayout,
    };
    use regia_render::baked_gi::BakedGiLayer;

    /// A 4 × 4-column terrain volume from `x0` (4 m columns, levels 1 and 4 m) lit `sky`.
    fn tile(x0: f32, sky: f32) -> Arc<BakedGiVolume> {
        let layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [x0, 0.0],
                spacing: [4.0, 4.0],
                counts: [4, 4],
                levels: vec![1.0, 4.0],
                ground: vec![0.0; 16],
            }),
            bricks: Vec::new(),
            moment_resolution: 4,
            max_distance: 16.0,
        };
        let k = 4.0 * std::f32::consts::PI * SH_Y00;
        let lit = [[sky * k, 0.0, 0.0, 0.0]; 3];
        let dark = [[0.0f32; 4]; 3];
        let probes: Vec<ProbeData> = (0..layout.probe_count())
            .map(|_| ProbeData {
                sh: [dark, lit, dark, dark],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: ProbeFlags::VALID,
                },
            })
            .collect();
        Arc::new(
            BakedGiVolume::from_probes(
                &layout,
                BakeLights {
                    sky_scale: 1.0,
                    ..BakeLights::default()
                },
                BakeInfo::default(),
                &probes,
            )
            .expect("volume"),
        )
    }

    fn world_with_two_tiles() -> World {
        let mut world = World::new();
        let mut renderer = RuntimeRenderer::default();
        renderer.0.set_baked_gi(Some(tile(0.0, 1.0)));
        renderer
            .0
            .set_baked_gi_layers(&[BakedGiLayer::new(tile(16.0, 3.0))]);
        world.insert_resource(renderer);
        world
    }

    #[test]
    fn a_line_across_the_seam_reports_small_steps_and_full_coverage() {
        let mut world = world_with_two_tiles();
        let points: Vec<Value> = (0..=56).map(|i| json!([i as f32 * 0.5, 1.5, 6.0])).collect();
        let got = sample(&mut world, &json!({ "points": points })).expect("samples");
        let samples = got["samples"].as_array().expect("samples");
        assert_eq!(samples.len(), 57);
        assert_eq!(got["volumes"], 2);
        for s in samples {
            let c = s["coverage"].as_f64().expect("coverage");
            assert!((c - 1.0).abs() < 1e-4, "{s}");
        }
        let first = samples[0]["irradiance_over_pi"][0].as_f64().expect("e");
        let last = samples[56]["irradiance_over_pi"][0].as_f64().expect("e");
        assert!(
            (first - 1.0).abs() < 0.02 && (last - 3.0).abs() < 0.05,
            "{first} {last}"
        );
        // 2 over the 4 m seam cell, sampled every 0.5 m: 0.25 per step.
        let step = got["max_step"]["irradiance_over_pi"].as_f64().expect("step");
        assert!(step < 0.26, "{step}");
        assert!(got["max_step"]["coverage"].as_f64().expect("coverage step") < 1e-4);
    }

    #[test]
    fn bad_points_and_normals_are_refused() {
        let mut world = world_with_two_tiles();
        for params in [
            json!({}),
            json!({ "points": [] }),
            json!({ "points": [[0.0, 1.0]] }),
            json!({ "points": [[0.0, 1.0, "x"]] }),
            json!({ "points": [[0.0, 1.0, 2.0]], "normal": [0.0, 0.0, 0.0] }),
        ] {
            assert!(sample(&mut world, &params).is_err(), "{params}");
        }
        let mut bare = World::new();
        assert!(sample(&mut bare, &json!({ "points": [[0.0, 0.0, 0.0]] })).is_err());
    }

    #[test]
    fn the_stream_settings_change_field_by_field() {
        let mut world = World::new();
        assert!(
            set_stream(&mut world, &json!({ "fade_seconds": 2.0 })).is_err(),
            "no plugin"
        );
        world.insert_resource(BakedGiStreamSettings::default());
        let got = set_stream(&mut world, &json!({ "fade_seconds": 2.5, "priority": -1 })).expect("set");
        assert_eq!(got["after"]["fade_seconds"], 2.5);
        assert_eq!(got["after"]["priority"], -1);
        assert_eq!(got["after"]["blend_distance"], 0.0);
        assert!(set_stream(&mut world, &json!({ "blend_distance": -1.0 })).is_err());
        assert!(set_stream(&mut world, &json!({ "priority": 1.5 })).is_err());
        let s = *world.resource::<BakedGiStreamSettings>();
        assert_eq!((s.fade_seconds, s.priority), (2.5, -1));
        world.insert_resource(BakedGiStreamState::default());
        let got = get_stream(&mut world).expect("state");
        assert_eq!(got["plugin"], true);
        assert_eq!(got["settings"]["fade_seconds"], 2.5);
        assert!(got["tiles"].as_array().is_some_and(Vec::is_empty));
        assert!(got["renderer"].is_null());
    }
}
