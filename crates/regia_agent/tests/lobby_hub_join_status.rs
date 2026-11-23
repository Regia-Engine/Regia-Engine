// SPDX-License-Identifier: MPL-2.0
//! `lobby.hub_join_status` end to end through the agent protocol (book/agents/net-lobby-hub-join.md,
//! ADR-8607): a hub core brokers three joins (mapped, punch, relay), one ask waits, and a forged
//! Hello was refused with `hub_identity_mismatch`; its report is published the way `HubHost::publish`
//! does, and the agent reads each member's path, punch attempts, relay use and the refusal count.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::crypto::PlayerIdentity;
use regia_net::lobby::LobbySpec;
use regia_net::lobby::hub::{HubServer, HubSettings, NatClass, ReachKind, ReachOffer};
use regia_net::nat::{Candidate, CandidateKind};
use regia_test_utils::unique_temp_dir;
use serde_json::{Value, json};

struct Harness {
    app: App,
    inbox: Sender<Inbound>,
    replies: Receiver<Response>,
    reply: Sender<Response>,
    project: std::path::PathBuf,
}

impl Harness {
    fn new() -> Self {
        let project = unique_temp_dir("agent-lobby-hub-join-status");
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

/// Acceptance (NL-05): `lobby.hub_join_status` reports the path chosen per member, punch attempts,
/// relay use, and the `hub_identity_mismatch` refusals. On the base there was no such method.
#[test]
fn lobby_hub_join_status_reports_paths_punches_relays_and_identity_refusals() {
    let mut hub = HubServer::with_secret(HubSettings::default(), [9; 32]);
    let key = |b: u8| PlayerIdentity::from_private([b; 32]).expect("curve").public();
    let owner = hub.connect_player(key(1), "host", 0).expect("host");
    let lobby = hub.create(owner, &LobbySpec::default(), 0).expect("lobby");
    let relay = "192.0.2.1:7800".parse().expect("addr");
    hub.offer_reach(
        owner,
        lobby,
        ReachOffer {
            candidates: vec![Candidate::new(
                CandidateKind::Host,
                "10.0.0.2:7777".parse().expect("addr"),
            )],
            nat: Some(NatClass::PORT_RESTRICTED),
            relay: Some(relay),
            join_code: Some("ABCDEF".into()),
            ticket: Vec::new(),
        },
        0,
    )
    .expect("offer");
    let members = [
        ("android", 2u8, Some(NatClass::SYMMETRIC), ReachKind::Relay, 0u32),
        ("deck", 3, Some(NatClass::FULL_CONE), ReachKind::Punch, 6),
        ("windows", 4, None, ReachKind::Punch, 4),
    ];
    for (handle, k, nat, path, attempts) in members {
        let m = hub.connect_player(key(k), handle, 0).expect("member");
        hub.join(m, lobby, None, 0).expect("join");
        hub.set_member_data(m, lobby, "platform", Some(handle), 0)
            .expect("tag");
        hub.ask_reach(
            m,
            lobby,
            ReachOffer {
                nat,
                ..ReachOffer::default()
            },
            0,
        )
        .expect("ask");
        let plan = hub
            .approve(owner, lobby, m, true, 0)
            .expect("approve")
            .expect("plan");
        assert_eq!(plan.path, path, "{handle}");
        hub.reach_done(m, lobby, path, attempts, 0).expect("done");
    }
    let waiting = hub.connect_player(key(5), "mac", 0).expect("mac");
    hub.join(waiting, lobby, None, 0).expect("join");
    hub.ask_reach(waiting, lobby, ReachOffer::default(), 0)
        .expect("ask");
    // What `HubHost` notes when a Hello's player proof does not match its link.
    hub.note_refused("hub_identity_mismatch");

    let mut h = Harness::new();
    let empty = h
        .call("lobby.hub_join_status", json!({}))
        .expect("answers without a report");
    assert_eq!(empty["hubs"], json!([]));
    h.app
        .world_mut()
        .get_resource_or_insert_with(NetAgentState::default)
        .publish_hub("hub", hub.report());
    let v = h.call("lobby.hub_join_status", json!({})).expect("answers");
    let r = &v["hubs"][0];
    assert_eq!(r["label"], "hub");
    assert_eq!(r["identity_mismatches"], 1, "{r}");
    assert_eq!(r["paths"], json!({ "mapped": 0, "punch": 2, "relay": 1 }), "{r}");
    let joins = r["joins"].as_array().expect("joins");
    assert_eq!(joins.len(), 4, "{r}");
    assert_eq!(joins[0]["handle"], "android");
    assert_eq!(joins[0]["platform"], "android");
    assert_eq!(joins[0]["nat"], "symmetric");
    assert_eq!(joins[0]["planned"], "relay");
    assert_eq!(joins[0]["path"], "relay");
    assert_eq!(joins[0]["relayed"], true);
    assert_eq!(joins[0]["punch_attempts"], 0);
    assert_eq!(joins[1]["path"], "punch");
    assert_eq!(joins[1]["punch_attempts"], 6);
    assert_eq!(joins[1]["relayed"], false);
    assert_eq!(joins[2]["nat"], Value::Null, "an unknown NAT");
    assert_eq!(joins[3]["state"], "asked");
    assert_eq!(joins[3]["path"], Value::Null);
    assert_eq!(joins[0]["identity"], key(2).short());
    assert_eq!(joins[0]["lobby"], lobby.0.to_string());

    let one = h
        .call("lobby.hub_join_status", json!({ "hub": "hub" }))
        .expect("by label");
    assert_eq!(one["hubs"].as_array().map(Vec::len), Some(1));
    let err = h
        .call("lobby.hub_join_status", json!({ "hub": "nope" }))
        .expect_err("unknown label");
    assert_eq!(err["data"]["code"], "hub_unknown", "{err}");
    let err = h
        .call("lobby.hub_join_status", json!({ "hub": 7 }))
        .expect_err("not a string");
    assert_eq!(err["data"]["code"], "hub_bad_params", "{err}");
    let err = h
        .call("lobby.hub_join_status", json!({ "lobby": "1" }))
        .expect_err("unknown key");
    assert_eq!(err["data"]["code"], "hub_bad_params", "{err}");
    // The old method still answers as before.
    let status = h.call("lobby.hub_status", json!({})).expect("hub_status");
    assert_eq!(status["hubs"][0]["sessions"], 5);
}
