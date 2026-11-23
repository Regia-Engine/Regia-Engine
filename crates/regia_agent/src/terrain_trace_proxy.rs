// SPDX-License-Identifier: MPL-2.0
//! `terrain.trace_proxy` / `terrain.set_trace_proxy`: the terrain's caster proxy for the ray-traced
//! sun (ADR-2310, book `agents/hwrt.md`).
//!
//! The host inserts [`TerrainTraceProxyControl`] with its terrain draw's handle; the draw builds the
//! proxy while the frame traces (`render.set_sun_shadow_trace on|compute`) and rebuilds it with new
//! settings the next frame that traces. Without the resource both methods answer `F0003`; settings
//! out of range are `F0002` and change nothing.

use regia_ecs::prelude::*;
use regia_terrain::{TerrainTraceProxyControl, TraceProxySettings, TraceProxyStats};
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn missing() -> RpcError {
    RpcError::not_found(
        "no terrain trace proxy control in this app",
        "the host inserts TerrainTraceProxyControl(draw.trace_proxy_handle()) with its terrain draw",
    )
}

fn settings_json(settings: &TraceProxySettings) -> Value {
    json!({
        "spacing_m": settings.spacing_m,
        "max_triangles": settings.max_triangles,
        "tile_quads": settings.tile_quads,
    })
}

fn stats_json(stats: Option<TraceProxyStats>) -> Value {
    stats.map_or(Value::Null, |s| {
        json!({
            "stride": s.stride,
            "spacing_m": s.spacing_m,
            "tiles": s.tiles,
            "triangles": s.triangles,
            "tile_builds": s.tile_builds,
        })
    })
}

/// `terrain.trace_proxy`: the settings the next build uses, and what the built proxy holds (`null`
/// while no frame traces).
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let control = world
        .get_resource::<TerrainTraceProxyControl>()
        .ok_or_else(missing)?;
    Ok(json!({
        "settings": settings_json(&control.0.settings()),
        "proxy": stats_json(control.0.stats()),
    }))
}

/// `terrain.set_trace_proxy { spacing_m?, max_triangles?, tile_quads? }`: changes the given fields;
/// a built proxy is rebuilt with them the next frame that traces.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let handle = world
        .get_resource::<TerrainTraceProxyControl>()
        .ok_or_else(missing)?
        .0
        .clone();
    let before = handle.settings();
    let mut after = before;
    let mut any = false;
    if let Some(value) = params.get("spacing_m") {
        let spacing = value
            .as_f64()
            .ok_or_else(|| RpcError::invalid_params("`spacing_m` must be a number of metres"))?;
        after.spacing_m = spacing as f32;
        any = true;
    }
    for (name, field) in [
        ("max_triangles", &mut after.max_triangles),
        ("tile_quads", &mut after.tile_quads),
    ] {
        if let Some(value) = params.get(name) {
            *field = value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`{name}` must be a non-negative integer"))
                })?;
            any = true;
        }
    }
    if !any {
        return Err(RpcError::invalid_params(
            "pass at least one of spacing_m, max_triangles, tile_quads",
        ));
    }
    if let Some(why) = after.invalid() {
        return Err(RpcError::invalid_params(why));
    }
    handle.set_settings(after);
    Ok(json!({
        "before": settings_json(&before),
        "after": settings_json(&after),
        "note": "applies the next frame that traces scene geometry (render.set_sun_shadow_trace on or compute)",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &RpcError) -> Option<Value> {
        err.data.as_ref().map(|d| d["code"].clone())
    }

    #[test]
    fn without_the_control_both_answer_not_found() {
        let mut world = World::new();
        let err = get(&mut world).expect_err("no control");
        assert_eq!(code(&err), Some(json!("F0003")));
        let err = set(&mut world, &json!({ "spacing_m": 4.0 })).expect_err("no control");
        assert_eq!(code(&err), Some(json!("F0003")));
    }

    #[test]
    fn settings_reach_the_draw_handle_and_stats_read_back() {
        let mut world = World::new();
        let control = TerrainTraceProxyControl::default();
        let handle = control.0.clone();
        world.insert_resource(control);
        let reply = get(&mut world).expect("get");
        assert_eq!(reply["settings"]["spacing_m"], 2.0);
        assert_eq!(reply["settings"]["tile_quads"], 64);
        assert!(reply["proxy"].is_null(), "nothing traced yet");

        let reply = set(&mut world, &json!({ "spacing_m": 4.0, "tile_quads": 32 })).expect("set");
        assert_eq!(reply["before"]["spacing_m"], 2.0);
        assert_eq!(reply["after"]["spacing_m"], 4.0);
        assert_eq!(handle.settings().tile_quads, 32);
        assert_eq!(handle.settings().max_triangles, 2_000_000, "untouched");

        handle.set_stats(Some(TraceProxyStats {
            stride: 2,
            spacing_m: 4.0,
            tiles: 16,
            triangles: 2048,
            tile_builds: 16,
        }));
        let reply = get(&mut world).expect("get");
        assert_eq!(reply["proxy"]["tiles"], 16);
        assert_eq!(reply["proxy"]["triangles"], 2048);
    }

    #[test]
    fn bad_settings_are_refused_and_change_nothing() {
        let mut world = World::new();
        let control = TerrainTraceProxyControl::default();
        let handle = control.0.clone();
        world.insert_resource(control);
        for bad in [
            json!({}),
            json!({ "spacing_m": 0.0 }),
            json!({ "spacing_m": -1.0 }),
            json!({ "spacing_m": 5000.0 }),
            json!({ "spacing_m": "far" }),
            json!({ "max_triangles": 1 }),
            json!({ "tile_quads": 0 }),
            json!({ "tile_quads": 4096 }),
            json!({ "tile_quads": -3 }),
        ] {
            let err = set(&mut world, &bad).expect_err("refused");
            assert_eq!(code(&err), Some(json!("F0002")), "{bad}");
        }
        assert_eq!(handle.settings(), TraceProxySettings::default());
    }
}
