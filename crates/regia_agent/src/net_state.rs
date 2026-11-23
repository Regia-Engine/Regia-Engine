// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the state channel and the send loop's scale (spec 72 §§ 4, 7, 14;
//! `book/agents/net-state-channel.md`, ADR-1950): `net.state_channel`.
//!
//! Errors are `F0002` with `data.reason`: `net_not_running`, `peer_unknown`.

use regia_app::prelude::*;
use regia_net::model::{NetMode, PeerId};
use regia_net::replication::{ClientReplication, ServerReplication};
use regia_net::runtime::NetNode;
use regia_net::{MIN_NET_PROTOCOL_VERSION, NET_PROTOCOL_VERSION, NetSettings, StateChannel};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

/// `net.state_channel`: how state travels. On a server: the settings, the last send tick's wall
/// time by phase and, per connection, its wire and packet counters. On a client: what it received.
pub(crate) fn state_channel(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net runtime",
            "add regia_net::NetPlugin (or call regia_net::runtime::ensure_resources) and set a NetMode",
        ));
    }
    let only = match params.get("peer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .map(PeerId)
                .ok_or_else(|| RpcError::invalid_params("`peer` must be a peer id (an integer)"))?,
        ),
    };
    let settings = world
        .get_resource::<NetSettings>()
        .map(|s| s.replication.clone())
        .unwrap_or_default();
    let is_server = world
        .get_resource::<NetMode>()
        .copied()
        .unwrap_or_default()
        .is_server();
    let mut out = json!({
        "protocol": NET_PROTOCOL_VERSION,
        "min_protocol": MIN_NET_PROTOCOL_VERSION,
        "state_channel": match settings.state_channel {
            StateChannel::Unreliable => "unreliable",
            StateChannel::Reliable => "reliable",
        },
        "send_every_ticks": settings.send_every_ticks.max(1),
        "client_rate_bytes": settings.client_rate_bytes,
        "server_rate_bytes": settings.server_rate_bytes,
        "mtu_bytes": settings.mtu_bytes,
        // Spec 120 § 8.2, ADR-8467: the quantized transform record this build speaks.
        "transform_record": {
            "revision": regia_net::replication::TRANSFORM_RECORD_REVISION,
            "position_cell_bits": regia_net::replication::POSITION_CELL_BITS,
            "default_step_m": regia_net::replication::DEFAULT_WIRE_STEP_M,
            "velocity_step_mps": regia_net::replication::VELOCITY_STEP_MPS,
            // ADR-8522: the position predictor's assumed rate and the gate's rotation half.
            "prediction_tick_hz": regia_net::replication::PREDICTION_TICK_HZ,
            "gate_rotation_deg": regia_net::profile::EXTRAPOLATION_GATE_DEG,
            // ADR-10124: the far-tier record (state record kind 3) and its coarse steps.
            "far_record_bits": regia_net::replication::FAR_RECORD_BITS,
            "far_xz_shift": regia_net::replication::FAR_XZ_SHIFT,
            "far_y_shift": regia_net::replication::FAR_Y_SHIFT,
        },
    });
    if is_server {
        let node = world.resource::<NetNode>();
        let peers = node.peers();
        if let Some(p) = only
            && !peers.contains(&p)
        {
            return Err(f0002(
                "peer_unknown",
                format!("no connected peer {}", p.0),
                "net.status lists the connected peers",
            ));
        }
        let rep = world.get_resource::<ServerReplication>();
        let rows: Vec<Value> = peers
            .iter()
            .filter(|p| only.is_none_or(|o| o == **p))
            .filter_map(|p| {
                let link = rep?.state_link(*p)?;
                let c = link.counters;
                Some(json!({
                    "peer": p.0,
                    "wire": link.wire.as_str(),
                    "packets_sent": c.packets_sent,
                    "packets_acked": c.packets_acked,
                    "packets_lost": c.packets_lost,
                    "records_sent": c.records_sent,
                    "baselines_expired": c.baselines_expired,
                    "oversize_records": c.oversize_records,
                    // Spec 120 § 8.2, ADR-8522: candidates the extrapolation gate held back.
                    "gate_held": c.gate_held,
                    // Spec 120 § 8.3, ADR-10124: far-tier updates sent as far records.
                    "far_records": c.far_records,
                    // Spec 120 § 8.1, ADR-8419: the measured state round trip and what follows it.
                    "rtt_ticks": link.rtt_ticks(),
                    "rtt_samples": c.rtt_samples,
                    "in_flight_timeout_ticks": link.in_flight_timeout_ticks(),
                    "log_frames": link.log_frames(),
                    "oversize_pending": link.oversize_pending(),
                }))
            })
            .collect();
        let t = rep.map(|r| r.timing).unwrap_or_default();
        out["role"] = json!("server");
        out["peers"] = json!(rows);
        out["last_send_ms"] = json!({
            "changes": t.changes_ms,
            "relevance": t.relevance_ms,
            "priority_and_packing": t.send_ms,
            "relevance_shared": t.relevance_shared_ms,
            "walk_cpu": t.walk_cpu_ms,
            "priority_cpu": t.priority_cpu_ms,
            "pack_cpu": t.pack_cpu_ms,
            "commit_cpu": t.commit_cpu_ms,
        });
        out["ordered_candidates"] = json!(t.ordered_candidates);
        // Spec 120 § 8.3, ADR-10124: whether far-tier updates may travel as far records.
        out["far_records"] = json!(rep.is_none_or(ServerReplication::far_records));
        // ADR-10199: a far copy whose entity stood this long gets its exact transform.
        out["far_settle_seconds"] = json!(regia_net::replication::FAR_SETTLE_SECONDS);
        // Entities over the 16 KiB entity-state cap, withheld from every connection (ADR-8419).
        out["state_too_large"] = json!(rep.map_or(0, |r| r.counters.state_too_large));
        out["too_large_entities"] = json!(
            rep.map(|r| r.too_large_entities().iter().map(|id| id.0).collect::<Vec<u32>>())
                .unwrap_or_default()
        );
    } else {
        let state = world.get_resource::<ClientReplication>().map(|c| &c.state);
        let (entities, versions) = state.map_or((0, 0), |s| s.stored());
        let c = state.map(|s| s.counters).unwrap_or_default();
        out["role"] = json!("client");
        out["on_state_channel"] = json!(state.is_some_and(|s| s.active()));
        out["packets"] = json!(c.packets);
        out["duplicates"] = json!(c.duplicates);
        out["records_applied"] = json!(c.applied);
        out["records_stored_only"] = json!(c.stored_only);
        out["baselines_missing"] = json!(c.baseline_missing);
        out["record_errors"] = json!(c.record_errors);
        out["records_tombstoned"] = json!(c.tombstoned);
        out["entities_stored"] = json!(entities);
        out["versions_stored"] = json!(versions);
        // ADR-10051: the version ring's cap, the longest ring, the bytes the stored versions take
        // and the deltas of frames reordered past the margin (taken as lost, no resync).
        out["version_ring"] = json!(regia_net::replication::STATE_VERSION_RING);
        out["longest_ring"] = json!(state.map_or(0, |s| s.longest_ring()));
        out["version_bytes_stored"] = json!(state.map_or(0, |s| s.stored_bytes()));
        out["records_retired_baseline"] = json!(state.map_or(0, |s| s.retired_baselines()));
        // ADR-8760: the client entity cap and what it did.
        let (_, tombstones, ownership_held) = state.map_or((0, 0, 0), |s| s.map_lens());
        out["entity_cap"] = json!(state.map_or(regia_net::replication::MAX_CLIENT_ENTITIES, |s| s.cap()));
        out["tombstones"] = json!(tombstones);
        out["ownership_held"] = json!(ownership_held);
        out["spawns_over_cap"] = json!(c.over_cap);
        out["cap_evicted"] = json!(c.cap_evicted);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::prelude::Component;
    use regia_net::ReplicateOptions;
    use regia_net::replication::{Replicated, register_replicated};
    use regia_net::testing::LoopbackHub;

    #[derive(Component, Clone, Debug, PartialEq, Default, regia_net::Replicate)]
    struct Hp(u32);

    fn setup(world: &mut World) {
        register_replicated::<Hp>(world, ReplicateOptions::default());
        world.resource_mut::<NetSettings>().replication.state_channel = StateChannel::Unreliable;
    }

    /// Spec 72 § 13 test 13 for this method: both roles round-trip and each reason is produced.
    #[test]
    fn state_channel_reports_both_sides_and_every_reason() {
        let mut bare = World::new();
        let e = state_channel(&mut bare, &json!({})).expect_err("no runtime");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("net_not_running"))
        );

        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 3, setup);
        let a = hub.add_client();
        hub.server.world.spawn((Replicated::default(), Hp(1)));
        hub.run(8);
        let peer = hub.clients[a].peer;
        let s = state_channel(&mut hub.server.world, &json!({ "peer": peer.0 })).expect("server");
        assert_eq!(s["role"], "server");
        assert_eq!(s["protocol"], NET_PROTOCOL_VERSION);
        assert_eq!(s["min_protocol"], MIN_NET_PROTOCOL_VERSION);
        assert_eq!(s["state_channel"], "unreliable");
        // ADR-8467: the transform record revision and its grids.
        assert_eq!(s["transform_record"]["revision"], 4);
        // ADR-10124: the far-tier record, on by default, and its per-peer counter.
        assert_eq!(s["transform_record"]["far_record_bits"], 40);
        assert_eq!(s["transform_record"]["far_xz_shift"], 5);
        assert_eq!(s["transform_record"]["far_y_shift"], 10);
        assert_eq!(s["far_records"], true);
        assert_eq!(s["far_settle_seconds"], 2.0);
        assert_eq!(s["peers"][0]["far_records"], 0);
        assert_eq!(s["transform_record"]["prediction_tick_hz"], 60.0);
        assert_eq!(s["transform_record"]["gate_rotation_deg"], 0.5);
        assert_eq!(s["transform_record"]["position_cell_bits"], 20);
        assert_eq!(s["transform_record"]["velocity_step_mps"], 0.03125);
        assert_eq!(s["peers"][0]["wire"], "acked_deltas");
        assert!(s["peers"][0]["packets_sent"].as_u64().is_some_and(|n| n > 0));
        assert!(s["peers"][0]["records_sent"].as_u64().is_some_and(|n| n >= 1));
        assert!(s["last_send_ms"]["relevance"].is_number());
        assert!(s["ordered_candidates"].is_number());
        // ADR-8419: the measured state round trip and the timeout that follows it.
        assert!(s["peers"][0]["rtt_samples"].as_u64().is_some_and(|n| n >= 1));
        assert!(s["peers"][0]["rtt_ticks"].as_f64().is_some_and(|r| r >= 1.0));
        assert!(
            s["peers"][0]["in_flight_timeout_ticks"]
                .as_u64()
                .is_some_and(|t| (3..30).contains(&t)),
            "a loopback round trip gives a short timeout: {}",
            s["peers"][0]["in_flight_timeout_ticks"]
        );
        assert!(s["peers"][0]["log_frames"].as_u64().is_some_and(|n| n >= 1));
        assert_eq!(s["peers"][0]["oversize_pending"], false);
        assert_eq!(s["state_too_large"], 0);
        assert_eq!(s["too_large_entities"], json!([]));

        let c = state_channel(&mut hub.clients[a].world, &json!({})).expect("client");
        assert_eq!(c["role"], "client");
        assert_eq!(c["on_state_channel"], true);
        assert!(c["records_applied"].as_u64().is_some_and(|n| n >= 1));
        assert_eq!(c["entities_stored"], 1);
        // ADR-10051: the version ring.
        assert_eq!(c["version_ring"], 512);
        assert!(c["longest_ring"].as_u64().is_some_and(|n| (1..=512).contains(&n)));
        assert!(c["version_bytes_stored"].as_u64().is_some_and(|n| n > 0));
        assert_eq!(c["records_retired_baseline"], 0);
        // ADR-8760: the cap and its counters.
        assert_eq!(c["entity_cap"], regia_net::replication::MAX_CLIENT_ENTITIES);
        assert_eq!(c["spawns_over_cap"], 0);
        assert_eq!(c["cap_evicted"], 0);
        assert_eq!(c["tombstones"], 0);
        assert_eq!(c["ownership_held"], 0);

        let e = state_channel(&mut hub.server.world, &json!({ "peer": 99 })).expect_err("unknown");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("peer_unknown"))
        );
        assert!(state_channel(&mut hub.server.world, &json!({ "peer": "x" })).is_err());
    }
}
