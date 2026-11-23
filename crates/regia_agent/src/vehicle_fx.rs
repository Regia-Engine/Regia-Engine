// SPDX-License-Identifier: MPL-2.0
//! Wheel surface effects on the agent protocol (ADR-3274, `book/agents/vehicle-fx.md`):
//!
//! - `vfx.vehicle_fx {}` reads the plugin's state: per wheel the surface, slip, speed, the effects
//!   playing and the mark being laid, the tuning, and the stamp pool's counters.
//! - `vfx.vehicle_fx_set { enabled?, default_surface?, spacing?, max_turn?, smoke_slip_threshold?,
//!   skid_slip_threshold?, min_speed?, mark_lifetime_s?, max_stamps?, effect_scale?, clear_marks? }`
//!   changes the switch and the tuning (validated before anything changes) and returns
//!   `{ before, after }`.
//!
//! Error reasons (`data.reason`): `no_vehicle_fx` (no `VehicleFxPlugin`), `unknown_field`,
//! `invalid_value`, `unknown_surface`.

use regia_app::prelude::*;
use regia_framework::vehicle_fx::{VehicleFxState, WheelFxReport};
use regia_vfx::vehicle_fx::{MAX_STAMP_CAP, WheelSurface};
use serde_json::{Map, Value, json};

use crate::post_volumes::error;
use crate::protocol::{self, RpcError};

/// The fields `vfx.vehicle_fx_set` accepts.
pub(crate) const SET_FIELDS: &[&str] = &[
    "enabled",
    "default_surface",
    "spacing",
    "max_turn",
    "smoke_slip_threshold",
    "skid_slip_threshold",
    "min_speed",
    "mark_lifetime_s",
    "max_stamps",
    "effect_scale",
    "clear_marks",
];

/// An `F0002` refusal whose hint names this module's book page.
fn invalid(reason: &str, message: impl Into<String>) -> RpcError {
    error(
        protocol::INVALID_PARAMS,
        "F0002",
        reason,
        message,
        "see book/agents/vehicle-fx.md",
    )
}

/// The params object (`null` reads as `{}`).
fn object(params: &Value) -> Result<&Map<String, Value>, RpcError> {
    match params {
        Value::Object(map) => Ok(map),
        Value::Null => {
            static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
            Ok(EMPTY.get_or_init(Map::new))
        }
        _ => Err(invalid("invalid_value", "params must be an object")),
    }
}

/// Refuses a key outside `allowed` with `unknown_field`.
fn check_fields(map: &Map<String, Value>, allowed: &[&str]) -> Result<(), RpcError> {
    match map.keys().find(|k| !allowed.contains(&k.as_str())) {
        Some(key) => Err(invalid(
            "unknown_field",
            format!("unknown field `{key}`; one of {}", allowed.join(", ")),
        )),
        None => Ok(()),
    }
}

fn missing() -> RpcError {
    error(
        protocol::UNSUPPORTED,
        "F0016",
        "no_vehicle_fx",
        "no VehicleFxState in this app",
        "add regia_framework::vehicle_fx::VehicleFxPlugin (see book/agents/vehicle-fx.md)",
    )
}

fn wheel_json(w: &WheelFxReport) -> Value {
    let o = &w.output;
    json!({
        "key": w.key,
        "surface": w.surface.name(),
        "slip": o.slip,
        "slip_ratio": o.slip_ratio,
        "slip_lateral": o.slip_lateral,
        "speed": o.speed,
        "mark": o.mark.name(),
        "smoke": o.smoke,
        "dust": o.dust,
        "spray": o.spray,
        "smoke_rate": o.smoke_rate,
        "dust_rate": o.dust_rate,
        "spray_rate": o.spray_rate,
        "new_stamps": o.new_stamps,
        "active": w.active.iter().flatten().collect::<Vec<_>>(),
    })
}

fn state_json(state: &VehicleFxState) -> Value {
    let c = state.config();
    let stats = state.logic().stats();
    let wheels: Vec<Value> = state.wheels().iter().map(wheel_json).collect();
    json!({
        "enabled": state.enabled,
        "default_surface": state.default_surface.name(),
        "terrain_probe": state.terrain_probe.is_some(),
        "config": {
            "spacing": c.spacing,
            "max_turn": c.max_turn,
            "smoke_slip_threshold": c.smoke_slip_threshold,
            "skid_slip_threshold": c.skid_slip_threshold,
            "min_speed": c.min_speed,
            "mark_lifetime_s": c.mark_lifetime_s,
            "max_stamps": c.limits.max_stamps,
            "effect_scale": c.limits.effect_scale,
        },
        "stamps": {
            "live": stats.live,
            "emitted": stats.emitted,
            "retired": stats.retired,
            "retired_by_cap": stats.retired_by_cap,
            "cap": c.limits.max_stamps,
        },
        "count": wheels.len(),
        "wheels": wheels,
    })
}

/// `vfx.vehicle_fx`.
pub fn read(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    check_fields(object(params)?, &[])?;
    let state = world.get_resource::<VehicleFxState>().ok_or_else(missing)?;
    Ok(state_json(state))
}

fn number(map: &Map<String, Value>, key: &str, range: (f64, f64)) -> Result<Option<f32>, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => match v.as_f64() {
            Some(n) if n.is_finite() && (range.0..=range.1).contains(&n) => Ok(Some(n as f32)),
            _ => Err(invalid(
                "invalid_value",
                format!("`{key}` must be a number from {} to {}", range.0, range.1),
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

/// `vfx.vehicle_fx_set`.
pub fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = object(params)?;
    check_fields(map, SET_FIELDS)?;
    // Validate everything before touching the state.
    let enabled = boolean(map, "enabled")?;
    let clear = boolean(map, "clear_marks")?;
    let surface = match map.get("default_surface") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let name = v
                .as_str()
                .ok_or_else(|| invalid("invalid_value", "`default_surface` must be a string"))?;
            Some(WheelSurface::from_name(name).ok_or_else(|| {
                invalid(
                    "unknown_surface",
                    format!(
                        "`default_surface` {name}: one of {}",
                        WheelSurface::ALL.map(WheelSurface::name).join(", ")
                    ),
                )
            })?)
        }
    };
    let spacing = number(map, "spacing", (0.05, 5.0))?;
    let max_turn = number(map, "max_turn", (0.01, 1.0))?;
    let smoke = number(map, "smoke_slip_threshold", (0.01, 0.95))?;
    let skid = number(map, "skid_slip_threshold", (0.01, 2.0))?;
    let min_speed = number(map, "min_speed", (0.0, 20.0))?;
    let life = number(map, "mark_lifetime_s", (0.5, 3600.0))?;
    let cap = number(map, "max_stamps", (0.0, MAX_STAMP_CAP as f64))?;
    let scale = number(map, "effect_scale", (0.0, 2.0))?;
    let mut state = world.get_resource_mut::<VehicleFxState>().ok_or_else(missing)?;
    let before = state_json(&state);
    let mut config = state.config();
    if let Some(v) = spacing {
        config.spacing = v;
    }
    if let Some(v) = max_turn {
        config.max_turn = v;
    }
    if let Some(v) = smoke {
        config.smoke_slip_threshold = v;
    }
    if let Some(v) = skid {
        config.skid_slip_threshold = v;
    }
    if let Some(v) = min_speed {
        config.min_speed = v;
    }
    if let Some(v) = life {
        config.mark_lifetime_s = v;
    }
    if let Some(v) = cap {
        config.limits.max_stamps = v as usize;
    }
    if let Some(v) = scale {
        config.limits.effect_scale = v;
    }
    state.set_config(config);
    if let Some(v) = enabled {
        state.enabled = v;
    }
    if let Some(v) = surface {
        state.default_surface = v;
    }
    if clear == Some(true) {
        state.clear_marks();
    }
    Ok(json!({ "before": before, "after": state_json(&state) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::vehicle_fx::VehicleFxPlugin;

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn without_the_plugin_both_methods_say_so() {
        let mut world = World::new();
        assert_eq!(
            reason(&read(&mut world, &json!({})).expect_err("no state")),
            "no_vehicle_fx"
        );
        assert_eq!(
            reason(&set(&mut world, &json!({})).expect_err("no state")),
            "no_vehicle_fx"
        );
    }

    #[test]
    fn the_set_method_validates_then_applies() {
        let mut app = App::new();
        app.add_plugins(VehicleFxPlugin);
        let world = app.world_mut();
        let r = read(world, &json!({})).expect("read");
        assert_eq!(r["enabled"], true);
        assert_eq!(r["count"], 0);
        assert_eq!(r["stamps"]["live"], 0);
        let bad = set(world, &json!({ "spacing": 0.5, "max_turn": 9.0 })).expect_err("range");
        assert_eq!(reason(&bad), "invalid_value");
        // Every refusal points at this module's page, not another module's.
        let field = set(world, &json!({ "nope": 1 })).expect_err("field");
        for err in [&bad, &field] {
            let hint = err
                .data
                .as_ref()
                .and_then(|d| d.get("hint"))
                .and_then(Value::as_str);
            assert_eq!(hint, Some("see book/agents/vehicle-fx.md"));
        }
        // The failed call changed nothing.
        assert_eq!(read(world, &json!({})).expect("read")["config"]["spacing"], 0.5);
        assert_eq!(
            reason(&set(world, &json!({ "nope": 1 })).expect_err("field")),
            "unknown_field"
        );
        assert_eq!(
            reason(&set(world, &json!({ "default_surface": "lava" })).expect_err("surface")),
            "unknown_surface"
        );
        let out = set(
            world,
            &json!({ "spacing": 1.0, "max_stamps": 10, "enabled": false, "default_surface": "gravel" }),
        )
        .expect("set");
        assert_eq!(out["before"]["config"]["spacing"], 0.5);
        assert_eq!(out["after"]["config"]["spacing"], 1.0);
        assert_eq!(out["after"]["stamps"]["cap"], 10);
        assert_eq!(out["after"]["enabled"], false);
        assert_eq!(out["after"]["default_surface"], "gravel");
    }
}
