// SPDX-License-Identifier: MIT-0
//! The template match over Steam: the N08 leg of the owner's two-account run (ADR-5375,
//! `book/user-guide/steam-real-run.md` § 4a).
//!
//! One [`SteamLegEndpoint`] runs on each machine, as a step machine over a [`SteamBackend`]:
//!
//! - **Host:** serves the game on [`SteamSocket`] (channel [`RUN_CHANNEL`]), creates a
//!   friends-only lobby through its agent (`lobby.create`, with the build commit and app id in the
//!   lobby data; the guest joins by id, so strangers on app 480 cannot take its slot), launches
//!   the lobby to its own Steam id once the guest is in, reads `net.status` while the guest plays,
//!   and ends after the guest left with its `netstats` line.
//! - **Guest:** joins the lobby through its agent (`lobby.join`), checks the advertised commit and
//!   app id, follows the launch over [`SteamLobbyTransport`] ([`crate::lobby::follow_lobby`]), reads
//!   `net.status` `connected`, lets the scripted [`Bot`] play [`LEG_TICKS`] fixed steps (a poll is one fixed step), stands for
//!   [`LEG_SETTLE`] more, writes its `netstats` line (corrections, resyncs, verify failures) and
//!   leaves.
//!
//! The agent calls go through the app's own agent dispatcher in-process (the same handlers a
//! connected agent reaches), so every call is answered on the frame it is made. Each endpoint's
//! evidence is a [`RealRunLog`] with the net-leg header, judged by
//! [`regia::steam::real_run_net::verify_net_leg`]. The logs carry account tags, never Steam ids or
//! credentials. The mock test (`tests/steam_leg.rs`) runs both endpoints on one `MockSteamNet`.

use std::path::PathBuf;
use std::sync::Arc;

use regia::agent::protocol::Request;
use regia::agent::server::Inbound;
use regia::agent::{AgentInbox, AgentPlugin, AgentSettings};
use regia::net as regia_net;
use regia::prelude::*;
use regia::steam::real_run::{
    APP_KEY, COMMIT_KEY, EndpointStatus, RUN_CHANNEL, RealRunLog, RealRunRole, RealRunSetup, account_tag,
};
use regia::steam::real_run_net::net_leg_log;
use regia::steam::{SteamBackend, SteamId, SteamLobbyTransport, SteamSocket, steam_id_of};
use regia_net::Lobbies;
use regia_net::link::{LinkOptions, NetLink};
use regia_net::lobby::{LobbyEvent, LobbyId, LobbyServer};
use serde_json::{Value, json};

use crate::lobby::{LobbyLinkOptions, LobbySteamTransport, NetGameLobbyPlugin};
use crate::{Bot, NetGameWire, PadSource, Side, headless_app, net_stats_line};

/// Fixed steps the guest's bot plays (10 s at 60 Hz).
pub const LEG_TICKS: u32 = 600;
/// Fixed steps the guest stands still after its bot stopped, before it reports.
pub const LEG_SETTLE: u32 = 120;

/// Where an endpoint is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Host: `lobby.create` was asked; waiting for the lobby id.
    Creating,
    /// Host: the lobby is open; waiting for the guest to join, then launching.
    Open(LobbyId),
    /// Host: launched; serving until the guest left.
    Serving { lobby: LobbyId, peers_seen: bool },
    /// Guest: `lobby.join` was asked; waiting to be in the lobby.
    Joining(LobbyId),
    /// Guest: in the lobby; waiting for the launch to connect the link.
    Connecting,
    /// Guest: connected; playing and settling for this many more fixed steps.
    Playing { left: u32 },
    /// Done (complete or failed).
    Finished,
}

impl Step {
    fn name(self) -> &'static str {
        match self {
            Self::Creating => "lobby_created",
            Self::Open(_) => "member_joined",
            Self::Serving {
                peers_seen: false, ..
            } => "server_peer",
            Self::Serving { peers_seen: true, .. } => "guest_left",
            Self::Joining(_) => "lobby_entered",
            Self::Connecting => "net_connected",
            Self::Playing { .. } => "netstats",
            Self::Finished => "finished",
        }
    }
}

/// One endpoint of the template-match leg. Create it with [`SteamLegEndpoint::start`], call
/// [`SteamLegEndpoint::poll`] after every Steam pump (the endpoint runs one app frame per poll)
/// until it is not [`EndpointStatus::Running`], and keep its [`SteamLegEndpoint::log`].
pub struct SteamLegEndpoint {
    setup: RealRunSetup,
    me: SteamId,
    app: App,
    log: RealRunLog,
    step: Step,
    status: EndpointStatus,
}

impl std::fmt::Debug for SteamLegEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SteamLegEndpoint")
            .field("role", &self.setup.role)
            .field("step", &self.step)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// Splits a `netstats` line (`net_game::net_stats_line`) into its fields.
fn netstats_fields(line: &str) -> Vec<(&str, String)> {
    line.split(' ')
        .skip(1)
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k, v.to_owned()))
        .collect()
}

impl SteamLegEndpoint {
    /// Starts the endpoint on `backend` (already initialized). `agent_dir` is the project folder
    /// the endpoint's agent uses (any empty folder); `opts` are the link options (the mock test
    /// passes a manual clock).
    ///
    /// # Errors
    /// `steam_not_ready` (no signed-in Steam: no lobbies, no sockets, or no Steam id),
    /// `socket_failed` (the Steam socket did not open), `link_failed` (the link did not start),
    /// `request_refused` (the agent refused `lobby.create` / `lobby.join`). Each is a lowercase
    /// code; nothing is logged as a pass.
    pub fn start(
        setup: RealRunSetup,
        backend: &dyn SteamBackend,
        agent_dir: PathBuf,
        opts: LinkOptions,
    ) -> Result<Self, &'static str> {
        let (Some(lobbies), Some(p2p), Some(me)) = (backend.lobbies(), backend.p2p(), backend.steam_id())
        else {
            return Err("steam_not_ready");
        };
        let side = match setup.role {
            RealRunRole::Host => Side::Server,
            RealRunRole::Guest => Side::Client,
        };
        let mut app = headless_app(side);
        app.insert_resource(Lobbies::new(lobbies));
        match setup.role {
            RealRunRole::Host => {
                app.add_plugins(regia_net::LobbyPlugin);
                let socket = SteamSocket::open(p2p, RUN_CHANNEL).map_err(|_| "socket_failed")?;
                app.world_mut()
                    .resource_mut::<NetLink>()
                    .host::<NetGameWire>(Box::new(socket), &opts)
                    .map_err(|_| "link_failed")?;
            }
            RealRunRole::Guest => {
                app.insert_resource(LobbySteamTransport(Arc::new(SteamLobbyTransport::new(
                    p2p,
                    RUN_CHANNEL,
                ))))
                .insert_resource(LobbyLinkOptions(opts))
                .add_plugins(NetGameLobbyPlugin);
            }
        }
        app.insert_resource(AgentSettings {
            project_dir: Some(agent_dir),
        })
        .add_plugins(AgentPlugin);
        app.finish_plugins();

        let mut log = net_leg_log();
        log.record(
            "endpoint",
            &[
                ("role", setup.role.name().to_owned()),
                ("app_id", setup.app_id.0.to_string()),
                (
                    "backend_app_id",
                    backend
                        .app_id()
                        .map_or_else(|| "none".to_owned(), |a| a.0.to_string()),
                ),
                ("commit", setup.commit.clone()),
                ("me", account_tag(me)),
                ("leg", "net_game".to_owned()),
            ],
        );
        let mut endpoint = Self {
            step: Step::Finished,
            status: EndpointStatus::Running,
            setup,
            me,
            app,
            log,
        };
        endpoint.step = match endpoint.setup.role {
            RealRunRole::Host => {
                let data = json!({
                    COMMIT_KEY: endpoint.setup.commit,
                    APP_KEY: endpoint.setup.app_id.0.to_string(),
                });
                endpoint
                    .agent(
                        "lobby.create",
                        json!({ "visibility": "friends_only", "max_members": 2, "data": data }),
                    )
                    .map_err(|_| "request_refused")?;
                Step::Creating
            }
            RealRunRole::Guest => {
                let lobby = endpoint.setup.lobby.ok_or("request_refused")?;
                endpoint
                    .agent("lobby.join", json!({ "lobby": lobby.0 }))
                    .map_err(|_| "request_refused")?;
                endpoint.log.record(
                    "agent",
                    &[
                        ("method", "lobby.join".to_owned()),
                        ("lobby", lobby.0.to_string()),
                    ],
                );
                Step::Joining(lobby)
            }
        };
        Ok(endpoint)
    }

    /// The evidence log so far.
    pub fn log(&self) -> &RealRunLog {
        &self.log
    }

    /// The host's lobby once it exists.
    pub fn lobby(&self) -> Option<LobbyId> {
        match self.step {
            Step::Open(lobby) | Step::Serving { lobby, .. } => Some(lobby),
            _ => None,
        }
    }

    /// Where the run stands.
    pub fn status(&self) -> &EndpointStatus {
        &self.status
    }

    /// The endpoint's app (the test reads its world).
    pub fn app(&self) -> &App {
        &self.app
    }

    /// Ends a run whose caller gave up waiting: logs `result status=failed code=timeout` with the
    /// step that never finished.
    pub fn time_out(&mut self) {
        if self.status == EndpointStatus::Running {
            let waiting = self.step.name();
            self.fail("timeout", &[("waiting", waiting.to_owned())]);
        }
    }

    /// Calls `method` on the endpoint's agent in-process; the app runs one frame to answer it.
    fn agent(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let (send, inbox) = crossbeam_channel::unbounded();
        let (reply, answers) = crossbeam_channel::unbounded();
        self.app.insert_resource(AgentInbox(inbox));
        let request = Request {
            jsonrpc: "2.0".into(),
            id: Some(json!(1)),
            method: method.into(),
            params,
        };
        send.send(Inbound { request, reply })
            .map_err(|_| "agent inbox closed".to_owned())?;
        self.app.update();
        let answer = answers
            .try_recv()
            .map_err(|_| format!("{method}: no answer on this frame"))?;
        match (answer.result, answer.error) {
            (Some(value), None) => Ok(value),
            (_, Some(error)) => Err(format!("{method}: {error:?}")),
            (None, None) => Err(format!("{method}: empty answer")),
        }
    }

    /// `net.status`'s endpoint labelled `label` (`server` or `client`).
    fn net_status(&mut self, label: &str) -> Option<Value> {
        let answer = self.agent("net.status", json!({})).ok()?;
        answer["endpoints"]
            .as_array()?
            .iter()
            .find(|e| e["label"] == label)
            .cloned()
    }

    fn fail(&mut self, code: &'static str, extra: &[(&str, String)]) {
        let mut fields = vec![("status", "failed".to_owned()), ("code", code.to_owned())];
        fields.extend(extra.iter().cloned());
        self.log.record("result", &fields);
        // Leave, so the other endpoint sees it and does not wait out its budget for us.
        self.app.world_mut().resource_mut::<Lobbies>().leave();
        self.status = EndpointStatus::Failed(code);
        self.step = Step::Finished;
    }

    fn complete(&mut self) {
        self.log.record("result", &[("status", "complete".to_owned())]);
        self.status = EndpointStatus::Complete;
        self.step = Step::Finished;
    }

    /// One step: runs one app frame (the lobby pump, the link, the game) and moves the script on.
    pub fn poll(&mut self) -> EndpointStatus {
        if self.status != EndpointStatus::Running {
            return self.status.clone();
        }
        self.app.update();
        match self.step {
            Step::Creating => {
                if let Some(lobby) = self.app.world().resource::<Lobbies>().current() {
                    self.log.record(
                        "agent",
                        &[
                            ("method", "lobby.create".to_owned()),
                            ("lobby", lobby.0.to_string()),
                        ],
                    );
                    self.step = Step::Open(lobby);
                }
            }
            Step::Open(lobby) => self.poll_open(lobby),
            Step::Serving { lobby, peers_seen } => self.poll_serving(lobby, peers_seen),
            Step::Joining(lobby) => self.poll_joining(lobby),
            Step::Connecting => self.poll_connecting(),
            Step::Playing { left } => self.poll_playing(left),
            Step::Finished => {}
        }
        self.status.clone()
    }

    fn poll_open(&mut self, lobby: LobbyId) {
        let me = self.me.0;
        let guest = self
            .app
            .world()
            .resource::<Lobbies>()
            .snapshot()
            .and_then(|s| s.members.iter().find(|m| m.id.0 != me).map(|m| SteamId(m.id.0)));
        let Some(guest) = guest else {
            return;
        };
        self.log
            .record("member_joined", &[("member", account_tag(guest))]);
        let launched = self
            .app
            .world_mut()
            .resource_mut::<Lobbies>()
            .launch(LobbyServer::Steam(me));
        if launched.is_err() {
            self.fail("launch_refused", &[]);
            return;
        }
        self.log.record("launch", &[("server", account_tag(self.me))]);
        self.step = Step::Serving {
            lobby,
            peers_seen: false,
        };
    }

    fn poll_serving(&mut self, lobby: LobbyId, peers_seen: bool) {
        let peers = self
            .app
            .world()
            .resource::<NetLink>()
            .status()
            .map_or(0, |s| s.peer_count);
        if !peers_seen {
            if peers >= 1
                && let Some(server) = self.net_status("server")
            {
                let state = server["state"]
                    .as_str()
                    .unwrap_or("unknown")
                    .replace(char::is_whitespace, "_");
                let count = server["peer_count"].as_u64().unwrap_or(0);
                self.log.record(
                    "net_status",
                    &[
                        ("endpoint", "server".to_owned()),
                        ("state", state),
                        ("peers", count.to_string()),
                    ],
                );
                self.step = Step::Serving {
                    lobby,
                    peers_seen: true,
                };
            }
            return;
        }
        // The guest leaves the lobby after its report; then the host reports and leaves too.
        let alone = self
            .app
            .world()
            .resource::<Lobbies>()
            .snapshot()
            .is_some_and(|s| s.members.iter().all(|m| m.id.0 == self.me.0));
        if alone {
            let line = net_stats_line(self.app.world(), "host");
            self.log.record("netstats", &netstats_fields(&line));
            self.app.world_mut().resource_mut::<Lobbies>().leave();
            self.complete();
        }
    }

    fn poll_joining(&mut self, lobby: LobbyId) {
        let lobbies = self.app.world().resource::<Lobbies>();
        // A refused join (no such lobby, full, banned, rate limited) fails now, not at the timeout.
        let refused = lobbies
            .recent_events()
            .any(|e| matches!(e, LobbyEvent::Entered { result: Err(_), .. }));
        if refused {
            self.fail("lobby_join_failed", &[]);
            return;
        }
        if lobbies.current() != Some(lobby) {
            return;
        }
        let Some(snapshot) = lobbies.snapshot() else {
            return;
        };
        let owner = SteamId(snapshot.owner.0);
        let commit = snapshot.data.get(COMMIT_KEY).cloned();
        let app_id = snapshot.data.get(APP_KEY).cloned();
        self.log.record(
            "lobby_entered",
            &[("lobby", lobby.0.to_string()), ("owner", account_tag(owner))],
        );
        let content_ok = commit.as_deref() == Some(self.setup.commit.as_str())
            && app_id == Some(self.setup.app_id.0.to_string());
        if !content_ok {
            self.fail("content_mismatch", &[]);
            return;
        }
        self.log
            .record("content_checked", &[("status", "match".to_owned())]);
        self.step = Step::Connecting;
    }

    fn poll_connecting(&mut self) {
        if !self.app.world().resource::<NetLink>().is_connected() {
            return;
        }
        let Some(client) = self.net_status("client") else {
            return;
        };
        let state = client["state"]
            .as_str()
            .unwrap_or("unknown")
            .replace(char::is_whitespace, "_");
        if state != "connected" {
            return;
        }
        // The account the link reached: its remote address is a Steam peer address, so the tag
        // comes from the link itself, not from the lobby (`unknown` when it is not a Steam peer).
        let server = self
            .app
            .world()
            .resource::<NetLink>()
            .status()
            .and_then(|s| s.remote_addr)
            .and_then(|addr| addr.parse::<std::net::SocketAddr>().ok())
            .and_then(steam_id_of)
            .map_or_else(|| "unknown".to_owned(), |(id, _)| account_tag(id));
        self.log.record(
            "net_status",
            &[
                ("endpoint", "client".to_owned()),
                ("state", state),
                ("server", server),
            ],
        );
        // The bot starts on the first connected step, so it plays all LEG_TICKS on the link.
        self.app
            .insert_resource(PadSource::Bot(Bot::new(0.0, 30, LEG_TICKS)));
        self.step = Step::Playing {
            left: LEG_TICKS + LEG_SETTLE,
        };
    }

    fn poll_playing(&mut self, left: u32) {
        // A link that drops mid-match ends the leg: the bounds below only mean something for a
        // match that was played to the end.
        if !self.app.world().resource::<NetLink>().is_connected() {
            let line = net_stats_line(self.app.world(), "guest");
            self.log.record("netstats", &netstats_fields(&line));
            self.fail("net_disconnected", &[("left", left.to_string())]);
            return;
        }
        if left > 0 {
            self.step = Step::Playing { left: left - 1 };
            return;
        }
        let line = net_stats_line(self.app.world(), "guest");
        self.log.record("netstats", &netstats_fields(&line));
        self.app.world_mut().resource_mut::<Lobbies>().leave();
        self.complete();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_netstats_line_splits_into_fields() {
        let fields = netstats_fields("netstats who=guest role=client corrections=3 cond=delay=0,jitter=0");
        assert_eq!(fields[0], ("who", "guest".to_owned()));
        assert_eq!(fields[2], ("corrections", "3".to_owned()));
        assert_eq!(fields[3], ("cond", "delay=0,jitter=0".to_owned()));
    }
}
