// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the entropy stage (spec 72 § 18, ADR-3315;
//! `book/agents/net-entropy.md`): `net.entropy_stats`.
//!
//! Errors are `F0002` with `data.reason`: `net_not_running`, `net_not_server`, `peer_unknown`.

use regia_app::prelude::*;
use regia_net::entropy::EntropyMode;
use regia_net::replication::ServerReplication;
use regia_net::runtime::NetNode;
use regia_net::{NetMode, NetSettings};
use serde_json::{Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

fn f0002(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut e = RpcError::new(INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = e.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    e
}

/// `net.entropy_stats`: the entropy stage's per-connection counters — how the configured mode
/// (config/net.ron's `replication.entropy`, or a dedicated host's `server.toml [net] entropy`)
/// negotiated, and the body bytes before and after the stage per connection. Server only: the
/// client has no counters to report.
pub(crate) fn entropy_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(f0002(
            "net_not_running",
            "the app has no regia_net runtime",
            "add regia_net::NetPlugin (or call regia_net::runtime::ensure_resources) and set a NetMode",
        ));
    }
    if !world
        .get_resource::<NetMode>()
        .copied()
        .unwrap_or_default()
        .is_server()
    {
        return Err(f0002(
            "net_not_server",
            "net.entropy_stats reads the server's per-connection stage counters",
            "run it on the server (net.status shows the side)",
        ));
    }
    let only = match params.get("peer") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|p| u32::try_from(p).ok())
                .map(regia_net::model::PeerId)
                .ok_or_else(|| RpcError::invalid_params("`peer` must be a peer id (an integer)"))?,
        ),
    };
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
    let configured = world
        .get_resource::<NetSettings>()
        .map(|s| s.replication.entropy)
        .unwrap_or_default();
    let host_override = world
        .get_resource::<regia_net::session::NetSession>()
        .filter(|s| s.is_dedicated())
        .and_then(|s| s.server())
        .and_then(|s| s.options().entropy);
    let rep = world.get_resource::<ServerReplication>();
    let rows: Vec<Value> = peers
        .iter()
        .filter(|p| only.is_none_or(|o| o == **p))
        .filter_map(|p| {
            let link = rep?.state_link(*p)?;
            let s = link.entropy_stats;
            Some(json!({
                "peer": p.0,
                "wire": link.wire.as_str(),
                "entropy": link.entropy.as_str(),
                "frames": s.frames,
                "coded": s.coded,
                "raw_fallback": s.raw_fallback,
                "bytes_in": s.bytes_in,
                "bytes_out": s.bytes_out,
                "saved": s.saved(),
            }))
        })
        .collect();
    Ok(json!({
        "protocol": regia_net::NET_PROTOCOL_VERSION,
        "min_protocol": regia_net::MIN_NET_PROTOCOL_VERSION,
        "configured": configured.as_str(),
        "server_toml_override": host_override.map(EntropyMode::as_str),
        "peers": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_macros::Replicate;
    use regia_net::replication::{ReplicateOptions, Replicated, register_replicated};
    use regia_net::testing::LoopbackHub;

    #[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
    struct Hp(u32);

    fn setup(world: &mut World) {
        register_replicated::<Hp>(world, ReplicateOptions::default());
        let mut settings = world.resource_mut::<NetSettings>();
        settings.replication.state_channel = regia_net::StateChannel::Unreliable;
        settings.replication.entropy = EntropyMode::Range;
    }

    /// Spec 72 § 18 acceptance: the method round-trips on a live server with an entropy-negotiated
    /// connection, and every error code is produced by a test.
    #[test]
    fn stats_report_a_live_entropy_connection_and_every_reason() {
        let mut bare = World::new();
        let e = entropy_stats(&mut bare, &json!({})).expect_err("no runtime");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("net_not_running"))
        );

        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 7, setup);
        let a = hub.add_client();
        hub.server.world.spawn((Replicated::default(), Hp(1)));
        hub.run(8);
        let peer = hub.clients[a].peer;
        let s = entropy_stats(&mut hub.server.world, &json!({ "peer": peer.0 })).expect("server");
        assert_eq!(s["protocol"], regia_net::NET_PROTOCOL_VERSION);
        assert_eq!(s["min_protocol"], regia_net::MIN_NET_PROTOCOL_VERSION);
        assert_eq!(s["configured"], "range");
        assert_eq!(s["server_toml_override"], Value::Null);
        let row = &s["peers"][0];
        assert_eq!(row["entropy"], "range", "the v3 test client negotiates entropy");
        assert!(row["frames"].as_u64().is_some_and(|n| n > 0));
        assert!(row["bytes_in"].as_u64().is_some_and(|n| n > 0));

        let e = entropy_stats(&mut hub.clients[a].world, &json!({})).expect_err("client");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("net_not_server"))
        );

        let e = entropy_stats(&mut hub.server.world, &json!({ "peer": 99 })).expect_err("unknown");
        assert_eq!(
            e.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("peer_unknown"))
        );
    }
}
