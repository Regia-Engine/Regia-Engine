// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of spec 74 (proxy interpolation; `book/agents/net-interpolation.md`,
//! ADR-1662): `net.interpolation`, `net.proxy_timeline`, `net.set_interp_delay`.
//!
//! Errors are `F0002` with `data.reason` set to one of: `net_not_client`, `entity_not_proxy`,
//! `delay_out_of_range`, `net_not_running`.

use regia_app::prelude::*;
use regia_net::interp::{InterpQueryError, interpolation_report, proxy_timeline, set_interp_delay};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn err(e: InterpQueryError) -> RpcError {
    let (message, hint) = match e {
        InterpQueryError::NotClient => (
            "interpolation runs on clients; this process is a server or offline",
            "call it on a client process",
        ),
        InterpQueryError::NotProxy => (
            "the entity is not an interpolated proxy",
            "net.replicated lists the copies; owned and predicted copies are not interpolated",
        ),
        InterpQueryError::NotRunning => (
            "this client has no interpolation running (not installed, or no state received yet)",
            "add regia_net::NetInterpolationPlugin (or call regia_net::interp::install_interpolation)",
        ),
        InterpQueryError::DelayOutOfRange => {
            ("the delay is outside 0..1000 ms", "pass ms in 0..=1000 or null")
        }
    };
    let mut r = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = r.data.as_mut() {
        data.insert("reason".into(), json!(e.code()));
    }
    r
}

/// `net.interpolation`: the client's interpolation clock and proxy state counts.
pub(crate) fn interpolation(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let r = interpolation_report(world).map_err(err)?;
    serde_json::to_value(r).map_err(|e| RpcError::internal(format!("net.interpolation: {e}")))
}

/// `net.proxy_timeline`: one proxy's buffered ticks, bracket, blend parameter and state.
pub(crate) fn timeline(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let e = Entity::try_from_bits(bits).filter(|e| world.get_entity(*e).is_ok());
    // A missing entity is not a proxy either; the client check comes first.
    let t = match e {
        Some(e) => proxy_timeline(world, e).map_err(err)?,
        None => {
            interpolation_report(world).map_err(err)?;
            return Err(err(InterpQueryError::NotProxy));
        }
    };
    Ok(json!({
        "buffered": t.buffered,
        "bracket": t.bracket.map(|(a, b)| json!([a, b])),
        "t": t.t,
        "state": t.state,
        "update_interval_ticks": t.update_interval_ticks,
    }))
}

/// `net.set_interp_delay`: fixes the interpolation delay (dev builds); `null` returns to adaptive.
pub(crate) fn set_delay(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    set_delay_gated(world, params, crate::net_model::SET_OWNER_AVAILABLE)
}

/// [`set_delay`] with the build's dev-build answer as a parameter, so a test sees the shipping
/// refusal (`net_dev_only`, spec 74 § 12 "client, dev builds", ADR-5993).
fn set_delay_gated(world: &mut World, params: &Value, dev_build: bool) -> Result<Value, RpcError> {
    crate::net_model::dev_only_gate(
        dev_build,
        "net.set_interp_delay",
        "run a development build; a shipping game sets the delay through regia_net::InterpolationSettings",
    )?;
    let ms = match params.get("ms") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_f64()
                .ok_or_else(|| RpcError::invalid_params("`ms` must be a number of milliseconds or null"))?,
        ),
    };
    let delay = set_interp_delay(world, ms).map_err(err)?;
    Ok(json!({ "delay_ms": delay }))
}

/// `net.anim_state` (ADR-2551): the replicated animation state of one entity: on the server what
/// it captured, on a client what the interpolation shows and the tick the rig's animator was last
/// re-anchored on. Errors: `net_not_running`, `entity_not_found`, `entity_not_anim_replicated`.
pub(crate) fn anim_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_framework::net_character::{NetAnimQueryError, net_anim_report};
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    let report = Entity::try_from_bits(bits)
        .ok_or(NetAnimQueryError::NotFound)
        .and_then(|e| net_anim_report(world, e))
        .map_err(|e| {
            let (message, hint) = match e {
                NetAnimQueryError::NotRunning => (
                    "the app has no regia_net runtime",
                    "add regia_net::NetPlugin and set a NetMode",
                ),
                NetAnimQueryError::NotFound => ("the entity does not exist", "net.replicated lists the copies"),
                NetAnimQueryError::NotAnimated => (
                    "the entity carries no NetAnimState",
                    "add NetAnimState to the replicated root and regia_framework::net_character::NetAnimationPlugin on every process",
                ),
            };
            let mut r = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
            if let Some(Value::Object(data)) = r.data.as_mut() {
                data.insert("reason".into(), json!(e.code()));
            }
            r
        })?;
    Ok(json!({
        "entity": bits,
        "role": report.role,
        "tick": report.tick,
        "past": report.past,
        "shown_tick": report.shown_tick,
        "words": report.words,
        "params": report.params,
        "rig": report.rig,
        "rig_state": report.rig_state,
        "refused": report.refused,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_net::interp::{NetInterpolation, install_interpolation};
    use regia_net::model::NetMode;
    use regia_net::runtime::NetEntities;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetId, Replicated, net_interpolate};
    use regia_scene::transform::Transform;

    fn setup(_: &mut World) {}

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    /// Spec 74 § 13 test 11: every method round-trips and each error code is produced.
    #[test]
    fn net_interp_methods_round_trip_and_every_reason() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 4, setup);
        let a = hub.add_client();
        let mover = hub
            .server
            .world
            .spawn((Replicated::default(), Transform::default()))
            .id();
        // Before installing: not running.
        let err = interpolation(&mut hub.clients[a].world, &json!({})).expect_err("not installed");
        assert_eq!(reason(&err).as_deref(), Some("net_not_running"));
        install_interpolation(&mut hub.clients[a].world, 60.0, 1.0);
        for step in 1..=40u32 {
            let now = f64::from(step) * 1000.0 / 60.0;
            hub.clients[a].world.resource_mut::<NetInterpolation>().now_ms = now;
            if let Some(mut t) = hub.server.world.get_mut::<Transform>(mover) {
                t.translation = Vec3::new(step as f32 * 0.1, 0.0, 0.0);
            }
            hub.step();
            net_interpolate(&mut hub.clients[a].world, now);
        }
        let id = *hub.server.world.get::<NetId>(mover).expect("id");
        let cw = &mut hub.clients[a].world;
        let copy = cw.resource::<NetEntities>().entity(id).expect("copy");
        let r = interpolation(cw, &json!({})).expect("interpolation");
        assert!(r["interp_tick"].as_f64().is_some_and(|t| t > 0.0));
        assert!(r["delay_ms"].as_f64().is_some());
        let t = timeline(cw, &json!({ "entity": copy.to_bits() })).expect("timeline");
        assert!(t["buffered"].as_array().is_some_and(|b| !b.is_empty()));
        assert_eq!(t["state"], "interpolating");
        let d = set_delay(cw, &json!({ "ms": 80.0 })).expect("fixed");
        assert_eq!(d["delay_ms"], 80.0);
        assert!(set_delay(cw, &json!({ "ms": null })).is_ok());

        // Errors.
        let err = set_delay(cw, &json!({ "ms": 1500.0 })).expect_err("range");
        assert_eq!(reason(&err).as_deref(), Some("delay_out_of_range"));
        let local = cw.spawn_empty().id();
        let err = timeline(cw, &json!({ "entity": local.to_bits() })).expect_err("not proxy");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_proxy"));
        let err = interpolation(&mut hub.server.world, &json!({})).expect_err("server");
        assert_eq!(reason(&err).as_deref(), Some("net_not_client"));
    }

    /// ADR-2551: `net.anim_state` round-trips on a server and produces each of its reasons.
    #[test]
    fn anim_state_round_trips_and_every_reason() {
        use regia_framework::net_character::NetAnimState;
        let mut w = World::new();
        let err = anim_state(&mut w, &json!({ "entity": 1 })).expect_err("no runtime");
        assert_eq!(reason(&err).as_deref(), Some("net_not_running"));
        w.insert_resource(NetMode::DedicatedServer);
        regia_net::runtime::ensure_resources(&mut w);
        let e = w
            .spawn(NetAnimState {
                tick: 5,
                words: vec![1, 1, 0, 0],
                params: vec![2.5f32.to_bits(), 1],
                blend_mask: 0b01,
                past: 0.0,
            })
            .id();
        let r = anim_state(&mut w, &json!({ "entity": e.to_bits() })).expect("anim state");
        assert_eq!(r["role"], "server");
        assert_eq!(r["tick"], 5);
        assert_eq!(r["words"], 4);
        assert_eq!(r["params"], json!([2.5, 1.0]));
        assert_eq!(r["rig"], Value::Null);
        let bare = w.spawn_empty().id();
        let err = anim_state(&mut w, &json!({ "entity": bare.to_bits() })).expect_err("no state");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_anim_replicated"));
        w.despawn(bare);
        let err = anim_state(&mut w, &json!({ "entity": bare.to_bits() })).expect_err("gone");
        assert_eq!(reason(&err).as_deref(), Some("entity_not_found"));
        assert!(anim_state(&mut w, &json!({})).is_err_and(|e| reason(&e).is_none()));
    }

    /// Spec 74 § 12 lists `net.set_interp_delay` for dev builds only (ADR-5993, like `net.set_owner`):
    /// a shipping build refuses it with `net_dev_only` before it reads a parameter or the world.
    #[test]
    fn set_interp_delay_is_refused_in_a_shipping_build() {
        let mut world = World::new();
        let err = set_delay_gated(&mut world, &json!({ "ms": 100 }), false).expect_err("shipping build");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let data = err.data.expect("data");
        assert_eq!(data["reason"], "net_dev_only", "{data}");
        assert!(err.message.contains("net.set_interp_delay"), "{}", err.message);
    }
}
