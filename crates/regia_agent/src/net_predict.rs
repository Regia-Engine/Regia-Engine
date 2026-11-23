// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of spec 73 (client prediction and reconciliation;
//! `book/agents/net-prediction.md`, ADR-1793): `net.prediction`, `net.input_buffer`,
//! `net.prediction_history`, `net.force_correction`.
//!
//! Errors are `F0002` with `data.reason` set to one of: `net_not_client`, `net_not_server`,
//! `entity_not_predicted`, `peer_unknown`, `net_not_running`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_net::predict::{
    PredictQueryError, force_correction, input_buffer_report, prediction_history, prediction_report,
};
use regia_net::session::PeerId;
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

pub(crate) fn err(e: PredictQueryError) -> RpcError {
    let (message, hint) = match e {
        PredictQueryError::NotClient => (
            "prediction state lives on clients; this process is a server or offline",
            "call it on a client process",
        ),
        PredictQueryError::NotServer => (
            "the input buffer and forced corrections live on the server; this process is a client",
            "call it on the server process",
        ),
        PredictQueryError::EntityNotPredicted => (
            "the entity is not predicted here",
            "on a client pass a NetRole::Predicted copy; on the server an entity with NetPredicted and a Transform",
        ),
        PredictQueryError::PeerUnknown => ("no connected peer has that id", "net.peers lists the peers"),
        PredictQueryError::NotRunning => (
            "prediction is not running (no input type registered, or no server state received yet)",
            "register the input with regia_net::add_net_input and predicted components with regia_net::predict",
        ),
    };
    let mut r = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = r.data.as_mut() {
        data.insert("reason".into(), json!(e.as_str()));
    }
    r
}

/// `net.prediction`: the client's clock and correction counters.
pub(crate) fn prediction(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let r = prediction_report(world).map_err(err)?;
    let dilation_lag = world
        .get_resource::<regia_net::predict::ClientPrediction>()
        .map_or(0.0, |cp| cp.dilation_lag());
    Ok(json!({
        "client_tick": r.client_tick,
        "lead": r.lead,
        "rtt_ms": r.rtt_ms,
        "jitter_ms": r.jitter_ms,
        "margin": r.margin,
        "corrections_per_s": r.corrections_per_s,
        "max_error_m": r.stats.max_error_m,
        "resim_ticks_last": r.stats.resim_ticks_last,
        "resets": r.stats.resets,
        "rejected_spawns": r.stats.rejected_spawns,
        "corrections": r.stats.corrections,
        "resyncs": r.stats.resyncs,
        "teleports": r.stats.teleports,
        "bound_spawns": r.stats.bound_spawns,
        "deferred": r.stats.deferred,
        "dilated": r.stats.dilated,
        "dilation_lag": dilation_lag,
        "resim_depth": r.resim_depth,
        "snaps": r.stats.snaps,
        "last_correction_m": r.stats.last_correction_m,
    }))
}

/// `net.input_buffer`: per player of a peer, the server's input stream counters.
pub(crate) fn input_buffer(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let peer = crate::params::opt_u64(params, "peer")?
        .and_then(|p| u32::try_from(p).ok())
        .ok_or_else(|| RpcError::invalid_params("`peer` (peer id) is required"))?;
    let rows = input_buffer_report(world, PeerId(peer)).map_err(err)?;
    let players: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "local": r.local,
                "newest_tick": r.stats.newest_tick.map(|t| t.0),
                "margin": r.stats.margin,
                "missing": r.stats.missing,
                "late": r.stats.late,
                "early": r.stats.early,
                "flood": r.stats.flood,
                "trusted": r.stats.trusted,
                "buffered": r.buffered,
            })
        })
        .collect();
    Ok(json!({ "players": players }))
}

fn entity_param(world: &World, params: &Value) -> Result<Option<Entity>, RpcError> {
    let bits = params
        .get("entity")
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params("`entity` (entity bits) is required"))?;
    Ok(Entity::try_from_bits(bits).filter(|e| world.get_entity(*e).is_ok()))
}

/// `net.prediction_history`: an entity's predicted values per tick and whether the server
/// confirmed them.
pub(crate) fn history(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let e = entity_param(world, params)?;
    let ticks = crate::params::opt_u64(params, "ticks")?.map(|t| usize::try_from(t).unwrap_or(usize::MAX));
    let rows = match e {
        Some(e) => prediction_history(world, e, ticks).map_err(err)?,
        None => {
            // The client check comes first; a missing entity is not predicted either.
            prediction_report(world).map_err(err)?;
            return Err(err(PredictQueryError::EntityNotPredicted));
        }
    };
    let rows: Vec<Value> = rows
        .iter()
        .map(|r| {
            let values: serde_json::Map<String, Value> = r
                .values
                .iter()
                .map(|(k, v)| ((*k).to_string(), json!(v)))
                .collect();
            json!({ "tick": r.tick, "values": values, "confirmed": r.confirmed })
        })
        .collect();
    Ok(json!({ "ticks": rows }))
}

/// `net.force_correction`: moves the server copy of a predicted entity (dev builds).
pub(crate) fn force(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    force_gated(world, params, crate::net_model::SET_OWNER_AVAILABLE)
}

/// [`force`] with the build's dev-build answer as a parameter, so a test sees the shipping
/// refusal (`net_dev_only`, spec 73 § 12 "server, dev builds", ADR-5993).
fn force_gated(world: &mut World, params: &Value, dev_build: bool) -> Result<Value, RpcError> {
    crate::net_model::dev_only_gate(
        dev_build,
        "net.force_correction",
        "run a development build; a shipping server moves entities through gameplay code",
    )?;
    let offset = crate::params::opt_array(params, "offset")?
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            let v: Vec<f32> = a.iter().filter_map(Value::as_f64).map(|f| f as f32).collect();
            (v.len() == 3 && v.iter().all(|f| f.is_finite())).then(|| Vec3::new(v[0], v[1], v[2]))
        })
        .ok_or_else(|| RpcError::invalid_params("`offset` must be [x, y, z] in meters"))?;
    let e = entity_param(world, params)?;
    let tick = match e {
        Some(e) => force_correction(world, e, offset).map_err(err)?,
        None => {
            if !world
                .get_resource::<regia_net::NetMode>()
                .is_some_and(|m| m.is_server())
            {
                return Err(err(PredictQueryError::NotServer));
            }
            return Err(err(PredictQueryError::EntityNotPredicted));
        }
    };
    Ok(json!({ "applied_tick": tick.0 }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::predict::{InputCommand, NetInput, set_transport_rtt};
    use regia_net::replication::NetPredicted;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetMode, NetOwner, Replicated, Tolerance};
    use regia_scene::transform::Transform;

    #[derive(Clone, Debug, PartialEq, Default, regia_net::Replicate)]
    struct Walk {
        x: i8,
    }

    impl NetInput for Walk {}

    fn step(
        cx: regia_net::PredictionCx,
        inputs: Query<&InputCommand<Walk>>,
        mut q: Query<(&mut Transform, Option<&regia_net::NetRole>), With<NetPredicted>>,
    ) {
        let x = inputs.iter().next().map_or(0, |c| c.value.x);
        for (mut t, role) in &mut q {
            if cx.simulates(role) {
                t.translation.x += f32::from(x) * 0.1;
            }
        }
    }

    fn setup(world: &mut World) {
        regia_net::add_net_input::<Walk>(world).expect("input");
        regia_net::predict::<Transform>(world, Tolerance::position(0.01)).expect("transform");
        regia_net::add_predicted_systems(world, step);
    }

    fn reason(e: &RpcError) -> Option<String> {
        e.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .map(str::to_string)
    }

    /// Spec 73 § 13 test 16: every method round-trips and each error code is produced.
    #[test]
    fn net_predict_methods_round_trip_and_every_reason() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
        let a = hub.add_client();
        hub.down.delay_ticks = 2;
        hub.up.delay_ticks = 2;
        set_transport_rtt(&mut hub.clients[a].world, 5.0 * 1000.0 / 60.0, 0.0);
        // Before any state: not running.
        let e = prediction(&mut hub.clients[a].world, &json!({})).expect_err("not started");
        assert_eq!(reason(&e).as_deref(), Some("net_not_running"));
        let hero = hub
            .server
            .world
            .spawn((
                Replicated::default(),
                Transform::default(),
                NetPredicted,
                NetOwner(hub.clients[a].peer),
            ))
            .id();
        hub.run(60);
        let r = prediction(&mut hub.clients[a].world, &json!({})).expect("prediction");
        assert!(r["client_tick"].as_u64().is_some_and(|t| t > 0));
        assert_eq!(r["corrections"], 0);
        // Spec 120 § 9.3 (ADR-8217): the dilation counters.
        assert!(r["dilated"].as_i64().is_some(), "{r}");
        assert!(
            r["dilation_lag"]
                .as_f64()
                .is_some_and(|l| (0.0..1.0).contains(&l)),
            "{r}"
        );
        let b = input_buffer(&mut hub.server.world, &json!({ "peer": 1 })).expect("buffer");
        assert_eq!(b["players"].as_array().map(Vec::len), Some(1));
        let applied = force(
            &mut hub.server.world,
            &json!({ "entity": hero.to_bits(), "offset": [0.0, 0.0, 1.0] }),
        )
        .expect("force");
        assert!(applied["applied_tick"].as_u64().is_some());
        hub.run(20);
        let r = prediction(&mut hub.clients[a].world, &json!({})).expect("prediction");
        assert_eq!(r["corrections"], 1);
        let copy = {
            let w = &mut hub.clients[a].world;
            let mut q = w.query::<(Entity, &regia_net::NetRole)>();
            q.iter(w)
                .find(|(_, r)| **r == regia_net::NetRole::Predicted)
                .map(|(e, _)| e)
                .expect("predicted copy")
        };
        let h = history(
            &mut hub.clients[a].world,
            &json!({ "entity": copy.to_bits(), "ticks": 4 }),
        )
        .expect("history");
        let ticks = h["ticks"].as_array().expect("rows");
        assert!(!ticks.is_empty() && ticks.len() <= 4);
        assert!(ticks[0]["values"].as_object().is_some_and(|v| !v.is_empty()));

        // Errors.
        let e = prediction(&mut hub.server.world, &json!({})).expect_err("server");
        assert_eq!(reason(&e).as_deref(), Some("net_not_client"));
        let e = input_buffer(&mut hub.clients[a].world, &json!({ "peer": 1 })).expect_err("client");
        assert_eq!(reason(&e).as_deref(), Some("net_not_server"));
        let e = input_buffer(&mut hub.server.world, &json!({ "peer": 99 })).expect_err("peer");
        assert_eq!(reason(&e).as_deref(), Some("peer_unknown"));
        let plain = hub.clients[a].world.spawn_empty().id();
        let e = history(&mut hub.clients[a].world, &json!({ "entity": plain.to_bits() })).expect_err("plain");
        assert_eq!(reason(&e).as_deref(), Some("entity_not_predicted"));
        let e = force(
            &mut hub.clients[a].world,
            &json!({ "entity": plain.to_bits(), "offset": [0, 0, 1] }),
        )
        .expect_err("client");
        assert_eq!(reason(&e).as_deref(), Some("net_not_server"));
        assert!(force(&mut hub.server.world, &json!({ "entity": hero.to_bits() })).is_err());
    }

    /// Spec 73 § 12 lists `net.force_correction` for dev builds only (ADR-5993, like `net.set_owner`):
    /// a shipping build refuses it with `net_dev_only` before it reads a parameter or the world.
    #[test]
    fn force_correction_is_refused_in_a_shipping_build() {
        let mut world = World::new();
        let err = force_gated(
            &mut world,
            &json!({ "entity": 1, "offset": [0.0, 0.0, 1.0] }),
            false,
        )
        .expect_err("shipping build");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let data = err.data.expect("data");
        assert_eq!(data["reason"], "net_dev_only", "{data}");
        assert!(err.message.contains("net.force_correction"), "{}", err.message);
    }
}
