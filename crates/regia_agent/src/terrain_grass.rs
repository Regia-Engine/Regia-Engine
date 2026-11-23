// SPDX-License-Identifier: MPL-2.0
//! The grass field on the agent protocol (ADR-1400, `book/agents/terrain-grass.md`):
//! `terrain.grass` reads the [`TerrainGrassHandle`] (the request in effect and the last frame's
//! report), `terrain.set_grass` patches the request (enabled, tier, density, range, shadows,
//! sway, debug view) and can queue a density edit (`clear`: a burnt patch or a crater). The
//! bound `regia_terrain::GrassLayer` applies both on its next frame.

use regia_app::prelude::*;
use regia_terrain::{GrassQuality, GrassRequest, TerrainGrassHandle};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn handle(world: &World) -> Result<TerrainGrassHandle, RpcError> {
    let handle = world.get_resource::<TerrainGrassHandle>().cloned().ok_or_else(|| {
        RpcError::not_found(
            "no TerrainGrassHandle resource in this app",
            "bind a regia_terrain::GrassLayer to the terrain draw (TerrainSharedDraw::set_grass) and insert its handle() as a resource",
        )
    })?;
    if !handle.is_linked() {
        return Err(RpcError::not_found(
            "the TerrainGrassHandle resource is not linked to a grass layer",
            "insert GrassLayer::handle() (or TerrainSharedDraw::grass_handle()) as the resource",
        ));
    }
    Ok(handle)
}

fn state(handle: &TerrainGrassHandle) -> Result<Value, RpcError> {
    let to_json = |v: Result<Value, serde_json::Error>| {
        v.map_err(|e| RpcError::internal(format!("grass report serialization failed: {e}")))
    };
    Ok(json!({
        "request": to_json(serde_json::to_value(handle.request()))?,
        "report": to_json(serde_json::to_value(handle.report()))?,
    }))
}

/// `terrain.grass`: the request in effect and the last frame's report (tier, classes, patches
/// per level, candidates, range, shadow distance and cascades, mask size and generation).
pub fn grass(world: &mut World) -> Result<Value, RpcError> {
    state(&handle(world)?)
}

const KNOWN: &[&str] = &[
    "enabled",
    "quality",
    "density_scale",
    "range_scale",
    "shadows",
    "sway_scale",
    "debug_view",
    "clear",
];

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_f64()
                .map(|f| f as f32)
                .filter(|f| f.is_finite())
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number")))
        })
        .transpose()
}

fn boolean(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    params
        .get(name)
        .map(|v| {
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a boolean")))
        })
        .transpose()
}

/// `terrain.set_grass`: any of `enabled`, `quality` (`off | minimal | low | medium | high |
/// ultra`, or `"auto"` to follow the render profile), `density_scale` (0..1), `range_scale`
/// (0.1..4), `shadows`, `sway_scale` (0..4), `debug_view` (0 shaded, 1 LOD, 2 patch level,
/// 3 fade, 4 foliage transmission), and `clear: { at: [x, z], radius, factor }` (scale the density there; factor 0
/// clears). Returns the request before and after; applies on the next frame.
pub fn set_grass(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(object) = params.as_object() else {
        return Err(RpcError::invalid_params("params must be an object"));
    };
    if let Some(unknown) = object.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(RpcError::invalid_params(format!(
            "unknown parameter `{unknown}` (known: {})",
            KNOWN.join(", ")
        )));
    }
    if object.is_empty() {
        return Err(RpcError::invalid_params(format!(
            "pass at least one of {}",
            KNOWN.join(", ")
        )));
    }
    let quality = match params.get("quality") {
        None => None,
        Some(v) => {
            let name = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`quality` must be a tier name or \"auto\""))?;
            if name == "auto" {
                Some(None)
            } else {
                let q = GrassQuality::from_name(name).ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "unknown grass quality `{name}`: auto | {}",
                        GrassQuality::ALL.map(GrassQuality::name).join(" | ")
                    ))
                })?;
                Some(Some(q))
            }
        }
    };
    let enabled = boolean(params, "enabled")?;
    let shadows = boolean(params, "shadows")?;
    let density = number(params, "density_scale")?;
    let range = number(params, "range_scale")?;
    let sway = number(params, "sway_scale")?;
    let debug = match params.get("debug_view") {
        None => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|d| *d <= 4)
                .ok_or_else(|| RpcError::invalid_params("`debug_view` must be 0, 1, 2, 3 or 4"))?
                as u32,
        ),
    };
    let clear = match params.get("clear") {
        None => None,
        Some(v) => {
            let at = crate::params::opt_array(v, "at")?
                .filter(|a| a.len() == 2)
                .and_then(|a| -> Option<[f32; 2]> {
                    let finite = |v: &Value| v.as_f64().filter(|n| n.is_finite() && (*n as f32).is_finite());
                    Some([finite(&a[0])? as f32, finite(&a[1])? as f32])
                })
                .ok_or_else(|| RpcError::invalid_params("`clear.at` must be [x, z] in meters"))?;
            let radius = crate::params::opt_f64(v, "radius")?
                .map(|r| r as f32)
                .filter(|r| r.is_finite() && *r > 0.0 && *r <= 10_000.0)
                .ok_or_else(|| RpcError::invalid_params("`clear.radius` must be in (0, 10000] meters"))?;
            let factor = match v.get("factor") {
                None => 0.0,
                Some(f) => f
                    .as_f64()
                    .map(|f| f as f32)
                    .filter(|f| (0.0..=1.0).contains(f))
                    .ok_or_else(|| RpcError::invalid_params("`clear.factor` must be in 0..1"))?,
            };
            Some((at, radius, factor))
        }
    };
    let handle = handle(world)?;
    let before = state(&handle)?;
    let mut request: GrassRequest = handle.request();
    if let Some(on) = enabled {
        request.enabled = on;
    }
    if let Some(q) = quality {
        request.quality = q;
    }
    if let Some(v) = density {
        request.density_scale = v;
    }
    if let Some(v) = range {
        request.range_scale = v;
    }
    if let Some(on) = shadows {
        request.shadows = on;
    }
    if let Some(v) = sway {
        request.sway_scale = v;
    }
    if let Some(v) = debug {
        request.debug_view = v;
    }
    handle.set_request(request);
    if let Some((at, radius, factor)) = clear {
        handle.scale_density(at[0], at[1], radius, factor);
    }
    Ok(json!({ "before": before["request"], "after": state(&handle)?["request"] }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &RpcError) -> &str {
        err.data.as_ref().and_then(|d| d["code"].as_str()).unwrap_or("")
    }

    #[test]
    fn missing_or_unlinked_handles_are_not_found() {
        let mut world = World::new();
        let err = grass(&mut world).expect_err("no resource");
        assert_eq!(code(&err), "F0003");
        world.insert_resource(TerrainGrassHandle::default());
        let err = grass(&mut world).expect_err("unlinked");
        assert_eq!(code(&err), "F0003");
        let err = set_grass(&mut world, &json!({ "enabled": false })).expect_err("unlinked");
        assert_eq!(code(&err), "F0003");
    }

    #[test]
    fn bad_params_are_refused_before_the_handle_is_read() {
        let mut world = World::new();
        for params in [
            json!({}),
            json!({ "bogus": 1 }),
            json!({ "quality": "extreme" }),
            json!({ "quality": 3 }),
            json!({ "density_scale": "a lot" }),
            json!({ "debug_view": 7 }),
            json!({ "clear": { "at": [1.0], "radius": 2.0 } }),
            json!({ "clear": { "at": [1.0, 2.0], "radius": -2.0 } }),
            json!({ "clear": { "at": [1.0, 2.0], "radius": 2.0, "factor": 3.0 } }),
        ] {
            let err = set_grass(&mut world, &params).expect_err("refused");
            assert_eq!(code(&err), "F0002", "{params}: {err:?}");
        }
    }

    #[test]
    fn set_grass_patches_the_request() {
        let mut world = World::new();
        let handle = TerrainGrassHandle::new_linked();
        world.insert_resource(handle.clone());
        let out = set_grass(
            &mut world,
            &json!({ "quality": "low", "shadows": false, "density_scale": 0.5, "clear": { "at": [1.0, 2.0], "radius": 3.0 } }),
        )
        .expect("set");
        assert_eq!(out["before"]["quality"], Value::Null, "{out}");
        assert_eq!(out["after"]["quality"], json!("low"), "{out}");
        assert_eq!(out["after"]["shadows"], json!(false), "{out}");
        let request = handle.request();
        assert_eq!(request.quality, Some(GrassQuality::Low));
        assert_eq!(request.density_scale, 0.5);
        let out = set_grass(&mut world, &json!({ "quality": "auto" })).expect("auto");
        assert_eq!(out["after"]["quality"], Value::Null, "{out}");
        let read = grass(&mut world).expect("read");
        assert_eq!(read["report"]["installed"], json!(false), "{read}");
    }

    /// `clear.at` of a finite f64 that overflows f32 is refused before any edit. The density
    /// scale in the same call stays at the previous value.
    #[test]
    fn clear_at_rejects_non_finite_without_editing() {
        let mut world = World::new();
        let handle = TerrainGrassHandle::new_linked();
        world.insert_resource(handle.clone());
        set_grass(&mut world, &json!({ "density_scale": 0.5 })).expect("baseline");
        let err = set_grass(
            &mut world,
            &json!({
                "density_scale": 0.25,
                "clear": { "at": [1.0e308, 0.0], "radius": 3.0 }
            }),
        )
        .expect_err("overflow");
        assert_eq!(code(&err), "F0002", "{err:?}");
        assert_eq!(handle.request().density_scale, 0.5);
    }
}
