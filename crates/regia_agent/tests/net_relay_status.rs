// SPDX-License-Identifier: MPL-2.0
//! `net.relay_status` end to end through the agent protocol (book/agents/net-relay.md, spec 120
//! § 10.6, ADR-8183): a relay holds 16 allocations of one host (sans-IO, a counted clock), its
//! report is published the way a server host publishes it, and the agent reads 16 allocations,
//! the caps, the refusals by reason and the bytes of each allocation.

use std::net::SocketAddr;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::relay::{
    RelayConfig, RelayCore, RelayReply, RelayReport, encode_allocate, encode_lookup, encode_register,
    encode_register_with_cookie,
};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

const BASE: u16 = 50_000;
const CLIENTS: u8 = 16;

struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    project: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let project = unique_temp_dir("agent-net-relay-status");
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(AgentSettings {
                project_dir: Some(project.clone()),
            })
            .add_plugins(AgentPlugin)
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        app.finish_plugins();
        let (inbox, inbox_rx) = unbounded();
        app.insert_resource(AgentInbox(inbox_rx));
        app.insert_resource(AgentPermissions {
            config: AgentConfig {
                rate_limit_per_second: u32::MAX,
                ..AgentConfig::default()
            },
            source: None,
        });
        app.update();
        let (reply, replies) = unbounded();
        Self {
            app,
            inbox,
            replies,
            reply,
            project,
        }
    }

    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        self.inbox
            .send(Inbound {
                request,
                reply: self.reply.clone(),
            })
            .expect("inbox open");
        for _ in 0..2 {
            self.app.update();
        }
        let response = self.replies.try_recv().expect("one reply per request");
        match (response.result, response.error) {
            (Some(result), None) => Ok(result),
            (None, Some(error)) => Err(serde_json::to_value(error).expect("error serializes")),
            other => panic!("malformed response {other:?}"),
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.project);
    }
}

fn only(out: &[(Vec<u8>, SocketAddr)]) -> RelayReply {
    assert_eq!(out.len(), 1, "one answer");
    RelayReply::decode(&out[0].0).expect("valid").expect("a reply")
}

/// A relay with one host and 16 allocations, 100 bytes forwarded each way on each.
fn relay_with_sixteen_allocations() -> RelayCore {
    let mut relay = RelayCore::with_config(3, [8; 32], RelayConfig::default());
    relay.add_allocation_ports(BASE..BASE + 24);
    let host = SocketAddr::from(([198, 51, 100, 7], 40_000));
    let RelayReply::RegisterChallenge(cookie) =
        only(&relay.handle(host, &encode_register(600, b"t").expect("fits"), 0))
    else {
        panic!("a challenge");
    };
    let RelayReply::JoinCode(code) = only(&relay.handle(
        host,
        &encode_register_with_cookie(600, b"t", &cookie).expect("fits"),
        0,
    )) else {
        panic!("a code");
    };
    let looker = SocketAddr::from(([10, 1, 0, 200], 5000));
    let RelayReply::HostInfo { bind_key, .. } =
        only(&relay.handle(looker, &encode_lookup(&code).expect("code"), 0))
    else {
        panic!("host info");
    };
    for i in 0..CLIENTS {
        let client = SocketAddr::from(([10, 1, 0, i + 1], 5000));
        let key = [i; 16];
        let RelayReply::Challenge(cookie) =
            only(&relay.handle(client, &encode_allocate(host, &bind_key, &key, None), 10))
        else {
            panic!("an allocate challenge");
        };
        let out = relay.handle(client, &encode_allocate(host, &bind_key, &key, Some(&cookie)), 10);
        let port = out
            .iter()
            .find_map(|(bytes, to)| match RelayReply::decode(bytes) {
                Ok(Some(RelayReply::Allocated { port, .. })) if *to == client => Some(port),
                _ => None,
            })
            .expect("allocated");
        assert_eq!(relay.handle_allocation(port, client, &[7u8; 100], 20).len(), 1);
        assert_eq!(relay.handle_allocation(port, host, &[9u8; 100], 20).len(), 1);
    }
    // A stranger on an allocation port is dropped and counted.
    let stranger = SocketAddr::from(([203, 0, 113, 9], 1));
    assert!(relay.handle_allocation(BASE, stranger, &[1u8; 40], 30).is_empty());
    relay
}

/// Acceptance (N113): `net.relay_status` reports 16 allocations with caps, refusals by reason
/// and the bytes of each allocation. On the base there was no such method.
#[test]
fn net_relay_status_reports_sixteen_allocations() {
    let relay = relay_with_sixteen_allocations();
    let report = RelayReport {
        role: "host+relay".into(),
        relay: "127.0.0.1:7800".into(),
        join_code: Some("ABCDEF".into()),
        relayed_clients: u64::from(CLIENTS),
        confirmed_allocations: u64::from(CLIENTS),
        status: Some(relay.status()),
        config: Some(relay.relay_config().clone()),
        allocations: relay.allocations(),
        nat: None,
        host_check: None,
    };
    let mut h = Harness::new();
    let empty = h
        .call("net.relay_status", json!({}))
        .expect("answers without a report");
    assert_eq!(empty["relays"], json!([]));
    h.app
        .world_mut()
        .get_resource_or_insert_with(NetAgentState::default)
        .publish_relay("server_host", report);
    let v = h.call("net.relay_status", json!({})).expect("answers");
    let r = &v["relays"][0];
    assert_eq!(r["label"], "server_host");
    assert_eq!(r["allocations"], u64::from(CLIENTS));
    assert_eq!(r["most_clients_of_one_host"], u64::from(CLIENTS));
    assert_eq!(r["caps"]["max_clients_per_host"], 40);
    assert_eq!(r["caps"]["max_allocations_per_ip"], 8);
    assert_eq!(r["refusals"]["permission"], 1, "the stranger, by reason");
    assert_eq!(r["refusals"]["full"], 0);
    let rows = r["allocation_list"].as_array().expect("rows");
    assert_eq!(rows.len(), usize::from(CLIENTS));
    for row in rows {
        assert_eq!(row["bytes_to_host"], 100, "{row}");
        assert_eq!(row["bytes_to_client"], 100, "{row}");
    }
    let one = h
        .call("net.relay_status", json!({ "relay": "server_host" }))
        .expect("by label");
    assert_eq!(one["relays"].as_array().map(Vec::len), Some(1));
    let err = h
        .call("net.relay_status", json!({ "relay": "nope" }))
        .expect_err("unknown label");
    assert_eq!(err["data"]["code"], "relay_unknown", "{err}");
    // An empty label reaches the handler; params that are not an object stop at the protocol.
    let err = h
        .call("net.relay_status", json!({ "relay": "" }))
        .expect_err("empty label");
    assert_eq!(err["data"]["code"], "relay_bad_params", "{err}");
    let err = h.call("net.relay_status", json!([1])).expect_err("an array");
    assert_eq!(err["data"]["code"], "F0002", "{err}");
}
