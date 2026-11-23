// SPDX-License-Identifier: MPL-2.0
//! The two-account Steam acceptance harness (spec 24 § 4, ADR-4896).
//!
//! The owner runs one endpoint on each of two machines, each signed in to Steam as a different
//! account (`book/user-guide/steam-real-run.md`). This module holds everything about that run
//! that does not need Steam, so it is tested deterministically against [`crate::MockSteamNet`]:
//!
//! - [`RealRunSetup`] reads the run's settings. Absent or broken settings are a
//!   [`RealRunNotRun`], never a pass.
//! - [`RealRunEndpoint`] is the host or guest script as a step machine: lobby create and join,
//!   a content check of the build, the launch to the host's Steam id, a datagram echo over
//!   [`crate::SteamSocket`], leave and rejoin, a second echo after the reconnect, and a cloud
//!   file round trip on the endpoint's own account. Each [`RealRunEndpoint::poll`] is one
//!   bounded step; the caller owns the pacing (the real test sleeps between polls, the mock test
//!   does not).
//! - Each endpoint writes its own evidence log ([`RealRunLog`]): plain `name key=value` lines
//!   with the app id, the build commit, and account tags ([`account_tag`]) instead of raw Steam
//!   ids. It never contains a credential: the harness never sees one.
//! - [`verify_real_run`] reads the two independent logs and answers [`RealRunVerdict`]: `PASS`
//!   only when both endpoints finished every step and their records agree with each other.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;

use regia_net::DatagramSocket;
use regia_net::lobby::{
    LobbyBackend, LobbyEvent, LobbyId, LobbyServer, LobbySpec, LobbyVisibility, RequestId,
};

use crate::backend::{AppId, SteamBackend, SteamId};
use crate::sockets::{SteamSocket, steam_peer_addr};

/// `host` or `guest`; unset means the run was not set up (`NOT RUN`).
pub const ROLE_ENV: &str = "REGIA_STEAM_TWO_ACCOUNT";
/// The lobby id the host printed; the guest needs it.
pub const LOBBY_ENV: &str = "REGIA_STEAM_LOBBY_ID";
/// The build commit both machines run (`git rev-parse HEAD`); the content check compares it.
pub const COMMIT_ENV: &str = "REGIA_STEAM_RUN_COMMIT";
/// Where the endpoint writes its evidence log (default `regia-steam-real-run-<role>.log`).
pub const LOG_ENV: &str = "REGIA_STEAM_RUN_LOG";
/// The Steam sockets channel the run uses.
pub const RUN_CHANNEL: u16 = 0;
/// Lobby data key carrying the host's build commit.
pub const COMMIT_KEY: &str = "regia_run_commit";
/// Lobby data key carrying the host's app id.
pub const APP_KEY: &str = "regia_run_app";
/// The first line of every evidence log.
pub const LOG_HEADER: &str = "regia_steam_real_run version=1";
/// Polls the guest keeps pumping after its last datagram so Steam flushes it.
pub const LINGER_POLLS: u32 = 20;

/// Which machine this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RealRunRole {
    /// Creates the lobby, launches to its own Steam id and echoes datagrams.
    Host,
    /// Joins the host's lobby, follows the launch and pings the host.
    Guest,
}

impl RealRunRole {
    /// `host` or `guest`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Guest => "guest",
        }
    }
}

/// Why the run did not start: the setup is absent or wrong, so nothing was tested. Never a pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealRunNotRun {
    /// Lowercase error code (`setup_absent`, `bad_role`, `missing_lobby_id`, `bad_lobby_id`,
    /// `missing_commit`, `bad_app_id`).
    pub code: &'static str,
    /// What to fix.
    pub detail: String,
}

impl fmt::Display for RealRunNotRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NOT RUN ({}): {}", self.code, self.detail)
    }
}

/// The settings of one endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealRunSetup {
    /// Host or guest.
    pub role: RealRunRole,
    /// The app both machines run (480 unless `STEAM_APP_ID` says otherwise).
    pub app_id: AppId,
    /// The lobby to join (guest only).
    pub lobby: Option<LobbyId>,
    /// The build commit (`REGIA_STEAM_RUN_COMMIT`).
    pub commit: String,
}

impl RealRunSetup {
    /// Reads the settings from raw values (`REGIA_STEAM_TWO_ACCOUNT`, `REGIA_STEAM_LOBBY_ID`,
    /// `STEAM_APP_ID`, `REGIA_STEAM_RUN_COMMIT`); `app` `None` means app 480.
    ///
    /// # Errors
    /// [`RealRunNotRun`] when the role is absent or unknown, the guest has no lobby id, the
    /// commit is missing, or a number does not parse.
    pub fn from_values(
        role: Option<&str>,
        lobby: Option<&str>,
        app: Option<&str>,
        commit: Option<&str>,
    ) -> Result<Self, RealRunNotRun> {
        let not_run = |code, detail: String| RealRunNotRun { code, detail };
        let role = match role.map(str::trim) {
            None | Some("") => {
                return Err(not_run(
                    "setup_absent",
                    format!("{ROLE_ENV} is not set; the two-account run needs an owner session"),
                ));
            }
            Some("host") => RealRunRole::Host,
            Some("guest") => RealRunRole::Guest,
            Some(other) => {
                return Err(not_run(
                    "bad_role",
                    format!("{ROLE_ENV} is `host` or `guest`, not `{other}`"),
                ));
            }
        };
        let app_id = match app.map(str::trim) {
            None | Some("") => AppId(480),
            Some(text) => AppId(
                text.parse()
                    .map_err(|_| not_run("bad_app_id", format!("STEAM_APP_ID `{text}` is not a number")))?,
            ),
        };
        let lobby =
            match (role, lobby.map(str::trim)) {
                (RealRunRole::Host, _) => None,
                (RealRunRole::Guest, None | Some("")) => {
                    return Err(not_run(
                        "missing_lobby_id",
                        format!("the guest needs {LOBBY_ENV}, the id the host printed"),
                    ));
                }
                (RealRunRole::Guest, Some(text)) => Some(LobbyId(text.parse().map_err(|_| {
                    not_run("bad_lobby_id", format!("{LOBBY_ENV} `{text}` is not a number"))
                })?)),
            };
        let commit = commit.map(str::trim).unwrap_or_default();
        if commit.is_empty() || !commit.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err(not_run(
                "missing_commit",
                format!("set {COMMIT_ENV}=$(git rev-parse HEAD) on both machines"),
            ));
        }
        Ok(Self {
            role,
            app_id,
            lobby,
            commit: commit.to_owned(),
        })
    }

    /// Reads the settings from the process environment (see [`RealRunSetup::from_values`]).
    ///
    /// # Errors
    /// As [`RealRunSetup::from_values`].
    pub fn from_env() -> Result<Self, RealRunNotRun> {
        let var = |name: &str| regia_core::brand::env_var(name).ok();
        Self::from_values(
            var(ROLE_ENV).as_deref(),
            var(LOBBY_ENV).as_deref(),
            var("STEAM_APP_ID").as_deref(),
            var(COMMIT_ENV).as_deref(),
        )
    }
}

/// A short, stable tag for a Steam account (FNV-1a 64 of the id), so the logs can be matched
/// against each other without publishing the raw account id. Not a secret and not a hash of one.
pub fn account_tag(id: SteamId) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in id.0.to_le_bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("acct-{hash:016x}")
}

/// A 64-bit FNV-1a digest of `bytes`, as hex (content identity in the logs, not security).
pub fn digest(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// One endpoint's evidence log: `name key=value ...` lines in the order things happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RealRunLog {
    lines: Vec<String>,
}

impl Default for RealRunLog {
    fn default() -> Self {
        Self {
            lines: vec![LOG_HEADER.to_owned()],
        }
    }
}

impl RealRunLog {
    /// An empty log whose first line is `header` (the net-leg log of
    /// [`crate::real_run_net`] has its own header).
    pub fn with_header(header: &str) -> Self {
        Self {
            lines: vec![header.to_owned()],
        }
    }

    /// Appends a record. Values must not contain spaces or `=` (the harness writes only
    /// numbers, tags, digests and fixed words).
    pub fn record(&mut self, name: &str, fields: &[(&str, String)]) {
        let mut line = name.to_owned();
        for (key, value) in fields {
            line.push(' ');
            line.push_str(key);
            line.push('=');
            line.push_str(value);
        }
        self.lines.push(line);
    }

    /// The lines so far.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// The log as text, one record per line.
    pub fn text(&self) -> String {
        let mut text = self.lines.join("\n");
        text.push('\n');
        text
    }

    /// Writes the log to `path` atomically (temporary file, then rename).
    ///
    /// # Errors
    /// Any IO failure.
    pub fn write_to(&self, path: &Path) -> std::io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        std::fs::write(&tmp, self.text())?;
        std::fs::rename(&tmp, path)
    }
}

/// What one [`RealRunEndpoint::poll`] reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointStatus {
    /// Still working; poll again.
    Running,
    /// Every step finished; the log ends `result status=complete`.
    Complete,
    /// The run failed; the log ends `result status=failed code=<code>`.
    Failed(&'static str),
}

/// Where the host is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostStep {
    Creating(RequestId),
    /// The lobby is open; echoes every ping until the guest's `done`.
    Open(LobbyId),
}

/// Where the guest is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuestStep {
    /// Joining; `round` is 1 for the first join, 2 for the reconnect.
    Joining { request: RequestId, round: u32 },
    /// In the lobby, waiting for the host's launch.
    WaitServer { round: u32 },
    /// Pinging the host until the round's pong.
    Pinging { host: SteamId, round: u32 },
    /// Left after round 1; waiting for `Left` before rejoining.
    Leaving,
    /// Sends `done` and pumps for [`LINGER_POLLS`] polls.
    Lingering { host: SteamId, left: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Host(HostStep),
    Guest(GuestStep),
    Finished,
}

/// One endpoint of the two-account run (host or guest), as a step machine over a
/// [`SteamBackend`]. Create it with [`RealRunEndpoint::start`], call [`RealRunEndpoint::poll`]
/// after every Steam pump until it is not [`EndpointStatus::Running`], and keep its
/// [`RealRunEndpoint::log`]. If the caller's poll budget runs out, [`RealRunEndpoint::time_out`]
/// records which step never finished.
pub struct RealRunEndpoint {
    setup: RealRunSetup,
    lobbies: Arc<dyn LobbyBackend>,
    socket: SteamSocket,
    me: SteamId,
    step: Step,
    log: RealRunLog,
    status: EndpointStatus,
    /// Rounds whose ping (host) the log already has; repeats are echoed but not logged.
    seen_rounds: Vec<u32>,
    /// Host: guests seen joining, for the reconnect evidence.
    joins: u32,
}

impl fmt::Debug for RealRunEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RealRunEndpoint")
            .field("role", &self.setup.role)
            .field("step", &self.step)
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

impl RealRunEndpoint {
    /// Starts the endpoint on an initialized backend: the host asks for its lobby, the guest
    /// joins the given one. Writes the `endpoint` record.
    ///
    /// # Errors
    /// A code when the backend offers no lobbies, Steam sockets or signed-in user
    /// (`steam_not_ready`), the socket does not open (`socket_failed`), or the first request is
    /// refused locally (`request_refused`).
    pub fn start(setup: RealRunSetup, backend: &dyn SteamBackend) -> Result<Self, &'static str> {
        let (Some(lobbies), Some(p2p), Some(me)) = (backend.lobbies(), backend.p2p(), backend.steam_id())
        else {
            return Err("steam_not_ready");
        };
        let socket = SteamSocket::open(p2p, RUN_CHANNEL).map_err(|_| "socket_failed")?;
        let mut log = RealRunLog::default();
        let backend_app = backend
            .app_id()
            .map_or_else(|| "unknown".to_owned(), |a| a.0.to_string());
        log.record(
            "endpoint",
            &[
                ("role", setup.role.name().to_owned()),
                ("app_id", setup.app_id.0.to_string()),
                ("backend_app_id", backend_app),
                ("commit", setup.commit.clone()),
                ("me", account_tag(me)),
            ],
        );
        let step = match setup.role {
            RealRunRole::Host => {
                // Friends-only (ADR-5375): the guest joins by id, and on the shared test app 480 a
                // public lobby lets a stranger take the guest's slot first.
                let mut spec = LobbySpec {
                    visibility: LobbyVisibility::FriendsOnly,
                    max_members: 2,
                    ..LobbySpec::default()
                };
                spec.data.insert(COMMIT_KEY.into(), setup.commit.clone());
                spec.data.insert(APP_KEY.into(), setup.app_id.0.to_string());
                let request = lobbies.create(&spec).map_err(|_| "request_refused")?;
                Step::Host(HostStep::Creating(request))
            }
            RealRunRole::Guest => {
                let lobby = setup.lobby.ok_or("request_refused")?;
                let request = lobbies.join(lobby).map_err(|_| "request_refused")?;
                Step::Guest(GuestStep::Joining { request, round: 1 })
            }
        };
        Ok(Self {
            setup,
            lobbies,
            socket,
            me,
            step,
            log,
            status: EndpointStatus::Running,
            seen_rounds: Vec::new(),
            joins: 0,
        })
    }

    /// The evidence log so far.
    pub fn log(&self) -> &RealRunLog {
        &self.log
    }

    /// The lobby id once the host created it (the guest needs it in [`LOBBY_ENV`]).
    pub fn lobby(&self) -> Option<LobbyId> {
        match self.step {
            Step::Host(HostStep::Open(lobby)) => Some(lobby),
            _ => self.setup.lobby,
        }
    }

    /// The latest status.
    pub fn status(&self) -> &EndpointStatus {
        &self.status
    }

    /// Records that the caller's poll budget ran out in the current step and fails the run with
    /// `timeout`.
    pub fn time_out(&mut self) {
        if self.status == EndpointStatus::Running {
            let waiting = self.waiting_for();
            self.fail("timeout", &[("waiting", waiting.to_owned())]);
        }
    }

    fn waiting_for(&self) -> &'static str {
        match self.step {
            Step::Host(HostStep::Creating(_)) => "lobby_created",
            Step::Host(HostStep::Open(_)) => "guest_done",
            Step::Guest(GuestStep::Joining { .. }) => "lobby_entered",
            Step::Guest(GuestStep::WaitServer { .. }) => "server_set",
            Step::Guest(GuestStep::Pinging { .. }) => "pong",
            Step::Guest(GuestStep::Leaving) => "left",
            Step::Guest(GuestStep::Lingering { .. }) | Step::Finished => "nothing",
        }
    }

    fn fail(&mut self, code: &'static str, extra: &[(&str, String)]) {
        let mut fields = vec![("status", "failed".to_owned()), ("code", code.to_owned())];
        fields.extend(extra.iter().cloned());
        self.log.record("result", &fields);
        self.status = EndpointStatus::Failed(code);
        self.step = Step::Finished;
        if let Some(lobby) = self.lobby() {
            self.lobbies.leave(lobby);
        }
    }

    /// One step: drains lobby events and datagrams and moves on. Call after each Steam pump.
    pub fn poll(&mut self, backend: &dyn SteamBackend) -> EndpointStatus {
        if self.status != EndpointStatus::Running {
            return self.status.clone();
        }
        let events = self.lobbies.poll();
        match self.step {
            Step::Host(step) => self.poll_host(step, &events, backend),
            Step::Guest(step) => self.poll_guest(step, &events, backend),
            Step::Finished => {}
        }
        self.status.clone()
    }

    /// Reads one datagram, if any: `(sender, payload)`. A socket error fails the run.
    fn recv(&mut self) -> Option<(SteamId, String)> {
        let mut buf = [0u8; 256];
        match self.socket.recv_from(&mut buf) {
            Ok(Some((n, from))) => {
                let sender = crate::sockets::steam_id_of(from).map_or(SteamId(0), |(id, _)| id);
                let text = std::str::from_utf8(&buf[..n])
                    .ok()
                    .filter(|t| t.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'))
                    .map_or_else(|| format!("unprintable.{n}"), str::to_owned);
                Some((sender, text))
            }
            Ok(None) => None,
            Err(_) => {
                self.fail("socket_failed", &[]);
                None
            }
        }
    }

    fn send(&mut self, to: SteamId, payload: &str) -> bool {
        if self
            .socket
            .send_to(payload.as_bytes(), steam_peer_addr(to, RUN_CHANNEL))
            .is_err()
        {
            self.fail("socket_failed", &[]);
            return false;
        }
        true
    }

    fn poll_host(&mut self, step: HostStep, events: &[LobbyEvent], backend: &dyn SteamBackend) {
        let lobby = match step {
            HostStep::Creating(request) => {
                let created = events.iter().find_map(|e| match e {
                    LobbyEvent::Created { request: r, result } if *r == request => Some(result.clone()),
                    _ => None,
                });
                match created {
                    None => return,
                    Some(Err(err)) => {
                        let reason = err.code().to_owned();
                        self.fail("lobby_create_failed", &[("reason", reason)]);
                        return;
                    }
                    Some(Ok(lobby)) => {
                        self.log
                            .record("lobby_created", &[("lobby", lobby.0.to_string())]);
                        self.step = Step::Host(HostStep::Open(lobby));
                        lobby
                    }
                }
            }
            HostStep::Open(lobby) => lobby,
        };
        for event in events {
            match event {
                LobbyEvent::MemberJoined { lobby: l, member } if *l == lobby && member.0 != self.me.0 => {
                    self.joins += 1;
                    self.log.record(
                        "member_joined",
                        &[
                            ("lobby", lobby.0.to_string()),
                            ("member", account_tag(SteamId(member.0))),
                            ("join", self.joins.to_string()),
                        ],
                    );
                    if self.joins == 1 {
                        if self
                            .lobbies
                            .set_server(lobby, LobbyServer::Steam(self.me.0))
                            .is_err()
                        {
                            self.fail("launch_refused", &[]);
                            return;
                        }
                        self.log.record(
                            "server_set",
                            &[("lobby", lobby.0.to_string()), ("server", account_tag(self.me))],
                        );
                    }
                }
                LobbyEvent::MemberLeft { lobby: l, member } if *l == lobby => {
                    self.log.record(
                        "member_left",
                        &[
                            ("lobby", lobby.0.to_string()),
                            ("member", account_tag(SteamId(member.0))),
                        ],
                    );
                }
                LobbyEvent::Left { lobby: l, reason } if *l == lobby => {
                    let reason = reason.code().to_owned();
                    self.fail("host_lost_lobby", &[("reason", reason)]);
                    return;
                }
                _ => {}
            }
        }
        while let Some((from, payload)) = self.recv() {
            let ping_round = parse_round(&payload, "ping", lobby);
            if let Some(round) = ping_round {
                let pong = format!("pong.{round}.{}", lobby.0);
                if !self.send(from, &pong) {
                    return;
                }
                if !self.seen_rounds.contains(&round) {
                    self.seen_rounds.push(round);
                    self.log.record(
                        "datagram_in",
                        &[
                            ("round", round.to_string()),
                            ("from", account_tag(from)),
                            ("payload", payload),
                        ],
                    );
                    self.log.record(
                        "datagram_out",
                        &[
                            ("round", round.to_string()),
                            ("to", account_tag(from)),
                            ("payload", pong),
                        ],
                    );
                }
            } else if payload == format!("done.{}", lobby.0) {
                self.log.record("guest_done", &[("from", account_tag(from))]);
                self.lobbies.leave(lobby);
                self.log.record("left", &[("lobby", lobby.0.to_string())]);
                self.finish_with_cloud(backend);
                return;
            } else {
                self.log.record(
                    "datagram_in",
                    &[("from", account_tag(from)), ("payload", payload)],
                );
                self.fail("wrong_payload", &[]);
                return;
            }
            if self.status != EndpointStatus::Running {
                return;
            }
        }
    }

    fn poll_guest(&mut self, step: GuestStep, events: &[LobbyEvent], backend: &dyn SteamBackend) {
        let Some(lobby) = self.setup.lobby else {
            self.fail("request_refused", &[]);
            return;
        };
        match step {
            GuestStep::Joining { request, round } => {
                let entered = events.iter().find_map(|e| match e {
                    LobbyEvent::Entered { request: r, result } if *r == request => Some(result.clone()),
                    _ => None,
                });
                match entered {
                    None => {}
                    Some(Err(err)) => {
                        let reason = err.code().to_owned();
                        self.fail("lobby_join_failed", &[("reason", reason)]);
                    }
                    Some(Ok(_)) => {
                        let info = self.lobbies.lobby(lobby);
                        let owner = info
                            .as_ref()
                            .map_or_else(|| "unknown".to_owned(), |i| account_tag(SteamId(i.owner.0)));
                        self.log.record(
                            "lobby_entered",
                            &[
                                ("lobby", lobby.0.to_string()),
                                ("owner", owner),
                                ("round", round.to_string()),
                            ],
                        );
                        let data = |key: &str| info.as_ref().and_then(|i| i.data.get(key).cloned());
                        let host_commit = data(COMMIT_KEY).unwrap_or_else(|| "missing".into());
                        let host_app = data(APP_KEY).unwrap_or_else(|| "missing".into());
                        let matches =
                            host_commit == self.setup.commit && host_app == self.setup.app_id.0.to_string();
                        self.log.record(
                            "content_checked",
                            &[
                                ("host_commit", host_commit),
                                ("host_app_id", host_app),
                                ("status", if matches { "match" } else { "mismatch" }.to_owned()),
                            ],
                        );
                        if matches {
                            self.step = Step::Guest(GuestStep::WaitServer { round });
                            // The launch may already be in the lobby data (the reconnect).
                            self.poll_guest(GuestStep::WaitServer { round }, &[], backend);
                        } else {
                            self.fail("content_mismatch", &[]);
                        }
                    }
                }
            }
            GuestStep::WaitServer { round } => {
                let from_event = events.iter().find_map(|e| match e {
                    LobbyEvent::ServerSet {
                        lobby: l,
                        server: LobbyServer::Steam(id),
                    } if *l == lobby => Some(*id),
                    _ => None,
                });
                let from_data = || match self.lobbies.lobby(lobby).and_then(|i| i.server) {
                    Some(LobbyServer::Steam(id)) => Some(id),
                    _ => None,
                };
                if let Some(id) = from_event.or_else(from_data) {
                    let host = SteamId(id);
                    self.log.record(
                        "server_seen",
                        &[("server", account_tag(host)), ("round", round.to_string())],
                    );
                    self.step = Step::Guest(GuestStep::Pinging { host, round });
                    self.poll_guest(GuestStep::Pinging { host, round }, &[], backend);
                }
            }
            GuestStep::Pinging { host, round } => {
                let ping = format!("ping.{round}.{}", lobby.0);
                if !self.send(host, &ping) {
                    return;
                }
                if !self.seen_rounds.contains(&round) {
                    self.seen_rounds.push(round);
                    self.log.record(
                        "datagram_out",
                        &[
                            ("round", round.to_string()),
                            ("to", account_tag(host)),
                            ("payload", ping),
                        ],
                    );
                }
                while let Some((from, payload)) = self.recv() {
                    match parse_round(&payload, "pong", lobby) {
                        Some(r) if r == round => {
                            self.log.record(
                                "datagram_in",
                                &[
                                    ("round", round.to_string()),
                                    ("from", account_tag(from)),
                                    ("payload", payload),
                                ],
                            );
                            self.lobbies.leave(lobby);
                            self.log.record(
                                "left",
                                &[("lobby", lobby.0.to_string()), ("round", round.to_string())],
                            );
                            if round == 1 {
                                self.step = Step::Guest(GuestStep::Leaving);
                            } else {
                                self.step = Step::Guest(GuestStep::Lingering {
                                    host,
                                    left: LINGER_POLLS,
                                });
                            }
                            return;
                        }
                        // A late echo of an earlier round.
                        Some(r) if r < round => {}
                        _ => {
                            self.log.record(
                                "datagram_in",
                                &[("from", account_tag(from)), ("payload", payload)],
                            );
                            self.fail("wrong_payload", &[]);
                            return;
                        }
                    }
                }
            }
            GuestStep::Leaving => {
                let left = events
                    .iter()
                    .any(|e| matches!(e, LobbyEvent::Left { lobby: l, .. } if *l == lobby))
                    || self.lobbies.lobby(lobby).is_none();
                if left {
                    match self.lobbies.join(lobby) {
                        Ok(request) => {
                            self.log.record("rejoin", &[("lobby", lobby.0.to_string())]);
                            self.step = Step::Guest(GuestStep::Joining { request, round: 2 });
                        }
                        Err(_) => self.fail("request_refused", &[]),
                    }
                }
            }
            GuestStep::Lingering { host, left } => {
                let done = format!("done.{}", lobby.0);
                if !self.send(host, &done) {
                    return;
                }
                if left == LINGER_POLLS {
                    self.log.record("done_sent", &[("to", account_tag(host))]);
                }
                // Drain late echoes so they are not mistaken for anything later.
                while self.recv().is_some() {}
                if self.status != EndpointStatus::Running {
                    return;
                }
                if left <= 1 {
                    self.finish_with_cloud(backend);
                } else {
                    self.step = Step::Guest(GuestStep::Lingering { host, left: left - 1 });
                }
            }
        }
    }

    /// The endpoint's own cloud round trip: write a file named after the run, read it back,
    /// compare, delete, and confirm it is gone. Then the `result` record.
    fn finish_with_cloud(&mut self, backend: &dyn SteamBackend) {
        let lobby = self.lobby().map_or(0, |l| l.0);
        let role = self.setup.role.name();
        let file = format!("saves/regia_real_run_{role}.frts");
        let bytes = format!("regia real run {role} lobby {lobby} commit {}", self.setup.commit).into_bytes();
        let wrote = digest(&bytes);
        if backend.cloud_write(&file, &bytes).is_err() {
            self.fail("cloud_write_failed", &[("file", file)]);
            return;
        }
        let read = match backend.cloud_read(&file) {
            Ok(Some(read)) => digest(&read),
            Ok(None) => "missing".to_owned(),
            Err(_) => {
                self.fail("cloud_read_failed", &[("file", file)]);
                return;
            }
        };
        let deleted = backend.cloud_delete(&file).is_ok() && backend.cloud_exists(&file) == Ok(false);
        self.log.record(
            "cloud_round_trip",
            &[
                ("file", file),
                ("bytes", bytes.len().to_string()),
                ("wrote", wrote.clone()),
                ("read", read.clone()),
                ("deleted", deleted.to_string()),
            ],
        );
        if read != wrote {
            self.fail("cloud_mismatch", &[]);
        } else if !deleted {
            self.fail("cloud_delete_failed", &[]);
        } else {
            self.log.record("result", &[("status", "complete".to_owned())]);
            self.status = EndpointStatus::Complete;
            self.step = Step::Finished;
        }
    }
}

/// `<kind>.<round>.<lobby>` → the round, when it is that kind for that lobby.
fn parse_round(payload: &str, kind: &str, lobby: LobbyId) -> Option<u32> {
    let mut parts = payload.split('.');
    if parts.next()? != kind {
        return None;
    }
    let round = parts.next()?.parse().ok()?;
    let l: u64 = parts.next()?.parse().ok()?;
    (l == lobby.0 && parts.next().is_none()).then_some(round)
}

/// The judgment over the two endpoint logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealRunVerdict {
    /// Both endpoints finished every step and agree.
    Pass,
    /// Nothing to judge: a log is missing (`missing_host_log`, `missing_guest_log`) or neither
    /// exists (`setup_absent`).
    NotRun {
        /// Lowercase error code.
        code: &'static str,
    },
    /// An endpoint did not get through every step (`incomplete_host`, `incomplete_guest`).
    Incomplete {
        /// Lowercase error code.
        code: &'static str,
        /// The records that are missing.
        missing: Vec<String>,
    },
    /// The logs contradict a pass: wrong content, wrong peer, a mismatch or a failed endpoint.
    Fail {
        /// Lowercase error code (`log_unreadable`, `wrong_role`, `app_id_mismatch`,
        /// `commit_mismatch`, `same_account`, `lobby_mismatch`, `peer_mismatch`,
        /// `wrong_payload`, `content_mismatch`, `cloud_mismatch`, `endpoint_failed`).
        code: &'static str,
        /// What disagreed.
        detail: String,
    },
}

impl RealRunVerdict {
    /// Only [`RealRunVerdict::Pass`] is a pass.
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }

    /// The lowercase code (`pass` for a pass).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::NotRun { code } | Self::Incomplete { code, .. } | Self::Fail { code, .. } => code,
        }
    }
}

impl fmt::Display for RealRunVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS"),
            Self::NotRun { code } => write!(f, "NOT RUN ({code})"),
            Self::Incomplete { code, missing } => {
                write!(f, "INCOMPLETE ({code}): missing {}", missing.join(", "))
            }
            Self::Fail { code, detail } => write!(f, "FAIL ({code}): {detail}"),
        }
    }
}

/// A parsed record: name plus fields.
pub(crate) type Record = (String, BTreeMap<String, String>);

fn parse_log(text: &str) -> Option<Vec<Record>> {
    parse_log_with(text, LOG_HEADER)
}

/// Parses a log whose first line must be `header`.
pub(crate) fn parse_log_with(text: &str, header: &str) -> Option<Vec<Record>> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next()? != header {
        return None;
    }
    lines
        .map(|line| {
            let mut parts = line.split(' ');
            let name = parts.next()?.to_owned();
            let fields = parts
                .map(|kv| kv.split_once('=').map(|(k, v)| (k.to_owned(), v.to_owned())))
                .collect::<Option<BTreeMap<_, _>>>()?;
            Some((name, fields))
        })
        .collect()
}

/// One endpoint's log, indexed.
pub(crate) struct Endpoint {
    pub(crate) records: Vec<Record>,
}

impl Endpoint {
    pub(crate) fn all(&self, name: &str) -> impl Iterator<Item = &BTreeMap<String, String>> {
        self.records
            .iter()
            .filter(move |(n, _)| n == name)
            .map(|(_, f)| f)
    }
    pub(crate) fn first(&self, name: &str) -> Option<&BTreeMap<String, String>> {
        self.all(name).next()
    }
    pub(crate) fn field(&self, name: &str, key: &str) -> Option<&str> {
        self.first(name).and_then(|f| f.get(key)).map(String::as_str)
    }
    fn round(&self, name: &str, round: u32) -> Option<&BTreeMap<String, String>> {
        let round = round.to_string();
        self.all(name).find(|f| f.get("round") == Some(&round))
    }
}

/// Judges the host's and the guest's evidence logs (`None` = the file does not exist). See
/// [`RealRunVerdict`] for the codes. Order: absent logs, unreadable logs, contradictions, then
/// missing steps, so a log that is both short and wrong reports the contradiction.
pub fn verify_real_run(host: Option<&str>, guest: Option<&str>) -> RealRunVerdict {
    let (host, guest) = match (host, guest) {
        (None, None) => return RealRunVerdict::NotRun { code: "setup_absent" },
        (None, Some(_)) => {
            return RealRunVerdict::NotRun {
                code: "missing_host_log",
            };
        }
        (Some(_), None) => {
            return RealRunVerdict::NotRun {
                code: "missing_guest_log",
            };
        }
        (Some(h), Some(g)) => (h, g),
    };
    let fail = |code, detail: String| RealRunVerdict::Fail { code, detail };
    let (Some(host), Some(guest)) = (parse_log(host), parse_log(guest)) else {
        return fail(
            "log_unreadable",
            "a log lacks the header or has a malformed line".into(),
        );
    };
    let (h, g) = (Endpoint { records: host }, Endpoint { records: guest });
    if h.field("endpoint", "role") != Some("host") || g.field("endpoint", "role") != Some("guest") {
        return fail(
            "wrong_role",
            "the first log must be the host's, the second the guest's".into(),
        );
    }
    let same = |name: &str, key: &str| h.field(name, key) == g.field(name, key);
    if !same("endpoint", "app_id") {
        return fail(
            "app_id_mismatch",
            format!(
                "{:?} vs {:?}",
                h.field("endpoint", "app_id"),
                g.field("endpoint", "app_id")
            ),
        );
    }
    if !same("endpoint", "commit") {
        return fail("commit_mismatch", "the machines ran different builds".into());
    }
    let (host_me, guest_me) = (h.field("endpoint", "me"), g.field("endpoint", "me"));
    if host_me.is_none() || host_me == guest_me {
        return fail(
            "same_account",
            "both endpoints report the same Steam account".into(),
        );
    }
    // Contradictions inside the records that exist.
    for (who, e) in [("host", &h), ("guest", &g)] {
        if let Some(r) = e
            .all("result")
            .find(|r| r.get("status").map(String::as_str) == Some("failed"))
        {
            let code = r.get("code").map_or("unknown", String::as_str);
            if code == "content_mismatch" {
                return fail(
                    "content_mismatch",
                    format!("{who}: the lobby advertised another build"),
                );
            }
            if code == "wrong_payload" || code == "cloud_mismatch" {
                return fail(
                    if code == "wrong_payload" {
                        "wrong_payload"
                    } else {
                        "cloud_mismatch"
                    },
                    format!("{who} failed with `{code}`"),
                );
            }
            return fail("endpoint_failed", format!("{who} failed with `{code}`"));
        }
        if let Some(c) = e.first("cloud_round_trip")
            && c.get("wrote") != c.get("read")
        {
            return fail(
                "cloud_mismatch",
                format!("{who}: the cloud file read back differently"),
            );
        }
    }
    if let Some(lobby) = h.field("lobby_created", "lobby") {
        let lobbies_ok = g
            .all("lobby_entered")
            .all(|r| r.get("lobby").map(String::as_str) == Some(lobby));
        if !lobbies_ok {
            return fail("lobby_mismatch", "the guest entered another lobby".into());
        }
        for (who, e, kind) in [("host", &h, "ping"), ("guest", &g, "pong")] {
            for r in e.all("datagram_in") {
                let ok = r.get("payload").is_some_and(|p| {
                    r.get("round")
                        .is_some_and(|round| *p == format!("{kind}.{round}.{lobby}"))
                });
                if !ok {
                    return fail("wrong_payload", format!("{who} received {:?}", r.get("payload")));
                }
            }
        }
    }
    let peer_ok = |e: &Endpoint, name: &str, key: &str, want: Option<&str>| {
        e.all(name).all(|r| r.get(key).map(String::as_str) == want)
    };
    if !peer_ok(&h, "member_joined", "member", guest_me)
        || !peer_ok(&h, "datagram_in", "from", guest_me)
        || !peer_ok(&g, "server_seen", "server", host_me)
        || !peer_ok(&g, "lobby_entered", "owner", host_me)
        || !peer_ok(&g, "datagram_in", "from", host_me)
    {
        return fail(
            "peer_mismatch",
            "a record names an account other than the other endpoint".into(),
        );
    }
    // Missing steps.
    let host_steps: [(&str, bool); 9] = [
        ("lobby_created", h.first("lobby_created").is_some()),
        (
            "member_joined join=1",
            h.all("member_joined")
                .any(|r| r.get("join").map(String::as_str) == Some("1")),
        ),
        ("server_set", h.first("server_set").is_some()),
        ("datagram_in round=1", h.round("datagram_in", 1).is_some()),
        ("member_left", h.first("member_left").is_some()),
        (
            "member_joined join=2",
            h.all("member_joined")
                .any(|r| r.get("join").map(String::as_str) == Some("2")),
        ),
        ("datagram_in round=2", h.round("datagram_in", 2).is_some()),
        ("guest_done", h.first("guest_done").is_some()),
        ("cloud_round_trip", h.first("cloud_round_trip").is_some()),
    ];
    let guest_steps: [(&str, bool); 9] = [
        ("lobby_entered round=1", g.round("lobby_entered", 1).is_some()),
        ("content_checked", g.first("content_checked").is_some()),
        ("server_seen round=1", g.round("server_seen", 1).is_some()),
        ("datagram_in round=1", g.round("datagram_in", 1).is_some()),
        ("rejoin", g.first("rejoin").is_some()),
        ("lobby_entered round=2", g.round("lobby_entered", 2).is_some()),
        ("datagram_in round=2", g.round("datagram_in", 2).is_some()),
        ("done_sent", g.first("done_sent").is_some()),
        ("cloud_round_trip", g.first("cloud_round_trip").is_some()),
    ];
    let complete = |e: &Endpoint| {
        e.all("result")
            .any(|r| r.get("status").map(String::as_str) == Some("complete"))
    };
    for (code, steps, e) in [
        ("incomplete_host", &host_steps, &h),
        ("incomplete_guest", &guest_steps, &g),
    ] {
        let mut missing: Vec<String> = steps
            .iter()
            .filter(|(_, present)| !present)
            .map(|(name, _)| (*name).to_owned())
            .collect();
        if !complete(e) {
            missing.push("result status=complete".into());
        }
        if !missing.is_empty() {
            return RealRunVerdict::Incomplete { code, missing };
        }
    }
    RealRunVerdict::Pass
}

/// Reads the two log files and judges them; a file that does not exist counts as absent.
///
/// # Errors
/// An IO error other than "not found".
pub fn verify_real_run_files(host: &Path, guest: &Path) -> std::io::Result<RealRunVerdict> {
    let read = |path: &Path| match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err),
    };
    let (host, guest) = (read(host)?, read(guest)?);
    Ok(verify_real_run(host.as_deref(), guest.as_deref()))
}
