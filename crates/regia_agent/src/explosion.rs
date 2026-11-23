// SPDX-License-Identifier: MPL-2.0
//! `gameplay.explode` and `gameplay.explosion_stats` (book/agents/explosion.md, ADR-2430): the
//! agent face of `regia_framework::explosion`.
//!
//! Handlers are plain functions over `&mut World` (tests drive them without a server). A world
//! without `ExplosionPlugin` answers `UNSUPPORTED` (`F0016`, reason `no_explosion_plugin`).
//! A bad parameter answers `INVALID_PARAMS` (`F0002`) with the reason in `data.reason`:
//! `bad_center`, `bad_radius`, `bad_impulse`, `bad_tnt`, `bad_upward_bias`, `bad_torque`,
//! `unknown_preset`, `missing_size`, `bad_falloff`, `bad_occlusion`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::explosion::{Explosion, ExplosionError, ExplosionStats, Falloff, Occlusion, explode};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

fn refuse(reason: &str, message: impl Into<String>) -> RpcError {
    RpcError {
        code: protocol::INVALID_PARAMS,
        message: message.into(),
        data: Some(json!({
            "code": "F0002",
            "reason": reason,
            "hint": "see book/agents/explosion.md for the parameters and the error reasons",
        })),
    }
}

fn from_error(e: ExplosionError) -> RpcError {
    refuse(e.code(), e.to_string())
}

fn unsupported() -> RpcError {
    RpcError {
        code: protocol::UNSUPPORTED,
        message: "no explosion system in this app".into(),
        data: Some(json!({
            "code": "F0016",
            "reason": "no_explosion_plugin",
            "hint": "add regia_framework::explosion::ExplosionPlugin (and PhysicsPlugin) so explosions have something to push",
        })),
    }
}

fn number(params: &Value, field: &str, reason: &str) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if (n as f32).is_finite() => Ok(Some(n as f32)),
            _ => Err(refuse(reason, format!("`{field}` must be a finite number"))),
        },
    }
}

fn center(params: &Value) -> Result<Vec3, RpcError> {
    let bad = || refuse("bad_center", "`center` must be [x, y, z], three finite numbers");
    let items = params.get("center").and_then(Value::as_array).ok_or_else(bad)?;
    if items.len() != 3 {
        return Err(bad());
    }
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item.as_f64().ok_or_else(bad)? as f32;
        if !n.is_finite() {
            return Err(bad());
        }
        *slot = n;
    }
    Ok(Vec3::from_array(out))
}

fn text<'a>(params: &'a Value, field: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(refuse(
            &format!("bad_{field}"),
            format!("`{field}` must be a string"),
        )),
    }
}

/// The explosion `params` describe.
pub(crate) fn explosion_from(params: &Value) -> Result<Explosion, RpcError> {
    let at = center(params)?;
    let tnt = number(params, "tnt_kg", "bad_tnt")?;
    let radius = number(params, "radius", "bad_radius")?;
    let impulse = number(params, "impulse", "bad_impulse")?;
    let mut e = match text(params, "preset")? {
        Some(name) => Explosion::preset(name, at).map_err(from_error)?,
        None => match (tnt, radius, impulse) {
            (Some(kg), _, _) => Explosion::from_tnt(at, kg),
            (None, Some(_), Some(_)) => Explosion::from_tnt(at, 0.0),
            _ => {
                return Err(refuse(
                    "missing_size",
                    "name a `preset`, a `tnt_kg`, or both `radius` and `impulse`",
                ));
            }
        },
    };
    if let (Some(kg), Some(_)) = (tnt, text(params, "preset")?) {
        let sized = Explosion::from_tnt(at, kg);
        e.tnt_kg = kg;
        e.radius = sized.radius;
        e.impulse = sized.impulse;
    }
    if let Some(r) = radius {
        e.radius = r;
    }
    if let Some(i) = impulse {
        e.impulse = i;
    }
    if let Some(b) = number(params, "upward_bias", "bad_upward_bias")? {
        e.upward_bias = b;
    }
    if let Some(t) = number(params, "torque", "bad_torque")? {
        e.torque = t;
    }
    match text(params, "falloff")? {
        None => {}
        Some("linear") => e.falloff = Falloff::Linear,
        Some("inverse_square") => {
            let floor = number(params, "floor", "bad_falloff")?.unwrap_or(e.radius * 0.1);
            e.falloff = Falloff::InverseSquare { floor };
        }
        Some(_) => {
            return Err(refuse(
                "bad_falloff",
                "`falloff` must be \"linear\" or \"inverse_square\"",
            ));
        }
    }
    match text(params, "occlusion")? {
        None => {}
        Some("none") => e.occlusion = Occlusion::None,
        Some("ray") => e.occlusion = Occlusion::RayCast,
        Some(_) => return Err(refuse("bad_occlusion", "`occlusion` must be \"none\" or \"ray\"")),
    }
    if let Some(seed) = crate::params::opt_u64(params, "seed")? {
        e.seed = seed;
    }
    for (field, slot) in [
        ("fx", &mut e.fx),
        ("destruction", &mut e.destruction),
        ("gust", &mut e.gust),
    ] {
        match params.get(field) {
            None | Some(Value::Null) => {}
            Some(Value::Bool(b)) => *slot = *b,
            Some(_) => {
                return Err(refuse(
                    &format!("bad_{field}"),
                    format!("`{field}` must be a boolean"),
                ));
            }
        }
    }
    e.validate().map_err(from_error)?;
    Ok(e)
}

fn describe(e: &Explosion) -> Value {
    json!({
        "center": [e.center.x, e.center.y, e.center.z],
        "tnt_kg": e.tnt_kg,
        "radius": e.radius,
        "impulse": e.impulse,
        "peak_delta_v": e.peak_delta_v(),
        "falloff": match e.falloff {
            Falloff::Linear => json!({ "kind": "linear" }),
            Falloff::InverseSquare { floor } => json!({ "kind": "inverse_square", "floor": floor }),
        },
        "upward_bias": e.upward_bias,
        "occlusion": match e.occlusion { Occlusion::None => "none", Occlusion::RayCast => "ray" },
        "torque": e.torque,
        "seed": e.seed,
    })
}

/// `gameplay.explode { center, preset?, tnt_kg?, radius?, impulse?, falloff?, floor?,
/// upward_bias?, occlusion?, torque?, seed?, fx?, destruction?, gust? }`: queues the explosion
/// for the next fixed step.
pub fn gameplay_explode(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<ExplosionStats>().is_none() {
        return Err(unsupported());
    }
    let e = explosion_from(params)?;
    explode(world, e).map_err(from_error)?;
    Ok(json!({ "queued": true, "explosion": describe(&e), "presets": Explosion::PRESETS }))
}

/// `gameplay.explosion_stats {}`: what the explosions did so far.
pub fn gameplay_explosion_stats(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(s) = world.get_resource::<ExplosionStats>().copied() else {
        return Err(unsupported());
    };
    Ok(json!({
        "explosions": s.explosions,
        "skipped_replica": s.skipped_replica,
        "bodies_hit": s.bodies_hit,
        "bodies_occluded": s.bodies_occluded,
        "impulse_total": s.impulse_total,
        "joints_broken": s.joints_broken,
        "damage_queued": s.damage_queued,
        "chunks_hit": s.chunks_hit,
        "chunks_pushed": s.chunks_pushed,
        "gusts": s.gusts,
        "fx_played": s.fx_played,
        "shakes": s.shakes,
    }))
}

#[cfg(test)]
mod tests {
    use regia_framework::explosion::ExplosionPlugin;

    use super::*;

    fn world() -> App {
        let mut app = App::new();
        app.add_plugins(ExplosionPlugin);
        app
    }

    fn reason(e: &RpcError) -> &str {
        e.data.as_ref().and_then(|d| d["reason"].as_str()).unwrap_or("")
    }

    #[test]
    fn a_world_without_the_plugin_answers_unsupported() {
        let mut w = World::new();
        let e = gameplay_explode(&mut w, &json!({ "center": [0, 0, 0], "preset": "grenade" }))
            .expect_err("no plugin");
        assert_eq!(
            (e.code, reason(&e)),
            (protocol::UNSUPPORTED, "no_explosion_plugin")
        );
        assert!(gameplay_explosion_stats(&mut w, &json!({})).is_err());
    }

    #[test]
    fn presets_and_overrides_build_the_explosion() {
        let mut app = world();
        let r = gameplay_explode(
            app.world_mut(),
            &json!({ "center": [1, 2, 3], "preset": "barrel", "radius": 9.0, "occlusion": "none" }),
        )
        .expect("queued");
        assert_eq!(r["explosion"]["radius"], 9.0);
        assert_eq!(r["explosion"]["occlusion"], "none");
        assert_eq!(r["explosion"]["falloff"]["kind"], "inverse_square");
        let r = gameplay_explode(app.world_mut(), &json!({ "center": [0, 0, 0], "tnt_kg": 8.0 }))
            .expect("queued");
        assert!((r["explosion"]["radius"].as_f64().unwrap_or(0.0) - 24.0).abs() < 1e-3);
        let r = gameplay_explode(
            app.world_mut(),
            &json!({ "center": [0, 0, 0], "radius": 5.0, "impulse": 300.0 }),
        )
        .expect("queued");
        assert_eq!(r["explosion"]["tnt_kg"], 0.0);
    }

    #[test]
    fn every_bad_parameter_has_its_reason() {
        let mut app = world();
        let cases = [
            (json!({ "preset": "grenade" }), "bad_center"),
            (json!({ "center": [0, 0] }), "bad_center"),
            (json!({ "center": [0, 0, 0] }), "missing_size"),
            (json!({ "center": [0, 0, 0], "preset": "nuke" }), "unknown_preset"),
            (
                json!({ "center": [0, 0, 0], "radius": -2.0, "impulse": 1.0 }),
                "bad_radius",
            ),
            (
                json!({ "center": [0, 0, 0], "radius": 2.0, "impulse": -1.0 }),
                "bad_impulse",
            ),
            (json!({ "center": [0, 0, 0], "tnt_kg": -1.0 }), "bad_tnt"),
            (
                json!({ "center": [0, 0, 0], "tnt_kg": 1.0, "upward_bias": 3.0 }),
                "bad_upward_bias",
            ),
            (
                json!({ "center": [0, 0, 0], "tnt_kg": 1.0, "falloff": "cubic" }),
                "bad_falloff",
            ),
            (
                json!({ "center": [0, 0, 0], "tnt_kg": 1.0, "occlusion": "x" }),
                "bad_occlusion",
            ),
            (json!({ "center": [0, 0, 0], "tnt_kg": "a" }), "bad_tnt"),
        ];
        for (params, want) in cases {
            let e = gameplay_explode(app.world_mut(), &params).expect_err(want);
            assert_eq!(reason(&e), want, "{params}");
            assert_eq!(e.code, protocol::INVALID_PARAMS);
        }
    }

    #[test]
    fn stats_start_at_zero() {
        let mut app = world();
        let s = gameplay_explosion_stats(app.world_mut(), &json!({})).expect("stats");
        assert_eq!(s["explosions"], 0);
        assert_eq!(s["bodies_hit"], 0);
    }
}
