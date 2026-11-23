// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of networked hover hulls (`book/agents/net-hover.md`, ADR-8819):
//! `net.hovers`.
//!
//! Errors are `F0002` with `data.reason` set to `net_not_running`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::net_hover::{
    NetHoverStats, net_hover_corrections, net_hover_rows, net_hovers_installed,
};
use regia_net::predict::PredictQueryError;
use serde_json::{Value, json};

use crate::net_predict::err;
use crate::protocol::RpcError;

fn vec3(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

/// `net.hovers`: every networked hover hull in this process, the step's counters and, on a client,
/// the correction counters.
pub(crate) fn hovers(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    if !net_hovers_installed(world) {
        return Err(err(PredictQueryError::NotRunning));
    }
    let rows: Vec<Value> = net_hover_rows(world)
        .into_iter()
        .map(|r| {
            json!({
                "entity": r.entity.to_bits(),
                "net_id": r.net_id.map(|id| id.0),
                "role": r.role,
                "hull_key": format!("{:016x}", r.hull_key),
                "hull_known": r.hull_known,
                "position": vec3(r.position),
                "rendered_position": vec3(r.rendered_position),
                "velocity": vec3(r.velocity),
                "yaw": r.yaw,
                "altitude": r.altitude,
                "airborne": r.airborne,
                "fault": r.fault,
            })
        })
        .collect();
    let s = world.get_resource::<NetHoverStats>().copied().unwrap_or_default();
    let corrections = net_hover_corrections(world).map(|(corrections, snaps, last)| {
        json!({ "corrections": corrections, "snaps": snaps, "last_correction_m": last })
    });
    Ok(json!({
        "hovers": rows,
        "stats": {
            "steps": s.steps,
            "resim_steps": s.resim_steps,
            "hops": s.hops,
            "jumps": s.jumps,
            "recovered": s.recovered,
            "unknown_hull": s.unknown_hull,
            "adopted_moves": s.adopted_moves,
            "dt_mismatch": s.dt_mismatch,
        },
        "prediction": corrections,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::det_hover::{DetHoverHull, DetHoverState};
    use regia_framework::net_hover::{HoverNetInput, NetHover, install_net_hover};
    use regia_net::replication::NetPredicted;
    use regia_net::testing::LoopbackHub;
    use regia_net::{NetMode, NetOwner, PeerId, Replicated, TransformReplication};
    use regia_scene::Transform;

    fn setup(world: &mut World) {
        install_net_hover::<HoverNetInput>(world).expect("net hover");
    }

    /// The method round-trips on both sides and its error is produced.
    #[test]
    fn net_hovers_round_trips_and_its_reason() {
        let mut bare = World::new();
        let e = hovers(&mut bare, &json!({})).expect_err("nothing installed");
        let reason = e.data.as_ref().and_then(|d| d["reason"].as_str());
        assert_eq!(reason, Some("net_not_running"));

        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
        let a = hub.add_client();
        hub.down.delay_ticks = 2;
        hub.up.delay_ticks = 2;
        regia_net::predict::set_transport_rtt(&mut hub.clients[a].world, 5.0 * 1000.0 / 60.0, 0.0);
        let hull = DetHoverHull::DEFAULT;
        hub.server.world.spawn((
            Replicated {
                transform: TransformReplication::Full,
                ..Replicated::default()
            },
            Transform::default(),
            hull,
            NetHover::for_hull(&hull, 0),
            DetHoverState::at_rest([0.0, 0.0, 0.0], 0.0, &hull.params),
            NetPredicted,
            NetOwner(PeerId(1)),
        ));
        hub.run(60);
        let server = hovers(&mut hub.server.world, &json!({})).expect("server");
        assert_eq!(server["hovers"][0]["role"], "authority");
        assert_eq!(server["hovers"][0]["fault"], Value::Null);
        assert!(server["stats"]["steps"].as_u64().is_some_and(|n| n > 0));
        assert_eq!(server["prediction"], Value::Null);
        let client = hovers(&mut hub.clients[a].world, &json!({})).expect("client");
        assert_eq!(client["hovers"][0]["role"], "predicted");
        assert_eq!(client["hovers"][0]["hull_known"], true);
        assert_eq!(client["prediction"]["corrections"], 0);
        assert_eq!(client["hovers"][0]["hull_key"], server["hovers"][0]["hull_key"]);
    }
}
