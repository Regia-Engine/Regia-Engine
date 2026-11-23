// SPDX-License-Identifier: MPL-2.0
//! `lobby.hub_status` end to end through the agent protocol (book/agents/net-lobby-hub.md,
//! ADR-8559): a hub core holds four sessions, a public lobby with a confirmed session and a private
//! one with an invite; its report is published the way `HubHost::publish` does, and the agent reads
//! presences, games, generations and state bytes, and never the session address.

use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use regia_agent::protocol::{Request, Response};
use regia_agent::server::Inbound;
use regia_agent::{AgentConfig, AgentInbox, AgentPermissions, AgentPlugin, AgentSettings};
use regia_app::prelude::*;
use regia_net::NetAgentState;
use regia_net::lobby::hub::{HubServer, HubSettings};
use regia_net::lobby::{LobbyServer, LobbySpec, LobbyVisibility};
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
        let project = unique_temp_dir("agent-lobby-hub-status");
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

/// Acceptance (NL-04): `lobby.hub_status` reports presences, games, generations and state bytes.
/// On the base there was no such method.
#[test]
fn lobby_hub_status_reports_presence_games_and_generations() {
    let mut hub = HubServer::with_secret(HubSettings::default(), [9; 32]);
    let ann = hub.connect("ann", 0).expect("ann");
    let bob = hub.connect("bob", 0).expect("bob");
    let _cy = hub.connect("cy", 0).expect("cy");
    let public = hub.create(ann, &LobbySpec::default(), 0).expect("public");
    hub.join(bob, public, None, 0).expect("join");
    let server = LobbyServer::Address("10.20.30.40:7777".parse().expect("addr"));
    hub.confirm_session(ann, public, Some(server), 0)
        .expect("confirm");
    let cy = hub.member_by_handle("cy").expect("cy");
    let private = hub
        .create(
            cy,
            &LobbySpec {
                visibility: LobbyVisibility::Private,
                ..LobbySpec::default()
            },
            0,
        )
        .expect("private");
    hub.invite(cy, private, "ann", 0).expect("invite");
    let dee = hub.connect("dee", 0).expect("dee");
    assert!(hub.join(dee, private, None, 0).is_err(), "no invite");
    let (dir_gen, presence_gen) = hub.generations();

    let mut h = Harness::new();
    let empty = h
        .call("lobby.hub_status", json!({}))
        .expect("answers without a report");
    assert_eq!(empty["hubs"], json!([]));
    h.app
        .world_mut()
        .get_resource_or_insert_with(NetAgentState::default)
        .publish_hub("hub", hub.report());
    let v = h.call("lobby.hub_status", json!({})).expect("answers");
    let r = &v["hubs"][0];
    assert_eq!(r["label"], "hub");
    assert_eq!(r["sessions"], 4);
    assert_eq!(r["lobbies"], 2);
    assert_eq!(r["listed"], 1, "only the public lobby is listed");
    assert_eq!(r["invites"], 1);
    assert_eq!(r["generations"]["directory"], dir_gen);
    assert_eq!(r["generations"]["presence"], presence_gen);
    assert!(r["state_bytes"].as_u64().is_some_and(|b| b > 0));
    assert_eq!(r["refused"]["hub_invite_only"], 1, "{r}");
    let players = r["players"].as_array().expect("players");
    assert_eq!(players.len(), 4);
    let presence: Vec<&str> = players.iter().filter_map(|p| p["presence"].as_str()).collect();
    assert_eq!(presence, ["in_game", "in_game", "lobby", "menu"]);
    assert_eq!(players[0]["member"], ann.0.to_string());
    assert_eq!(players[0]["lobby"], public.0.to_string());
    assert_eq!(
        players[2]["lobby"],
        Value::Null,
        "a private lobby is not shown in presence"
    );
    let games = r["games"].as_array().expect("games");
    assert_eq!(games.len(), 1);
    assert_eq!(games[0]["member_count"], 2);
    assert_eq!(games[0]["owner_handle"], "ann");
    assert!(
        !v.to_string().contains("10.20.30.40"),
        "no session address in the report: {v}"
    );
    let _ = bob;

    let one = h
        .call("lobby.hub_status", json!({ "hub": "hub" }))
        .expect("by label");
    assert_eq!(one["hubs"].as_array().map(Vec::len), Some(1));
    let err = h
        .call("lobby.hub_status", json!({ "hub": "nope" }))
        .expect_err("unknown label");
    assert_eq!(err["data"]["code"], "hub_unknown", "{err}");
    let err = h
        .call("lobby.hub_status", json!({ "hub": "" }))
        .expect_err("empty label");
    assert_eq!(err["data"]["code"], "hub_bad_params", "{err}");
    let err = h
        .call("lobby.hub_status", json!({ "label": "hub" }))
        .expect_err("unknown key");
    assert_eq!(err["data"]["code"], "hub_bad_params", "{err}");
}
