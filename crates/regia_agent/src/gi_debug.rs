// SPDX-License-Identifier: MPL-2.0
//! Debugging views of the baked GI volume on the agent protocol (`book/agents/gi-debug-views.md`,
//! ADR-1109). Three methods, all CPU queries on the installed volume through the same code as
//! the lookup mirror (`regia_render::baked_gi::trace_static_gi`):
//!
//! - `render.get_gi_probes`: the probes near a point with their validity flags, position and the
//!   light they hold, plus the volume's counts by flag.
//! - `render.explain_gi`: how one lookup at a point was weighted, corner by corner.
//! - `render.set_gi_debug_overlay`: draws probe validity, probe irradiance or one lookup's corner
//!   weights as gizmos in the world, so a screenshot shows them.

use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::baked_gi::{BakedGiVolume, GI_COMPONENTS, GiComponent};
use regia_core::math::{Color, Vec3};
use regia_render::RuntimeRenderer;
use regia_render::baked_gi::{
    BakedGiSettings, CornerTrace, GiLookupTrace, ProbeDebugPoint, ProbeFilter, ProbeQuery, SunRelight,
    probe_debug_points, trace_static_gi, volume_bounds, volume_summary,
};
use regia_render::gizmos::{Gizmo, GizmoAsset};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Probes `render.get_gi_probes` returns at most.
const MAX_LISTED: usize = 2000;
/// Probes the overlay draws at most.
const MAX_DRAWN: usize = 5000;
/// Default `max` of `render.get_gi_probes`.
const DEFAULT_LISTED: usize = 100;
/// Default `max` of the overlay.
const DEFAULT_DRAWN: usize = 400;
/// Default sphere radius of the overlay, metres.
const DEFAULT_SIZE: f32 = 0.15;

/// The gizmo entity `render.set_gi_debug_overlay` draws with.
#[derive(Component)]
struct GiDebugGizmo;

/// The installed volume with the settings and component factors the lookup runs with.
struct Installed {
    volume: Arc<BakedGiVolume>,
    settings: BakedGiSettings,
    scales: [[f32; 3]; GI_COMPONENTS],
    /// The last frame's sun relight (ADR-861) when a baked frame lit and the renderer relights;
    /// else none, the sun as baked.
    relight: SunRelight,
    /// The factors are the last frame's (a baked frame lit); else 1, the level of the bake.
    from_frame: bool,
}

fn installed(world: &World) -> Result<Installed, RpcError> {
    let renderer = world.get_resource::<RuntimeRenderer>().ok_or_else(|| {
        RpcError::not_found(
            "no RuntimeRenderer resource: this app has no runtime renderer",
            "add regia_render::RenderPlugin",
        )
    })?;
    let volume = renderer.0.baked_gi().cloned().ok_or_else(|| {
        RpcError::not_found(
            "no baked GI volume is installed",
            "install one with FrameRenderer::set_baked_gi, or open a level whose look names a .fgiv \
             (book/agents/baked-gi-maps.md); render.get_gi_mode reports baked.installed",
        )
    })?;
    let status = renderer.0.baked_gi_status();
    let settings = renderer.0.baked_gi_settings();
    Ok(Installed {
        volume,
        relight: if settings.sun_relight && status.active {
            status.sun_relight
        } else {
            SunRelight::NONE
        },
        settings,
        scales: if status.active {
            status.scales
        } else {
            [[1.0; 3]; GI_COMPONENTS]
        },
        from_frame: status.active,
    })
}

fn bad(what: impl Into<String>) -> RpcError {
    RpcError::invalid_params(what)
}

fn vec3(params: &Value, key: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(raw) = params.get(key) else {
        return Ok(None);
    };
    let wrong = || bad(format!("`{key}` must be an array of 3 finite numbers"));
    let xyz = raw.as_array().filter(|a| a.len() == 3).ok_or_else(wrong)?;
    let mut out = [0.0f32; 3];
    for (o, x) in out.iter_mut().zip(xyz) {
        let v = x.as_f64().filter(|v| v.is_finite()).ok_or_else(wrong)?;
        *o = v as f32;
        if !o.is_finite() {
            return Err(wrong());
        }
    }
    Ok(Some(out))
}

fn unit(v: [f32; 3], key: &str) -> Result<[f32; 3], RpcError> {
    // f64 intermediates keep the length finite for every finite f32 direction.
    let len = v
        .iter()
        .map(|&x| {
            let x = f64::from(x);
            x * x
        })
        .sum::<f64>()
        .sqrt();
    if !len.is_finite() || len < 1e-6 {
        return Err(bad(format!("`{key}` must be a finite, nonzero vector")));
    }
    let out = v.map(|x| (f64::from(x) / len) as f32);
    if !out.iter().all(|x| x.is_finite()) {
        return Err(bad(format!("`{key}` must normalize to a finite vector")));
    }
    Ok(out)
}

fn positive(params: &Value, key: &str) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None => Ok(None),
        Some(v) => match v.as_f64() {
            Some(x) if (x as f32).is_finite() && (x as f32) > 0.0 => Ok(Some(x as f32)),
            _ => Err(bad(format!("`{key}` must be a number > 0"))),
        },
    }
}

fn count(params: &Value, key: &str, default: usize, cap: usize) -> Result<usize, RpcError> {
    match params.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|n| (n as usize).min(cap))
            .ok_or_else(|| bad(format!("`{key}` must be a whole number >= 0"))),
    }
}

fn filter(params: &Value) -> Result<ProbeFilter, RpcError> {
    match params.get("filter") {
        None => Ok(ProbeFilter::All),
        Some(v) => v
            .as_str()
            .and_then(ProbeFilter::parse)
            .ok_or_else(|| bad("`filter` must be \"all\", \"valid\", \"invalid\" or \"relocated\"")),
    }
}

/// The query the params describe. `center` defaults to the middle of the volume, `radius` to the
/// whole volume.
fn probe_query(
    inst: &Installed,
    params: &Value,
    default_max: usize,
    cap: usize,
) -> Result<ProbeQuery, RpcError> {
    let center = match vec3(params, "center")? {
        Some(c) => c,
        None => volume_bounds(&inst.volume).map_or([0.0; 3], |(lo, hi)| {
            std::array::from_fn(|a| 0.5 * (lo[a] + hi[a]))
        }),
    };
    Ok(ProbeQuery {
        center,
        radius: positive(params, "radius")?.unwrap_or(f32::INFINITY),
        max: count(params, "max", default_max, cap)?,
        normal: unit(vec3(params, "normal")?.unwrap_or([0.0, 1.0, 0.0]), "normal")?,
        filter: filter(params)?,
    })
}

fn lum(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

fn probe_json(p: &ProbeDebugPoint) -> Value {
    json!({
        "index": p.index,
        "layer": p.layer.name(),
        "brick": p.brick,
        "interior": p.interior,
        "position": p.position,
        "valid": p.valid,
        "relocated": p.relocated,
        "under_terrain": p.under_terrain,
        "inside_geometry": p.inside_geometry,
        "flags": p.flags,
        "irradiance": p.irradiance_over_pi,
        "luminance": lum(p.irradiance_over_pi),
        "mean_distance": p.mean_distance,
    })
}

fn scales_json(inst: &Installed) -> Value {
    json!({
        "source": if inst.from_frame { "frame" } else { "as_baked" },
        "factors": inst.scales,
    })
}

/// `render.get_gi_probes`: the probes of the installed baked GI volume within `radius` of
/// `center` (default: the whole volume around its middle), nearest first, at most `max`, with
/// their validity flags and light, and the volume's counts by flag.
pub(crate) fn get_gi_probes(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let inst = installed(world)?;
    let query = probe_query(&inst, params, DEFAULT_LISTED, MAX_LISTED)?;
    let (points, matched) = probe_debug_points(&inst.volume, &inst.settings, &inst.scales, &query);
    let s = volume_summary(&inst.volume);
    Ok(json!({
        "center": query.center,
        "radius": query.radius.is_finite().then_some(query.radius),
        "filter": crate::params::opt_str(params, "filter")?.unwrap_or("all"),
        "matched": matched,
        "returned": points.len(),
        "summary": {
            "total": s.total,
            "valid": s.valid,
            "invalid": s.invalid,
            "relocated": s.relocated,
            "under_terrain": s.under_terrain,
            "inside_geometry": s.inside_geometry,
            "terrain_probes": s.terrain_probes,
            "bricks": s.bricks,
            "interior_bricks": s.interior_bricks,
        },
        "bounds": volume_bounds(&inst.volume).map(|(lo, hi)| json!({ "min": lo, "max": hi })),
        "scales": scales_json(&inst),
        "probes": points.iter().map(probe_json).collect::<Vec<_>>(),
    }))
}

fn corner_json(c: &CornerTrace, share: f32) -> Value {
    json!({
        "index": c.index,
        "position": c.position,
        "valid": c.valid,
        "walled": c.walled,
        "trilinear": c.trilinear,
        "visibility": c.visibility,
        "weight": c.weight,
        "share": share,
    })
}

fn layer_json(corners: &[CornerTrace], sum: f32) -> Vec<Value> {
    let shares = GiLookupTrace::normalized(corners, sum);
    corners
        .iter()
        .zip(shares)
        .map(|(c, s)| corner_json(c, s))
        .collect()
}

/// Short codes for what is wrong with a lookup, empty when it is healthy.
fn findings(t: &GiLookupTrace) -> Vec<&'static str> {
    let mut out = Vec::new();
    if t.sample.coverage <= 0.0 {
        out.push(if t.brick_corners.is_empty() && t.terrain_corners.is_empty() {
            "outside_volume"
        } else {
            "all_corners_rejected"
        });
    }
    let rejected = |cs: &[CornerTrace]| cs.iter().any(|c| !c.valid);
    if rejected(&t.brick_corners) || rejected(&t.terrain_corners) {
        out.push("rejected_corners");
    }
    let walled = |cs: &[CornerTrace]| cs.iter().any(|c| c.walled);
    if walled(&t.brick_corners) || walled(&t.terrain_corners) {
        out.push("walled_corners");
    }
    if t.interior {
        out.push("interior_brick");
    }
    if t.sample.coverage > 0.0 && t.sample.coverage < 1.0 {
        out.push("partial_coverage");
    }
    out
}

/// `render.explain_gi`: how the baked lookup at `p` (surface normal `n`, default up; direction to
/// the viewer `v`, default `n`) was put together: the layer shares and, per corner probe, its
/// validity, trilinear weight and final weight.
pub(crate) fn explain_gi(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let inst = installed(world)?;
    let p =
        vec3(params, "p")?.ok_or_else(|| bad("`p` is required: the world point to look up, [x, y, z]"))?;
    let n = unit(vec3(params, "n")?.unwrap_or([0.0, 1.0, 0.0]), "n")?;
    let v = unit(vec3(params, "v")?.unwrap_or(n), "v")?;
    let t = trace_static_gi(&inst.volume, &inst.settings, &inst.scales, &inst.relight, p, n, v);
    let components: serde_json::Map<String, Value> = GiComponent::ALL
        .iter()
        .zip(t.sample.components)
        .map(|(c, e)| (c.name().to_string(), json!(e)))
        .collect();
    Ok(json!({
        "p": p,
        "n": n,
        "v": v,
        "irradiance": t.sample.irradiance_over_pi,
        "components": components,
        "coverage": t.sample.coverage,
        "brick": t.brick,
        "interior": t.interior,
        "brick_coverage": t.brick_coverage,
        "terrain_coverage": t.terrain_coverage,
        "brick_visibility": t.brick_visibility,
        "terrain_visibility": t.terrain_visibility,
        "brick_corners": layer_json(&t.brick_corners, t.brick_weight_sum),
        "terrain_corners": layer_json(&t.terrain_corners, t.terrain_weight_sum),
        "findings": findings(&t),
        "scales": scales_json(&inst),
        "sun_relight": inst.relight.ground_factor(),
    }))
}

/// Blue (low) to white (high) for `t` in 0..=1.
fn heat(t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::rgb(0.15 + 0.85 * t, 0.3 + 0.7 * t, 1.0)
}

fn validity_color(p: &ProbeDebugPoint) -> Color {
    if p.inside_geometry {
        Color::rgb(1.0, 0.15, 0.15)
    } else if p.under_terrain {
        Color::rgb(0.9, 0.2, 0.9)
    } else if !p.valid {
        Color::rgb(1.0, 0.5, 0.1)
    } else if p.relocated {
        Color::rgb(1.0, 0.85, 0.1)
    } else {
        Color::rgb(0.2, 0.9, 0.3)
    }
}

/// One sphere per probe: green kept, yellow relocated, red inside geometry, magenta under the
/// terrain, orange rejected for another reason.
fn validity_asset(points: &[ProbeDebugPoint], size: f32) -> GizmoAsset {
    let mut asset = GizmoAsset::new();
    for p in points {
        asset.sphere(Vec3::from_array(p.position), size, validity_color(p));
    }
    asset
}

/// One sphere per probe in the light it holds, divided by `peak` (times `exposure`); rejected
/// probes are dark grey. Returns the asset and the peak channel it normalized by.
fn irradiance_asset(points: &[ProbeDebugPoint], size: f32, exposure: f32) -> (GizmoAsset, f32) {
    let peak = points
        .iter()
        .filter(|p| p.valid)
        .flat_map(|p| p.irradiance_over_pi)
        .fold(0.0f32, f32::max);
    let k = if peak > 0.0 { exposure / peak } else { 0.0 };
    let mut asset = GizmoAsset::new();
    for p in points {
        let color = if p.valid {
            let c = p.irradiance_over_pi.map(|x| (x * k).clamp(0.0, 1.0));
            Color::rgb(c[0], c[1], c[2])
        } else {
            Color::rgb(0.15, 0.15, 0.15)
        };
        asset.sphere(Vec3::from_array(p.position), size, color);
    }
    (asset, peak)
}

/// A line from the lookup point to each corner, brighter and thicker-ended with its share of the
/// weight; a rejected corner is red.
fn weights_asset(trace: &GiLookupTrace, p: [f32; 3], size: f32) -> GizmoAsset {
    let mut asset = GizmoAsset::new();
    let at = Vec3::from_array(p);
    asset.sphere(at, size * 0.5, Color::rgb(1.0, 1.0, 1.0));
    for (corners, sum) in [
        (&trace.brick_corners, trace.brick_weight_sum),
        (&trace.terrain_corners, trace.terrain_weight_sum),
    ] {
        let shares = GiLookupTrace::normalized(corners, sum);
        for (c, share) in corners.iter().zip(shares) {
            let to = Vec3::from_array(c.position);
            if c.walled {
                let orange = Color::rgb(1.0, 0.55, 0.1);
                asset.line(at, to, orange);
                asset.sphere(to, size * 0.5, orange);
            } else if c.valid {
                let color = heat(share.sqrt());
                asset.line(at, to, color);
                asset.sphere(to, size * (0.5 + 1.5 * share), color);
            } else {
                let red = Color::rgb(1.0, 0.1, 0.1);
                asset.line(at, to, red);
                asset.sphere(to, size * 0.5, red);
            }
        }
    }
    asset
}

/// `render.set_gi_debug_overlay`: draws a debug view of the baked volume as a gizmo; a later call
/// replaces it. `mode` is `validity`, `irradiance`, `weights` or `off`.
pub(crate) fn set_gi_debug_overlay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = params
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("`mode` is required: \"validity\", \"irradiance\", \"weights\" or \"off\""))?;
    if !matches!(mode, "validity" | "irradiance" | "weights" | "off") {
        return Err(bad(format!(
            "unknown mode {mode:?}: use \"validity\", \"irradiance\", \"weights\" or \"off\""
        )));
    }
    let size = positive(params, "size")?.unwrap_or(DEFAULT_SIZE);
    let exposure = positive(params, "exposure")?.unwrap_or(1.0);
    // Build first, so a bad call leaves the previous drawing alone.
    let built = if mode == "off" {
        None
    } else {
        let inst = installed(world)?;
        Some(match mode {
            "weights" => {
                let p = vec3(params, "p")?
                    .ok_or_else(|| bad("`p` is required for mode \"weights\": the world point to look up"))?;
                let n = unit(vec3(params, "n")?.unwrap_or([0.0, 1.0, 0.0]), "n")?;
                let v = unit(vec3(params, "v")?.unwrap_or(n), "v")?;
                let t = trace_static_gi(&inst.volume, &inst.settings, &inst.scales, &inst.relight, p, n, v);
                let drawn = t.brick_corners.len() + t.terrain_corners.len();
                (weights_asset(&t, p, size), drawn, drawn, None)
            }
            _ => {
                let query = probe_query(&inst, params, DEFAULT_DRAWN, MAX_DRAWN)?;
                let (points, matched) =
                    probe_debug_points(&inst.volume, &inst.settings, &inst.scales, &query);
                if mode == "validity" {
                    (validity_asset(&points, size), points.len(), matched, None)
                } else {
                    let (asset, peak) = irradiance_asset(&points, size, exposure);
                    (asset, points.len(), matched, Some(peak))
                }
            }
        })
    };
    let old: Vec<Entity> = world
        .query_filtered::<Entity, With<GiDebugGizmo>>()
        .iter(world)
        .collect();
    for e in old {
        world.despawn(e);
    }
    let Some((asset, drawn, matched, peak)) = built else {
        return Ok(json!({ "mode": "off", "drawn": 0, "matched": 0, "peak": null }));
    };
    if drawn > 0 {
        world.spawn((Gizmo::new(Arc::new(asset)), GiDebugGizmo));
    }
    Ok(json!({
        "mode": mode,
        "drawn": drawn,
        "matched": matched,
        "peak": peak,
        "note": "drawn as world gizmos from the next frame: validity green kept, yellow relocated, \
                 red inside geometry, magenta under terrain; irradiance is normalized by `peak`; \
                 weights shows blue-to-white lines by corner share, red for rejected corners and orange for \
                 corners behind a baked wall",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::baked_gi::{
        BakeInfo, BakeLights, DetailBrick, ProbeData, ProbeFlags, ProbeMeta, SH_Y00, ShRgb, TerrainLayer,
        VolumeLayout,
    };

    fn uniform(l: [f32; 3]) -> ShRgb {
        let k = 4.0 * std::f32::consts::PI * SH_Y00;
        [
            [l[0] * k, 0.0, 0.0, 0.0],
            [l[1] * k, 0.0, 0.0, 0.0],
            [l[2] * k, 0.0, 0.0, 0.0],
        ]
    }

    /// 4×4 columns × 3 levels over flat ground at 10 m, one brick; probe 0 is under the terrain,
    /// probe 1 relocated, probe 2 inside geometry.
    fn volume() -> Arc<BakedGiVolume> {
        let mut layout = VolumeLayout {
            terrain: Some(TerrainLayer {
                origin: [0.0, 0.0],
                spacing: [4.0, 4.0],
                counts: [4, 4],
                levels: vec![1.0, 2.0, 4.0],
                ground: vec![10.0; 16],
            }),
            bricks: vec![DetailBrick {
                origin: [4.0, 10.5, 4.0],
                spacing: [1.0, 1.0, 1.0],
                counts: [3, 3, 3],
                first_probe: 0,
                flags: 0,
            }],
            moment_resolution: 4,
            max_distance: 16.0,
        };
        layout.assign_brick_offsets();
        let probes: Vec<_> = (0..layout.probe_count())
            .map(|i| ProbeData {
                sh: [
                    uniform([0.0; 3]),
                    uniform([2.0, 1.0, 0.5]),
                    uniform([0.0; 3]),
                    uniform([0.0; 3]),
                ],
                moments: vec![[16.0, 256.0]; 16],
                meta: ProbeMeta {
                    offset: [0.0; 3],
                    flags: match i {
                        0 => ProbeFlags::UNDER_TERRAIN,
                        1 => ProbeFlags::VALID | ProbeFlags::RELOCATED,
                        2 => ProbeFlags::INSIDE_GEOMETRY,
                        _ => ProbeFlags::VALID,
                    },
                },
            })
            .collect();
        Arc::new(
            BakedGiVolume::from_probes(&layout, BakeLights::default(), BakeInfo::default(), &probes)
                .expect("volume"),
        )
    }

    fn world_with_volume() -> World {
        let mut world = World::new();
        let mut renderer = RuntimeRenderer::default();
        renderer.set_baked_gi(Some(volume()));
        world.insert_resource(renderer);
        world
    }

    fn overlays(world: &mut World) -> usize {
        world.query::<&GiDebugGizmo>().iter(world).count()
    }

    #[test]
    fn every_method_needs_a_renderer_with_a_volume() {
        let mut world = World::new();
        assert!(get_gi_probes(&mut world, &json!({})).is_err(), "no renderer");
        world.insert_resource(RuntimeRenderer::default());
        for err in [
            get_gi_probes(&mut world, &json!({})),
            explain_gi(&mut world, &json!({ "p": [0.0, 0.0, 0.0] })),
            set_gi_debug_overlay(&mut world, &json!({ "mode": "validity" })),
        ] {
            let err = err.expect_err("no volume installed");
            assert!(err.message.contains("no baked GI volume"), "{}", err.message);
        }
        // Switching the overlay off needs no volume.
        let off = set_gi_debug_overlay(&mut world, &json!({ "mode": "off" })).expect("off works");
        assert_eq!(off["drawn"], 0);
    }

    #[test]
    fn probes_report_counts_flags_and_filters() {
        let mut world = world_with_volume();
        let all = get_gi_probes(&mut world, &json!({ "max": 1000 })).expect("probes");
        let total = volume().probe_count();
        assert_eq!(all["matched"], total);
        assert_eq!(all["returned"], total);
        assert_eq!(all["summary"]["invalid"], 2);
        assert_eq!(all["summary"]["relocated"], 1);
        assert_eq!(all["summary"]["under_terrain"], 1);
        assert_eq!(all["summary"]["inside_geometry"], 1);
        assert_eq!(all["summary"]["bricks"], 1);
        assert_eq!(all["scales"]["source"], "as_baked", "no frame has lit the volume");
        assert!(all["radius"].is_null());
        let probes = all["probes"].as_array().expect("list");
        let by = |i: u64| probes.iter().find(|p| p["index"] == i).expect("listed");
        assert_eq!(by(0)["valid"], false);
        assert_eq!(by(0)["under_terrain"], true);
        assert_eq!(by(1)["relocated"], true);
        assert_eq!(by(2)["inside_geometry"], true);
        assert!(by(5)["luminance"].as_f64().expect("number") > 0.5);
        assert_eq!(by(5)["layer"], "terrain");
        let brick_probe = by(volume().bricks[0].first_probe as u64);
        assert_eq!(brick_probe["layer"], "brick");
        assert_eq!(brick_probe["brick"], 0);

        let invalid = get_gi_probes(&mut world, &json!({ "filter": "invalid" })).expect("probes");
        assert_eq!(invalid["matched"], 2);
        let few = get_gi_probes(&mut world, &json!({ "max": 3, "radius": 100.0 })).expect("probes");
        assert_eq!(few["returned"], 3);
        assert_eq!(few["matched"], total);
        assert_eq!(few["radius"], 100.0);
        let none = get_gi_probes(&mut world, &json!({ "max": 0 })).expect("probes");
        assert_eq!(none["returned"], 0);
        assert_eq!(none["summary"]["total"], total);
    }

    #[test]
    fn probe_params_are_checked() {
        let mut world = world_with_volume();
        for bad in [
            json!({ "center": [0.0, 1.0] }),
            json!({ "center": "here" }),
            json!({ "radius": 0.0 }),
            json!({ "radius": -3.0 }),
            json!({ "max": -1 }),
            json!({ "normal": [0.0, 0.0, 0.0] }),
            json!({ "filter": "nope" }),
        ] {
            assert!(get_gi_probes(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn audit_gi_rejects_unrepresentable_vectors_without_changing_lookup() {
        let mut world = world_with_volume();
        let valid = json!({ "p": [5.0, 11.5, 5.0] });
        let before = explain_gi(&mut world, &valid).expect("lookup");
        for key in ["n", "v", "p"] {
            let mut params = valid.clone();
            params[key] = json!([1e308, 0, 0]);
            let err = explain_gi(&mut world, &params).expect_err("out of f32 range");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
            assert!(err.message.contains(key), "{err:?}");
            assert_eq!(explain_gi(&mut world, &valid).expect("unchanged lookup"), before);
        }
        assert!(unit([f32::INFINITY, 0.0, 0.0], "n").is_err());
        assert!(unit([f32::NAN, 0.0, 0.0], "n").is_err());
        let normal = unit([1e30, 0.0, 0.0], "n").expect("finite large direction");
        assert_eq!(normal, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn audit_gi_positive_parameters_must_fit_in_f32() {
        let mut world = world_with_volume();
        for params in [json!({ "radius": 1e308 }), json!({ "radius": 1e-100 })] {
            let err = get_gi_probes(&mut world, &params).expect_err("invalid f32 radius");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        }
        for key in ["size", "exposure"] {
            let mut params = json!({ "mode": "irradiance" });
            params[key] = json!(1e308);
            let err = set_gi_debug_overlay(&mut world, &params).expect_err("invalid f32 parameter");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
            assert_eq!(overlays(&mut world), 0);
        }
    }

    #[test]
    fn explain_lists_the_corners_and_their_shares() {
        let mut world = world_with_volume();
        // Between brick probes: the brick layer carries the point.
        let got = explain_gi(&mut world, &json!({ "p": [5.0, 11.5, 5.0] })).expect("explain");
        assert_eq!(got["brick"], 0);
        let corners = got["brick_corners"].as_array().expect("list");
        assert_eq!(corners.len(), 8);
        let share: f64 = corners.iter().map(|c| c["share"].as_f64().expect("number")).sum();
        assert!((share - 1.0).abs() < 1e-3, "{share}");
        assert!(got["coverage"].as_f64().expect("number") > 0.99);
        assert!(got["irradiance"][0].as_f64().expect("number") > 1.0);
        assert!(got["components"]["sky"].is_array());
        // No wall codes in this volume, open moments: nothing walled, every receiver seen.
        assert!(corners.iter().all(|c| c["walled"] == false));
        assert!(
            corners
                .iter()
                .all(|c| c["visibility"].as_f64().expect("number") > 0.99)
        );
        assert!(got["brick_visibility"].as_f64().expect("number") > 0.99);
        assert_eq!(
            got["sun_relight"], 1.0,
            "no frame lit the volume: the sun as baked"
        );
        // Over open terrain: only the terrain layer, rejected corners are listed with no weight.
        let open = explain_gi(
            &mut world,
            &json!({ "p": [1.0, 11.5, 1.0], "n": [0.0, 2.0, 0.0] }),
        )
        .expect("explain");
        assert!(open["brick"].is_null());
        let terrain = open["terrain_corners"].as_array().expect("list");
        assert_eq!(terrain.len(), 8);
        assert!(
            terrain
                .iter()
                .filter(|c| c["valid"] == false)
                .all(|c| c["weight"] == 0.0)
        );
        assert!(
            !terrain.iter().all(|c| c["valid"] == true),
            "probe 0 (under the terrain) is a corner here"
        );
        let codes = open["findings"].as_array().expect("list");
        assert!(codes.contains(&json!("rejected_corners")), "{codes:?}");
        // Far outside the volume: nothing, and it says so.
        let out = explain_gi(&mut world, &json!({ "p": [500.0, 11.5, 500.0] })).expect("explain");
        assert_eq!(out["coverage"], 0.0);
        assert!(
            out["findings"]
                .as_array()
                .expect("list")
                .contains(&json!("outside_volume"))
        );
        for bad in [
            json!({}),
            json!({ "p": [1.0, 2.0] }),
            json!({ "p": [1.0, 2.0, 3.0], "n": [0.0, 0.0, 0.0] }),
            json!({ "p": [1.0, 2.0, 3.0], "v": [0.0, 0.0, 0.0] }),
        ] {
            assert!(explain_gi(&mut world, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_overlay_replaces_itself_and_turns_off() {
        let mut world = world_with_volume();
        let v = set_gi_debug_overlay(&mut world, &json!({ "mode": "validity", "max": 50 })).expect("overlay");
        assert_eq!(v["drawn"], 50);
        assert_eq!(overlays(&mut world), 1);
        let i = set_gi_debug_overlay(&mut world, &json!({ "mode": "irradiance", "filter": "valid" }))
            .expect("overlay");
        assert!(i["peak"].as_f64().expect("number") > 1.0, "{i}");
        assert_eq!(overlays(&mut world), 1, "the new drawing replaced the old");
        let w = set_gi_debug_overlay(&mut world, &json!({ "mode": "weights", "p": [5.0, 11.5, 5.0] }))
            .expect("overlay");
        assert_eq!(w["drawn"], 8);
        assert_eq!(overlays(&mut world), 1);
        // A bad call leaves the drawing alone.
        assert!(set_gi_debug_overlay(&mut world, &json!({ "mode": "weights" })).is_err());
        assert!(set_gi_debug_overlay(&mut world, &json!({ "mode": "lava" })).is_err());
        assert!(set_gi_debug_overlay(&mut world, &json!({})).is_err());
        assert!(set_gi_debug_overlay(&mut world, &json!({ "mode": "validity", "size": 0.0 })).is_err());
        assert_eq!(overlays(&mut world), 1);
        // Nothing matches: nothing is drawn.
        let none = set_gi_debug_overlay(
            &mut world,
            &json!({ "mode": "validity", "center": [900.0, 0.0, 900.0], "radius": 1.0 }),
        )
        .expect("overlay");
        assert_eq!(none["drawn"], 0);
        assert_eq!(overlays(&mut world), 0);
        set_gi_debug_overlay(&mut world, &json!({ "mode": "validity", "max": 5 })).expect("overlay");
        let off = set_gi_debug_overlay(&mut world, &json!({ "mode": "off" })).expect("off");
        assert_eq!(off["mode"], "off");
        assert_eq!(overlays(&mut world), 0);
    }

    #[test]
    fn colors_tell_the_flags_apart() {
        let base = ProbeDebugPoint {
            index: 0,
            layer: regia_render::baked_gi::GiLayer::Terrain,
            brick: None,
            interior: false,
            position: [0.0; 3],
            flags: ProbeFlags::VALID,
            valid: true,
            relocated: false,
            under_terrain: false,
            inside_geometry: false,
            irradiance_over_pi: [0.0; 3],
            mean_distance: 0.0,
        };
        let kept = validity_color(&base);
        let moved = validity_color(&ProbeDebugPoint {
            relocated: true,
            ..base.clone()
        });
        let under = validity_color(&ProbeDebugPoint {
            valid: false,
            under_terrain: true,
            ..base.clone()
        });
        let inside = validity_color(&ProbeDebugPoint {
            valid: false,
            inside_geometry: true,
            ..base
        });
        let all = [kept, moved, under, inside];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.to_array(), b.to_array());
            }
        }
        assert!(heat(1.0).to_array()[0] > heat(0.0).to_array()[0]);
    }
}
