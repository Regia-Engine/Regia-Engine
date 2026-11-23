// SPDX-License-Identifier: MPL-2.0
//! The [`Lobbies`] resource and [`LobbyPlugin`] (spec 89 § 8).
//!
//! `Lobbies` wraps a [`LobbyBackend`] with the game-facing rules: one current lobby, the engine
//! compatibility keys and filters (spec 89 § 4), reserved-key checks, ready flags, launch, the
//! matchmaker and a bounded buffer of recent events for the agent. `LobbyPlugin` polls it once
//! per frame in `PreUpdate`, forwards every event as a [`LobbyMessage`] and, with
//! `auto_connect`, connects the [`NetSession`] to a launched lobby's server.

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use regia_app::prelude::*;

use super::matchmaker::{MatchState, Matchmaker, MatchmakingRequest};
use super::query::{Compare, LobbyQuery};
use super::{
    LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyServer, LobbySpec, MemberId, RequestId,
    UnavailableLobbies, check_game_key, check_value, engine_keys, hash_hex, keys,
};
use crate::plugin::NetSet;
use crate::session::NetSession;
use crate::session::lifecycle::{ClientSessionState, HostOptions, PeerState};
use crate::session::net_session::{ConnectOptions, ConnectTarget, SessionError, SocketOpener};
use crate::socket::DatagramSocket;

/// Recent events [`Lobbies`] keeps for `lobby.status`.
pub const RECENT_EVENTS: usize = 256;

/// The `lobby` section of `config/net.ron`, defined in the core lobby module so `NetSettings` builds
/// without the `ecs` feature (ADR-8032); re-exported here at its old path.
pub use super::LobbySettings;

/// How lobby members reach a launched session when it does not run over plain UDP: the Steam
/// plugin installs one over its sockets (`regia_steam`); tests install one over a
/// [`crate::LoopbackNet`].
pub trait LobbyTransport: Send + Sync {
    /// A short name (`steam`, `loopback`).
    fn name(&self) -> &'static str;
    /// Opens a datagram socket for hosting or joining.
    fn open(&self) -> io::Result<Box<dyn DatagramSocket + Send>>;
    /// What the owner publishes after hosting on a socket from [`LobbyTransport::open`] bound at
    /// `local`.
    fn server_for_host(&self, local: SocketAddr) -> LobbyServer;
    /// Where a member connects for `server`; `None` when this transport cannot reach it.
    fn peer_addr(&self, server: &LobbyServer) -> Option<SocketAddr>;
}

/// A [`LobbyTransport`] over an in-memory [`crate::LoopbackNet`]: play-in-editor with several
/// clients in one process, and tests. Sockets bind `127.0.0.1` on a free port.
#[derive(Debug, Clone, Default)]
pub struct LoopbackLobbyTransport {
    net: crate::LoopbackNet,
}

impl LoopbackLobbyTransport {
    /// A transport on `net` (share one net between every process of the test).
    pub fn new(net: crate::LoopbackNet) -> Self {
        Self { net }
    }
}

impl LobbyTransport for LoopbackLobbyTransport {
    fn name(&self) -> &'static str {
        "loopback"
    }

    fn open(&self) -> io::Result<Box<dyn DatagramSocket + Send>> {
        let socket = self.net.bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        Ok(Box::new(socket))
    }

    fn server_for_host(&self, local: SocketAddr) -> LobbyServer {
        LobbyServer::Address(local)
    }

    fn peer_addr(&self, server: &LobbyServer) -> Option<SocketAddr> {
        match server {
            LobbyServer::Address(a) => Some(*a),
            LobbyServer::Steam(_) => None,
        }
    }
}

/// One [`LobbyEvent`] as an ECS message (written by [`LobbyPlugin`]'s pump).
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct LobbyMessage(pub LobbyEvent);

/// Why [`Lobbies::launch_hosting`] or the auto-connect failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LaunchError {
    /// A lobby rule refused it.
    #[error(transparent)]
    Lobby(#[from] LobbyError),
    /// The session refused it.
    #[error(transparent)]
    Session(#[from] SessionError),
    /// No transport to host on and no address given.
    #[error("no lobby transport is installed; pass an address")]
    NoTransport,
}

impl LaunchError {
    /// The stable code (spec 89 § 9).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Lobby(e) => e.code(),
            Self::Session(_) => "session_error",
            Self::NoTransport => "no_transport",
        }
    }
}

/// Where the identity [`Lobbies`] writes into the engine keys and filters on came from (spec 89
/// § 4, ADR-7881).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LobbyIdentity {
    /// Nothing set it: create, list and matchmaking answer [`LobbyError::NoIdentity`] unless a
    /// game id was set ([`Lobbies::set_game`]) or the game opted in
    /// ([`Lobbies::allow_unidentified`]).
    Unset,
    /// [`LobbyPlugin`] copied the hashes from the app's `NetSession` manifest, the one the content
    /// handshake sends, and keeps them in step with it every frame.
    Session,
    /// The game called [`Lobbies::set_identity`]; the plugin leaves it alone.
    Explicit,
}

impl LobbyIdentity {
    /// `unset`, `session` or `explicit` (`lobby.status` `identity`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Unset => "unset",
            Self::Session => "session",
            Self::Explicit => "explicit",
        }
    }
}

/// The game's lobby state (spec 89 § 8). Insert it with a backend (`Lobbies::new`) before
/// [`LobbyPlugin`] runs, or let the plugin insert an unavailable one.
///
/// The engine keys carry an identity (game id, protocol and content hash) so a build only sees
/// lobbies it could join. [`LobbyPlugin`] takes it from the app's `NetSession` manifest (the
/// values the content handshake checks) unless the game set one ([`Lobbies::set_identity`]).
/// Without either, create, list and matchmaking answer [`LobbyError::NoIdentity`] unless the game
/// opts in with [`Lobbies::allow_unidentified`] (ADR-7881).
#[derive(Resource)]
pub struct Lobbies {
    backend: Arc<dyn LobbyBackend>,
    settings: LobbySettings,
    game: String,
    protocol_hash: u64,
    content_hash: u64,
    identity: LobbyIdentity,
    allow_unidentified: bool,
    current: Option<LobbyId>,
    snapshot: Option<LobbyInfo>,
    pending_create: Option<RequestId>,
    pending_join: Option<RequestId>,
    pending_list: Option<RequestId>,
    last_list: Option<Result<Vec<LobbyInfo>, LobbyError>>,
    matchmaker: Matchmaker,
    matchmaking: bool,
    recent: VecDeque<LobbyEvent>,
    transport: Option<Arc<dyn LobbyTransport>>,
    connected_to: Option<LobbyServer>,
    /// The server auto-connect tried in this lobby (success or not).
    connect_tried: Option<LobbyServer>,
    last_launch_error: Option<String>,
    /// `regia.mode` / `regia.playlist` the host publishes (ADR-8897).
    rotation: BTreeMap<String, String>,
}

impl std::fmt::Debug for Lobbies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lobbies")
            .field("backend", &self.backend.name())
            .field("current", &self.current)
            .field("matchmaking", &self.matchmaker.state())
            .finish_non_exhaustive()
    }
}

impl Default for Lobbies {
    fn default() -> Self {
        Self::new(Arc::new(UnavailableLobbies))
    }
}

/// Membership, data, owner, or server events for `lobby` make the cached snapshot stale.
/// Chat and invites do not.
fn snapshot_needs_refresh(lobby: LobbyId, event: &LobbyEvent) -> bool {
    match event {
        LobbyEvent::MemberJoined { lobby: id, .. }
        | LobbyEvent::MemberLeft { lobby: id, .. }
        | LobbyEvent::DataChanged { lobby: id, .. }
        | LobbyEvent::OwnerChanged { lobby: id, .. }
        | LobbyEvent::ServerSet { lobby: id, .. } => *id == lobby,
        LobbyEvent::Created { result: Ok(id), .. } | LobbyEvent::Entered { result: Ok(id), .. } => {
            *id == lobby
        }
        _ => false,
    }
}

impl Lobbies {
    /// Lobby state over `backend`, with default settings and an empty game id.
    pub fn new(backend: Arc<dyn LobbyBackend>) -> Self {
        Self {
            backend,
            settings: LobbySettings::default(),
            game: String::new(),
            protocol_hash: 0,
            content_hash: 0,
            identity: LobbyIdentity::Unset,
            allow_unidentified: false,
            current: None,
            snapshot: None,
            pending_create: None,
            pending_join: None,
            pending_list: None,
            last_list: None,
            matchmaker: Matchmaker::default(),
            matchmaking: false,
            recent: VecDeque::new(),
            transport: None,
            connected_to: None,
            connect_tried: None,
            last_launch_error: None,
            rotation: BTreeMap::new(),
        }
    }

    /// Sets the game id and the hashes lobbies must match (from the session's manifest). From
    /// then on [`LobbyPlugin`] no longer copies them from the `NetSession`
    /// ([`LobbyIdentity::Explicit`]).
    pub fn set_identity(&mut self, game: &str, manifest: &crate::session::ContentManifest) {
        self.game = game.to_owned();
        self.protocol_hash = manifest.protocol_hash;
        self.content_hash = manifest.content_hash;
        self.identity = LobbyIdentity::Explicit;
    }

    /// Spec 89 § 4 (ADR-7213): while the game set no identity, the protocol and content hashes
    /// follow `manifest` (the session's, copied by [`lobby_system`] every pump); the game id
    /// stays as it is (the manifest has none). Returns whether the hashes changed. The same rule
    /// as [`Lobbies::adopt_session_identity`] (ADR-7881), which also marks the identity
    /// [`LobbyIdentity::Session`].
    pub fn follow_manifest(&mut self, manifest: &crate::session::ContentManifest) -> bool {
        let hashes_changed =
            (self.protocol_hash, self.content_hash) != (manifest.protocol_hash, manifest.content_hash);
        self.adopt_session_identity(manifest) && hashes_changed
    }

    /// Sets only the game id (`regia.game`), leaving the hashes to [`LobbyPlugin`], which copies
    /// them from the `NetSession` manifest. A non-empty game id also counts as an identity on its
    /// own (a game without a `NetSession`).
    pub fn set_game(&mut self, game: &str) {
        self.game = game.to_owned();
    }

    /// Takes the hashes from `manifest` (the `NetSession` manifest the content handshake sends),
    /// unless the game set the identity itself ([`Lobbies::set_identity`]). [`LobbyPlugin`] calls
    /// it at startup and every frame; returns whether the identity changed. A lobby this member
    /// owns gets the new keys on its next pump (ADR-7213).
    pub fn adopt_session_identity(&mut self, manifest: &crate::session::ContentManifest) -> bool {
        if self.identity == LobbyIdentity::Explicit {
            return false;
        }
        let changed = self.identity != LobbyIdentity::Session
            || self.protocol_hash != manifest.protocol_hash
            || self.content_hash != manifest.content_hash;
        self.protocol_hash = manifest.protocol_hash;
        self.content_hash = manifest.content_hash;
        self.identity = LobbyIdentity::Session;
        changed
    }

    /// Where the identity came from.
    pub fn identity(&self) -> LobbyIdentity {
        self.identity
    }

    /// The game id, protocol hash and content hash the engine keys carry.
    pub fn identity_values(&self) -> (&str, u64, u64) {
        (&self.game, self.protocol_hash, self.content_hash)
    }

    /// Opts in to lobbies with no identity (an empty game id and zero hashes): every build that
    /// does the same lists and joins every other one. Off by default (ADR-7881); tests and tools
    /// that do not care which build they meet turn it on.
    pub fn allow_unidentified(&mut self, allow: bool) {
        self.allow_unidentified = allow;
    }

    /// Whether [`Lobbies::allow_unidentified`] is on.
    pub fn unidentified_allowed(&self) -> bool {
        self.allow_unidentified
    }

    /// Whether create, list and matchmaking may run: an identity from the session or the game,
    /// a game id, or the opt-in. A backend with no platform answers `lobby_unavailable` first.
    fn check_identity(&self) -> Result<(), LobbyError> {
        let identified = self.identity != LobbyIdentity::Unset || !self.game.is_empty();
        if identified || self.allow_unidentified || self.backend.local_member().is_none() {
            return Ok(());
        }
        tracing::warn!(target: "regia_net", "lobby call refused: {}", LobbyError::NoIdentity);
        Err(LobbyError::NoIdentity)
    }

    /// Replaces the settings.
    pub fn set_settings(&mut self, settings: LobbySettings) {
        self.settings = settings;
    }

    /// The settings in force.
    pub fn settings(&self) -> &LobbySettings {
        &self.settings
    }

    /// Installs the transport members use to reach a launched session.
    pub fn set_transport(&mut self, transport: Arc<dyn LobbyTransport>) {
        self.transport = Some(transport);
    }

    /// The installed transport's name.
    pub fn transport_name(&self) -> Option<&'static str> {
        self.transport.as_ref().map(|t| t.name())
    }

    /// Replaces the backend (leaving the current lobby on the old one).
    pub fn set_backend(&mut self, backend: Arc<dyn LobbyBackend>) {
        self.leave();
        // Request ids are per backend: an old one must not match the new backend's answers.
        self.matchmaker.forget_abandoned();
        self.pending_list = None;
        self.backend = backend;
    }

    /// The backend.
    pub fn backend(&self) -> &dyn LobbyBackend {
        &*self.backend
    }

    /// The local member, if the platform is available.
    pub fn local_member(&self) -> Option<MemberId> {
        self.backend.local_member()
    }

    /// The lobby we are in.
    pub fn current(&self) -> Option<LobbyId> {
        self.current
    }

    /// The current lobby as of the last pump, members included.
    pub fn snapshot(&self) -> Option<&LobbyInfo> {
        self.snapshot.as_ref()
    }

    /// Whether we own the current lobby.
    pub fn is_owner(&self) -> bool {
        match (&self.snapshot, self.local_member()) {
            (Some(s), Some(me)) => s.owner == me,
            _ => false,
        }
    }

    /// The last list result.
    pub fn last_list(&self) -> Option<&Result<Vec<LobbyInfo>, LobbyError>> {
        self.last_list.as_ref()
    }

    /// The matchmaker.
    pub fn matchmaker(&self) -> &Matchmaker {
        &self.matchmaker
    }

    /// Recent events, oldest first (at most [`RECENT_EVENTS`]).
    pub fn recent_events(&self) -> impl Iterator<Item = &LobbyEvent> {
        self.recent.iter()
    }

    /// The server the session auto-connected to (`None` while not connected, and after a failed
    /// attempt: see [`Lobbies::last_launch_error`]).
    pub fn connected_to(&self) -> Option<LobbyServer> {
        self.connected_to
    }

    /// The last auto-connect failure in the current lobby, if any.
    pub fn last_launch_error(&self) -> Option<&str> {
        self.last_launch_error.as_deref()
    }

    /// The engine compatibility filters (spec 89 § 4) added to `query`.
    pub fn compatible(&self, mut query: LobbyQuery) -> LobbyQuery {
        query = query
            .with_string(keys::GAME, Compare::Eq, &self.game)
            .with_string(keys::PROTOCOL, Compare::Eq, &hash_hex(self.protocol_hash));
        if self.settings.match_content {
            query = query.with_string(keys::CONTENT, Compare::Eq, &hash_hex(self.content_hash));
        }
        query
    }

    /// The create spec with the engine keys added; game keys are checked.
    fn engine_spec(&self, mut spec: LobbySpec) -> Result<LobbySpec, LobbyError> {
        for (k, v) in &spec.data {
            check_game_key(k)?;
            check_value(v)?;
        }
        spec.data
            .extend(engine_keys(&self.game, self.protocol_hash, self.content_hash));
        spec.data
            .extend(self.rotation.iter().map(|(k, v)| (k.clone(), v.clone())));
        Ok(spec)
    }

    fn idle(&self) -> Result<(), LobbyError> {
        if self.current.is_some()
            || self.pending_create.is_some()
            || self.pending_join.is_some()
            || self.matchmaking
        {
            return Err(LobbyError::AlreadyInLobby);
        }
        Ok(())
    }

    /// Lists compatible lobbies; the result lands in [`Lobbies::last_list`].
    pub fn list(&mut self, query: LobbyQuery) -> Result<RequestId, LobbyError> {
        self.check_identity()?;
        let request = self.backend.request_list(&self.compatible(query))?;
        self.pending_list = Some(request);
        Ok(request)
    }

    /// Creates a lobby with the engine keys; game keys must not be reserved.
    pub fn create(&mut self, spec: LobbySpec) -> Result<RequestId, LobbyError> {
        self.idle()?;
        self.check_identity()?;
        let spec = self.engine_spec(spec)?;
        let request = self.backend.create(&spec)?;
        self.pending_create = Some(request);
        Ok(request)
    }

    /// A create spec from the settings.
    pub fn default_spec(&self) -> LobbySpec {
        LobbySpec {
            visibility: self.settings.visibility,
            max_members: self.settings.max_members,
            data: BTreeMap::new(),
            joinable: true,
        }
    }

    /// Joins a lobby by id.
    pub fn join(&mut self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
        self.idle()?;
        let request = self.backend.join(lobby)?;
        self.pending_join = Some(request);
        Ok(request)
    }

    /// Leaves the current lobby and stops matchmaking. No-op when not in one.
    pub fn leave(&mut self) {
        if self.matchmaking {
            self.matchmaker.cancel(&*self.backend);
            self.matchmaking = false;
        }
        if let Some(lobby) = self.current.take() {
            self.backend.leave(lobby);
        }
        // A create or join still in flight: leave the lobby it lands in (`pump`).
        for request in [self.pending_create.take(), self.pending_join.take()]
            .into_iter()
            .flatten()
        {
            self.matchmaker.abandon(request);
        }
        self.forget_lobby();
    }

    /// Clears everything that belongs to the lobby we were in (`connect_tried` would otherwise
    /// block auto-connect in the next one).
    fn forget_lobby(&mut self) {
        self.current = None;
        self.snapshot = None;
        self.connected_to = None;
        self.connect_tried = None;
        self.last_launch_error = None;
    }

    fn current_or_err(&self) -> Result<LobbyId, LobbyError> {
        self.current.ok_or(LobbyError::NotInLobby)
    }

    /// Sets or deletes a game key of the current lobby (owner only; `regia.` keys refused).
    pub fn set_data(&mut self, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
        check_game_key(key)?;
        self.backend.set_data(self.current_or_err()?, key, value)
    }

    /// Sets the rotation keys `regia.mode` and `regia.playlist` (ADR-8897): a lobby created from
    /// now on carries them, and the owner of the current lobby writes them at once. An empty value
    /// deletes its key. A browser or matchmaker filters on them with [`LobbyQuery::with_mode`] and
    /// [`LobbyQuery::with_playlist`].
    pub fn set_rotation_keys(&mut self, mode: &str, playlist: &str) -> Result<(), LobbyError> {
        check_value(mode)?;
        check_value(playlist)?;
        let next = super::rotation_keys(mode, playlist);
        if let (Some(lobby), true) = (self.current, self.is_owner()) {
            for key in [keys::MODE, keys::PLAYLIST] {
                if next.get(key) != self.snapshot.as_ref().and_then(|s| s.data.get(key)) {
                    self.backend
                        .set_data(lobby, key, next.get(key).map(String::as_str))?;
                }
            }
        }
        self.rotation = next;
        Ok(())
    }

    /// [`Lobbies::set_rotation_keys`] from a match session ([`super::session_rotation_keys`]): the
    /// host calls it after each launch.
    pub fn publish_session(
        &mut self,
        session: &regia_rules::session::MatchSession,
    ) -> Result<(), LobbyError> {
        let data = super::session_rotation_keys(session);
        let mode = data.get(keys::MODE).cloned().unwrap_or_default();
        let playlist = data.get(keys::PLAYLIST).cloned().unwrap_or_default();
        self.set_rotation_keys(&mode, &playlist)
    }

    /// The rotation keys this host publishes (ADR-8897).
    pub fn rotation_keys(&self) -> &BTreeMap<String, String> {
        &self.rotation
    }

    /// Sets or deletes a game key of the local member's data (`regia.` keys refused).
    pub fn set_member_data(&mut self, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
        check_game_key(key)?;
        self.backend.set_member_data(self.current_or_err()?, key, value)
    }

    /// Marks the local member ready (`regia.ready`).
    pub fn set_ready(&mut self, ready: bool) -> Result<(), LobbyError> {
        let lobby = self.current_or_err()?;
        self.backend
            .set_member_data(lobby, keys::READY, ready.then_some("1"))
    }

    /// Sends a chat message to the current lobby.
    pub fn send_chat(&mut self, text: &str) -> Result<(), LobbyError> {
        self.backend.send_chat(self.current_or_err()?, text)
    }

    /// Starts matchmaking with the engine filters and keys added; `request.query` holds the
    /// game's filters.
    pub fn start_matchmaking(&mut self, mut request: MatchmakingRequest) -> Result<(), LobbyError> {
        self.idle()?;
        if self.backend.local_member().is_none() {
            return Err(LobbyError::Unavailable);
        }
        self.check_identity()?;
        request.query = self.compatible(request.query);
        request.query.max_results = self.settings.results_per_round.clamp(1, super::MAX_LIST_RESULTS);
        request.create = self.engine_spec(request.create)?;
        // The fallback create goes out rounds later: refuse a spec the backend would refuse now,
        // with its own code, instead of failing then as `create_failed`. A join-only request
        // never sends it, so its member limit and key count do not matter.
        if request.create_if_none {
            super::check_spec(&request.create)?;
        }
        self.matchmaker.start(&*self.backend, request);
        self.matchmaking = true;
        Ok(())
    }

    /// A matchmaking request from the settings (game filters empty).
    pub fn default_request(&self) -> MatchmakingRequest {
        MatchmakingRequest {
            query: LobbyQuery::default(),
            rounds: self.settings.rounds.clone(),
            create_if_none: self.settings.create_if_none,
            create: self.default_spec(),
            min_players: self.settings.min_players,
            step_timeout: self.settings.step_timeout,
        }
    }

    /// Stops matchmaking and leaves any lobby it joined. A lobby joined by hand stays.
    pub fn cancel_matchmaking(&mut self) {
        if self.matchmaking {
            self.leave();
        }
        self.matchmaker.reset();
    }

    /// Whether the owner may launch (matchmaking or not): owner, enough members, all ready.
    pub fn ready_to_launch(&self) -> bool {
        let min = self.settings.min_players.max(1);
        self.is_owner()
            && self
                .snapshot
                .as_ref()
                .is_some_and(|s| s.member_count >= min && s.all_ready())
    }

    /// Publishes where the session is (owner only): `regia.state = launching`, the server, and
    /// the lobby closed to joins unless `joinable_in_game`.
    ///
    /// The publish is all-or-nothing: a refusal partway (a refused write, ownership lost
    /// mid-launch) rolls the publish back before the error returns, so the lobby never
    /// advertises a session the caller is about to tear down (members get one auto-connect
    /// attempt per lobby, spec 89 § 8).
    pub fn launch(&mut self, server: LobbyServer) -> Result<(), LobbyError> {
        let lobby = self.current_or_err()?;
        if !self.is_owner() {
            return Err(LobbyError::NotOwner);
        }
        // The server first: a refused publish must not leave a `launching` lobby without one.
        self.backend.set_server(lobby, server)?;
        let published = self
            .backend
            .set_data(lobby, keys::STATE, Some("launching"))
            .and_then(|()| {
                if self.settings.joinable_in_game {
                    Ok(())
                } else {
                    self.backend.set_joinable(lobby, false)
                }
            });
        if let Err(e) = published {
            // Half-published: roll it back best-effort (the state and the server key), so no
            // member spends its single auto-connect attempt on a dead session.
            if let Err(rollback) = self
                .backend
                .set_data(lobby, keys::STATE, Some("open"))
                .and_then(|()| self.backend.clear_server(lobby))
            {
                tracing::warn!(target: "regia_net", lobby = %lobby.0, error = %rollback, "lobby launch rolled back, but the un-publish failed");
            }
            return Err(e);
        }
        Ok(())
    }

    /// Hosts a listen session on the installed transport and launches the lobby to it. A launch
    /// that cannot be published leaves no session hosting.
    pub fn launch_hosting(
        &mut self,
        session: &mut NetSession,
        opts: HostOptions,
    ) -> Result<LobbyServer, LaunchError> {
        self.current_or_err()?;
        if !self.is_owner() {
            return Err(LobbyError::NotOwner.into());
        }
        let transport = self.transport.clone().ok_or(LaunchError::NoTransport)?;
        let socket = transport
            .open()
            .map_err(|e| SessionError::Transport(e.to_string()))?;
        let local = socket
            .local_addr()
            .map_err(|e| SessionError::Transport(e.to_string()))?;
        session.host_on(socket, opts, false)?;
        let server = transport.server_for_host(local);
        if let Err(e) = self.launch(server) {
            session.reset();
            return Err(e.into());
        }
        Ok(server)
    }

    /// Owner: moves `regia.state` from `launching` to `in_game` once `session` shows play: a
    /// hosted session with a peer in game (or the owner alone in the lobby), or the owner's own
    /// client in game on a session hosted elsewhere. [`LobbyPlugin`] calls it every frame; a
    /// refused write is tried again on the next one.
    pub fn sync_state(&mut self, session: &NetSession) {
        let (Some(lobby), Some(snapshot)) = (self.current, self.snapshot.as_ref()) else {
            return;
        };
        if snapshot.data.get(keys::STATE).map(String::as_str) != Some("launching") || !self.is_owner() {
            return;
        }
        let playing = match session.server() {
            Some(server) => {
                snapshot.member_count <= 1
                    || server
                        .peer_ids()
                        .into_iter()
                        .any(|peer| server.peer_state(peer) == Some(PeerState::InGame))
            }
            None => session.state() == ClientSessionState::InGame,
        };
        if playing && let Err(e) = self.backend.set_data(lobby, keys::STATE, Some("in_game")) {
            tracing::debug!(target: "regia_net", "lobby state not updated: {e}");
        }
    }

    fn record(&mut self, event: &LobbyEvent) {
        if self.recent.len() >= RECENT_EVENTS {
            self.recent.pop_front();
        }
        self.recent.push_back(event.clone());
    }

    /// Polls the backend, applies the events to the current lobby, steps the matchmaker and
    /// returns the events. Called once per frame by [`LobbyPlugin`]; call it yourself without
    /// the plugin.
    pub fn pump(&mut self) -> Vec<LobbyEvent> {
        let events = self.backend.poll();
        for event in &events {
            self.record(event);
            match event {
                LobbyEvent::Created { request, result } if Some(*request) == self.pending_create => {
                    self.pending_create = None;
                    if let Ok(lobby) = result {
                        self.current = Some(*lobby);
                    }
                }
                LobbyEvent::Entered { request, result } if Some(*request) == self.pending_join => {
                    self.pending_join = None;
                    if let Ok(lobby) = result {
                        self.current = Some(*lobby);
                    }
                }
                LobbyEvent::List { request, result } if Some(*request) == self.pending_list => {
                    self.pending_list = None;
                    self.last_list = Some(result.clone());
                }
                LobbyEvent::Left { lobby, .. } if Some(*lobby) == self.current => {
                    self.forget_lobby();
                }
                _ => {}
            }
        }
        if !self.matchmaking {
            // `step` settles them while matchmaking.
            self.matchmaker.settle_abandoned(&*self.backend, &events);
        }
        if self.matchmaking {
            self.matchmaker.step(&*self.backend, &events);
            match self.matchmaker.state() {
                MatchState::InLobby { lobby, .. } | MatchState::Launched { lobby, .. } => {
                    self.current = Some(*lobby);
                }
                MatchState::Failed(_) => {
                    self.matchmaking = false;
                }
                _ => {}
            }
        }
        if let Some(lobby) = self.current {
            let refresh =
                self.snapshot.is_none() || events.iter().any(|event| snapshot_needs_refresh(lobby, event));
            if refresh {
                self.snapshot = self.backend.lobby(lobby);
            } else if !self.backend.still_in(lobby) {
                // The backend no longer has us in it (a lost Left event).
                self.snapshot = None;
            }
            if self.snapshot.is_none() {
                self.forget_lobby();
                if self.matchmaking {
                    self.matchmaker.lobby_lost();
                    self.matchmaking = false;
                }
            }
        }
        self.resend_stale_identity_keys();
        events
    }

    /// Spec 89 § 4 (ADR-7213): a lobby created before the session manifest was followed (a
    /// `Startup` create not ordered `.after(lobby_follow_manifest)`, or a create still in flight
    /// when the hashes arrived) carries zero hashes; once the followed hashes are known, the owner
    /// re-sends the engine identity keys. A joined lobby is its owner's to patch. Runs every pump
    /// and writes only while a key disagrees, so a steady frame allocates nothing.
    fn resend_stale_identity_keys(&self) {
        if self.identity == LobbyIdentity::Explicit || !self.is_owner() {
            return;
        }
        let (Some(lobby), Some(snapshot)) = (self.current, self.snapshot.as_ref()) else {
            return;
        };
        for (key, hash) in [
            (keys::PROTOCOL, self.protocol_hash),
            (keys::CONTENT, self.content_hash),
        ] {
            if snapshot
                .data
                .get(key)
                .and_then(|v| u64::from_str_radix(v, 16).ok())
                == Some(hash)
            {
                continue;
            }
            tracing::debug!(target: "regia_net", key, "lobby: re-sending a stale engine identity key after the manifest follow");
            if let Err(e) = self.backend.set_data(lobby, key, Some(&hash_hex(hash))) {
                tracing::debug!(target: "regia_net", key, error = %e, "lobby: re-sending an engine identity key failed");
            }
        }
    }

    /// The server to auto-connect to now: launched, not the owner, not yet connected.
    fn pending_connect(&self) -> Option<LobbyServer> {
        // One attempt per lobby: a refusal is not retried every frame.
        if !self.settings.auto_connect || self.connect_tried.is_some() || self.is_owner() {
            return None;
        }
        self.snapshot.as_ref().and_then(|s| s.server)
    }

    /// Connects `session` to the current lobby's server if it launched and we have not
    /// connected yet. Returns the server connected to.
    pub fn auto_connect(&mut self, session: &mut NetSession) -> Option<Result<LobbyServer, LaunchError>> {
        let server = self.pending_connect()?;
        let result = self.connect_session(session, server);
        self.connect_tried = Some(server);
        match &result {
            Ok(_) => {
                self.connected_to = Some(server);
                self.last_launch_error = None;
            }
            Err(e) => {
                self.connected_to = None;
                self.last_launch_error = Some(e.to_string());
            }
        }
        Some(result)
    }

    fn connect_session(
        &self,
        session: &mut NetSession,
        server: LobbyServer,
    ) -> Result<LobbyServer, LaunchError> {
        let opts = ConnectOptions::default();
        let via_transport = self
            .transport
            .as_ref()
            .and_then(|t| t.peer_addr(&server).map(|a| (t, a)));
        match (via_transport, server) {
            (Some((transport, addr)), _) => {
                // The session keeps the opener: a suspended client resumes over this transport
                // (Steam sockets), not UDP (spec 120 § 10.3).
                let transport = Arc::clone(transport);
                let opener: SocketOpener = Arc::new(move || transport.open());
                session.connect_via(opener, addr, opts)?;
            }
            (None, LobbyServer::Address(addr)) => {
                session.connect(ConnectTarget::Address(addr), opts)?;
            }
            (None, LobbyServer::Steam(_)) => return Err(LaunchError::NoTransport),
        }
        Ok(server)
    }
}

/// Spec 89 § 4 (ADR-7213): copies `NetSession::manifest()`'s hashes into [`Lobbies`] while the game
/// set no identity ([`Lobbies::follow_manifest`]). Runs in `Startup` (order a `Startup` create
/// `.after(lobby_follow_manifest)`) and at the start of [`lobby_system`]; call it before a
/// `Lobbies::create` that may come earlier. Nothing without both resources.
pub fn lobby_follow_manifest(world: &mut World) {
    // One rule with the pump and the plugin's `build`/`finish` (ADR-7881): no clone on a steady frame.
    let Some(mut lobbies) = world.remove_resource::<Lobbies>() else {
        return;
    };
    adopt_session_identity(world, &mut lobbies, false);
    world.insert_resource(lobbies);
}

/// `PreUpdate`: polls the lobby backend, forwards [`LobbyMessage`]s and auto-connects the
/// session to a launched lobby.
pub fn lobby_system(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_net", "lobby_pump").entered();
    let Some(mut lobbies) = world.remove_resource::<Lobbies>() else {
        return;
    };
    adopt_session_identity(world, &mut lobbies, false);
    let events = lobbies.pump();
    if let Some(mut session) = world.get_resource_mut::<NetSession>() {
        if let Some(Err(e)) = lobbies.auto_connect(&mut session) {
            tracing::warn!(target: "regia_net", "lobby auto-connect failed: {e}");
        }
        lobbies.sync_state(&session);
    }
    world.insert_resource(lobbies);
    for e in events {
        world.write_message(LobbyMessage(e));
    }
}

/// Copies the `NetSession` manifest's hashes into `lobbies` (ADR-7881). With `fill_protocol` (at
/// `finish`), a manifest whose protocol hash is still 0 because this plugin finished before
/// `NetSessionPlugin` filled it in takes the hash `NetSessionPlugin` computes, so a lobby created in
/// `Startup` already carries the value the content handshake will send.
fn adopt_session_identity(world: &World, lobbies: &mut Lobbies, fill_protocol: bool) {
    if lobbies.identity() == LobbyIdentity::Explicit {
        return;
    }
    let Some(session) = world.get_resource::<NetSession>() else {
        return;
    };
    let manifest = session.manifest();
    let changed = if fill_protocol && manifest.protocol_hash == 0 {
        let mut filled = manifest.clone();
        filled.protocol_hash = crate::runtime::protocol_hash(world);
        lobbies.adopt_session_identity(&filled)
    } else {
        lobbies.adopt_session_identity(manifest)
    };
    if changed {
        let (game, protocol, content) = lobbies.identity_values();
        tracing::debug!(
            target: "regia_net",
            game,
            protocol = %hash_hex(protocol),
            content = %hash_hex(content),
            "lobby identity taken from the session manifest"
        );
    }
}

/// Adds [`Lobbies`] (unavailable unless inserted before), [`LobbyMessage`] and the `PreUpdate`
/// pump (spec 89 § 8). Reads the `lobby` section of a [`crate::NetSettings`] resource when present.
///
/// Unless the game called [`Lobbies::set_identity`], the plugin takes the lobby identity's
/// hashes from the app's `NetSession` manifest at `finish` and keeps them equal to it every
/// frame (ADR-7881).
pub struct LobbyPlugin;

impl Plugin for LobbyPlugin {
    fn build(&self, app: &mut App) {
        let settings = app
            .world()
            .get_resource::<crate::NetSettings>()
            .map(|s| s.lobby.clone());
        if !app.world().contains_resource::<Lobbies>() {
            app.insert_resource(Lobbies::default());
        }
        if let Some(settings) = settings {
            app.world_mut().resource_mut::<Lobbies>().set_settings(settings);
        }
        // The session's manifest as it stands; `finish` and every frame bring it up to date.
        let world = app.world_mut();
        if let Some(mut lobbies) = world.remove_resource::<Lobbies>() {
            adopt_session_identity(world, &mut lobbies, false);
            world.insert_resource(lobbies);
        }
        app.add_message::<LobbyMessage>();
        app.add_systems(Startup, lobby_follow_manifest);
        app.add_systems(PreUpdate, lobby_system.after(NetSet::Receive));
    }

    fn finish(&self, app: &mut App) {
        // Before `Startup`: a lobby created there carries the session's identity.
        let world = app.world_mut();
        if let Some(mut lobbies) = world.remove_resource::<Lobbies>() {
            adopt_session_identity(world, &mut lobbies, true);
            world.insert_resource(lobbies);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lobby::{Distance, LocalLobbyHub};

    fn lobbies(hub: &LocalLobbyHub, id: u64) -> Lobbies {
        let mut l = Lobbies::new(Arc::new(hub.member(id, &format!("p{id}"))));
        let manifest = crate::session::ContentManifest::build("e", "g", 1, 0x77, &[]);
        l.set_identity("example.duel", &manifest);
        l
    }

    #[test]
    fn engine_keys_filter_out_other_builds() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        let mut other_build = Lobbies::new(Arc::new(hub.member(2, "old")));
        other_build.set_identity(
            "example.duel",
            &crate::session::ContentManifest::build("e", "g", 1, 0x66, &[]),
        );
        let mut spec = a.default_spec();
        spec.data.insert("regia.state".into(), "x".into());
        assert_eq!(a.create(spec), Err(LobbyError::ReservedKey));
        a.create(a.default_spec()).expect("create");
        a.pump();
        let lobby = a.current().expect("in lobby");
        assert_eq!(
            a.snapshot().map(|s| s.data[keys::PROTOCOL].clone()),
            Some(hash_hex(0x77))
        );
        assert_eq!(a.create(a.default_spec()), Err(LobbyError::AlreadyInLobby));
        other_build.list(LobbyQuery::default()).expect("list");
        other_build.pump();
        assert_eq!(other_build.last_list(), Some(&Ok(Vec::new())), "protocol differs");
        let mut b = lobbies(&hub, 3);
        b.list(LobbyQuery::default()).expect("list");
        b.pump();
        let found: Vec<LobbyId> = b
            .last_list()
            .and_then(|r| r.as_ref().ok())
            .map(|v| v.iter().map(|l| l.id).collect())
            .unwrap_or_default();
        assert_eq!(found, [lobby]);
        assert_eq!(b.set_data("map", Some("x")), Err(LobbyError::NotInLobby));
        b.join(lobby).expect("join");
        b.pump();
        assert_eq!(b.current(), Some(lobby));
        assert!(!b.is_owner());
        assert_eq!(b.set_data("map", Some("x")), Err(LobbyError::NotOwner));
        assert_eq!(
            b.set_member_data(keys::READY, Some("1")),
            Err(LobbyError::ReservedKey)
        );
        b.set_ready(true).expect("ready");
        a.set_ready(true).expect("ready");
        a.pump();
        assert!(a.ready_to_launch());
        assert_eq!(b.launch(LobbyServer::Steam(9)), Err(LobbyError::NotOwner));
        a.launch(LobbyServer::Steam(9)).expect("launch");
        a.pump();
        b.pump();
        let snap = b.snapshot().expect("snapshot");
        assert_eq!(snap.server, Some(LobbyServer::Steam(9)));
        assert_eq!(snap.data[keys::STATE], "launching");
        assert!(!snap.joinable, "closed after launch");
        b.leave();
        assert_eq!(b.current(), None);
        assert!(b.recent_events().count() > 0);
    }

    #[test]
    fn a_failed_auto_connect_is_not_reported_as_connected_and_not_retried() {
        let hub = LocalLobbyHub::new();
        let mut owner = lobbies(&hub, 1);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let mut member = lobbies(&hub, 2);
        member.join(lobby).expect("join");
        member.pump();
        // A Steam server and no transport installed: the member cannot reach it.
        owner.launch(LobbyServer::Steam(9)).expect("launch");
        owner.pump();
        member.pump();
        let mut session = NetSession::default();
        let tried = member.auto_connect(&mut session).expect("one attempt");
        assert_eq!(tried.map_err(|e| e.code()), Err("no_transport"));
        assert_eq!(
            member.connected_to(),
            None,
            "a failed attempt is not a connection"
        );
        assert!(member.last_launch_error().is_some());
        member.pump();
        assert!(
            member.auto_connect(&mut session).is_none(),
            "not retried every frame"
        );
        member.leave();
        assert_eq!(
            member.last_launch_error(),
            None,
            "the error belongs to the lobby left"
        );
        // The owner never auto-connects to its own server.
        assert!(owner.auto_connect(&mut session).is_none());
    }

    #[test]
    fn leaving_with_a_create_or_join_in_flight_leaves_where_it_lands() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        a.create(a.default_spec()).expect("create");
        a.leave();
        a.pump();
        assert_eq!(a.current(), None);
        assert!(
            hub.lobbies().is_empty(),
            "the created lobby was left when it landed"
        );

        let mut owner = lobbies(&hub, 2);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let mut b = lobbies(&hub, 3);
        b.join(lobby).expect("join");
        b.leave();
        b.pump();
        assert_eq!(b.current(), None);
        owner.pump();
        assert_eq!(
            owner.snapshot().map(|s| s.member_count),
            Some(1),
            "b is not left inside"
        );
    }

    #[test]
    fn a_lobby_lost_without_a_left_event_clears_the_connect_state_and_fails_matchmaking() {
        let hub = LocalLobbyHub::new();
        let mut owner = lobbies(&hub, 1);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let mut member = lobbies(&hub, 2);
        // A second handle on the member: it swallows the `Left` event a real platform can lose.
        let side = hub.member(2, "p2");
        let mut req = member.default_request();
        req.rounds.truncate(1);
        req.create_if_none = false;
        member.start_matchmaking(req).expect("matchmaking");
        member.pump();
        member.pump();
        assert_eq!(member.current(), Some(lobby));
        owner.launch(LobbyServer::Steam(9)).expect("launch");
        owner.pump();
        member.pump();
        let mut session = NetSession::default();
        assert!(member.auto_connect(&mut session).is_some(), "one attempt");
        assert!(member.last_launch_error().is_some());

        side.leave(lobby);
        side.poll();
        member.pump();
        assert_eq!(member.current(), None);
        assert_eq!(
            member.last_launch_error(),
            None,
            "the error belonged to that lobby"
        );
        assert_eq!(
            *member.matchmaker().state(),
            MatchState::Failed(crate::lobby::MatchFailure::Left)
        );

        // The next lobby gets its own attempt.
        let mut other = lobbies(&hub, 3);
        other.create(other.default_spec()).expect("create");
        other.pump();
        member.join(other.current().expect("in lobby")).expect("join");
        member.pump();
        other.launch(LobbyServer::Steam(8)).expect("launch");
        other.pump();
        member.pump();
        assert!(
            member.auto_connect(&mut session).is_some(),
            "a lost lobby must not block auto-connect in the next one"
        );
    }

    #[test]
    fn a_left_event_clears_the_last_launch_error() {
        let hub = LocalLobbyHub::new();
        let mut owner = lobbies(&hub, 1);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let mut member = lobbies(&hub, 2);
        let side = hub.member(2, "p2");
        member.join(lobby).expect("join");
        member.pump();
        owner.launch(LobbyServer::Steam(9)).expect("launch");
        owner.pump();
        member.pump();
        let mut session = NetSession::default();
        assert!(member.auto_connect(&mut session).is_some(), "one attempt");
        // Removed behind `Lobbies`' back: this time the event arrives.
        side.leave(lobby);
        member.pump();
        assert_eq!(member.current(), None);
        assert_eq!(member.last_launch_error(), None);
    }

    #[test]
    fn cancel_matchmaking_keeps_a_lobby_joined_by_hand() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        a.create(a.default_spec()).expect("create");
        a.pump();
        let lobby = a.current().expect("in lobby");
        a.cancel_matchmaking();
        assert_eq!(a.current(), Some(lobby), "not matchmaking: nothing to cancel");
        assert_eq!(hub.lobbies().len(), 1);
    }

    #[test]
    fn a_new_backend_does_not_inherit_abandoned_requests() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        a.create(a.default_spec()).expect("create");
        let other = LocalLobbyHub::new();
        a.set_backend(Arc::new(other.member(1, "p1")));
        assert!(a.matchmaker().abandoned().is_empty());
        // Each hub counts requests from the same start, so this id equals the abandoned one.
        a.create(a.default_spec()).expect("create on the new backend");
        a.pump();
        assert!(a.current().is_some(), "the new lobby is not left");
        assert_eq!(other.lobbies().len(), 1);
    }

    #[test]
    fn a_launch_that_cannot_publish_leaves_no_session_hosting() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        a.set_transport(Arc::new(LoopbackLobbyTransport::default()));
        a.create(a.default_spec()).expect("create");
        a.pump();
        let lobby = a.current().expect("in lobby");
        // Gone since the last pump, so the snapshot still says we own it.
        hub.member(1, "p1").leave(lobby);
        let mut session = NetSession::default();
        let refused = a.launch_hosting(&mut session, HostOptions::default());
        assert_eq!(refused.map_err(|e| e.code()), Err("not_in_lobby"));
        assert!(session.server().is_none(), "the retry must not find a stale host");
    }

    #[test]
    fn launching_becomes_in_game_only_when_the_session_plays() {
        let hub = LocalLobbyHub::new();
        let state = |l: &Lobbies| l.snapshot().map(|s| s.data[keys::STATE].clone());
        // Published a session hosted elsewhere that the owner is not in: nothing shows play.
        let mut elsewhere = lobbies(&hub, 1);
        elsewhere.create(elsewhere.default_spec()).expect("create");
        elsewhere.pump();
        elsewhere.launch(LobbyServer::Steam(9)).expect("launch");
        elsewhere.pump();
        elsewhere.sync_state(&NetSession::default());
        elsewhere.pump();
        assert_eq!(state(&elsewhere).as_deref(), Some("launching"));

        // Hosted with another member who has not reached the game yet.
        let net = crate::LoopbackNet::default();
        let mut owner = lobbies(&hub, 2);
        owner.set_transport(Arc::new(LoopbackLobbyTransport::new(net.clone())));
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let mut member = lobbies(&hub, 3);
        member.join(owner.current().expect("in lobby")).expect("join");
        member.pump();
        owner.pump();
        let mut session = NetSession::default();
        owner
            .launch_hosting(&mut session, HostOptions::default())
            .expect("launch");
        owner.pump();
        owner.sync_state(&session);
        owner.pump();
        assert_eq!(state(&owner).as_deref(), Some("launching"));
        // A member's `Lobbies` never writes the owner's key.
        member.pump();
        member.sync_state(&NetSession::default());

        // Alone in the lobby, the host is the game.
        let mut solo = lobbies(&hub, 4);
        solo.set_transport(Arc::new(LoopbackLobbyTransport::new(net)));
        solo.create(solo.default_spec()).expect("create");
        solo.pump();
        let mut solo_session = NetSession::default();
        solo.launch_hosting(&mut solo_session, HostOptions::default())
            .expect("launch");
        solo.pump();
        solo.sync_state(&solo_session);
        solo.pump();
        assert_eq!(state(&solo).as_deref(), Some("in_game"));
    }

    #[test]
    fn recent_events_are_bounded() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        for _ in 0..(RECENT_EVENTS + 10) {
            a.list(LobbyQuery::default()).expect("list");
        }
        a.pump();
        assert_eq!(a.recent_events().count(), RECENT_EVENTS);
    }

    #[test]
    fn settings_section_defaults_and_parses() {
        let s: LobbySettings = ron::from_str("(max_members: 4, rounds: [Close])").expect("parse");
        assert_eq!(s.max_members, 4);
        assert_eq!(s.rounds, vec![Distance::Close]);
        assert_eq!(s.min_players, 2);
        let net = crate::NetSettings::from_ron("(lobby: (auto_connect: false))").expect("net.ron");
        assert!(!net.lobby.auto_connect);
        assert_eq!(crate::NetSettings::default().lobby, LobbySettings::default());
    }

    /// Spec 89 § 9: `matchmaking.start` checks the create spec when it starts, as `create` does,
    /// instead of failing rounds later as `create_failed`.
    #[test]
    fn matchmaking_refuses_a_bad_create_spec_up_front() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        for max in [0, crate::lobby::MAX_LOBBY_MEMBERS + 1] {
            let mut req = a.default_request();
            req.create.max_members = max;
            assert_eq!(a.start_matchmaking(req), Err(LobbyError::MembersInvalid), "{max}");
            assert_eq!(*a.matchmaker().state(), MatchState::Idle, "{max}");
        }
        // The engine keys count toward the limit: a full set of game keys leaves no room.
        let mut req = a.default_request();
        for i in 0..crate::lobby::MAX_KEYS {
            req.create.data.insert(format!("k{i}"), "v".into());
        }
        assert_eq!(a.start_matchmaking(req), Err(LobbyError::TooManyKeys));
        assert_eq!(*a.matchmaker().state(), MatchState::Idle);
        a.start_matchmaking(a.default_request())
            .expect("a good request starts");
    }

    fn listed(l: &mut Lobbies) -> Vec<LobbyId> {
        l.list(LobbyQuery::default()).expect("list");
        l.pump();
        l.last_list()
            .and_then(|r| r.as_ref().ok())
            .map(|v| v.iter().map(|i| i.id).collect())
            .unwrap_or_default()
    }

    /// Spec 89 § 4: with `match_content: false` a build lists lobbies whose content hash differs
    /// (the protocol still has to match).
    #[test]
    fn match_content_false_lists_lobbies_of_other_content() {
        let hub = LocalLobbyHub::new();
        let mut owner = lobbies(&hub, 1);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let mut manifest = crate::session::ContentManifest::build("e", "g", 1, 0x77, &[]);
        manifest.content_hash ^= 0xFF;
        let mut other = Lobbies::new(Arc::new(hub.member(2, "p2")));
        other.set_identity("example.duel", &manifest);
        assert!(listed(&mut other).is_empty(), "content differs");
        other.set_settings(LobbySettings {
            match_content: false,
            ..other.settings().clone()
        });
        assert_eq!(listed(&mut other), [lobby]);
        manifest.protocol_hash ^= 0xFF;
        other.set_identity("example.duel", &manifest);
        assert!(listed(&mut other).is_empty(), "the protocol always has to match");
    }

    /// Spec 89 § 5: with `joinable_in_game` the launched lobby stays joinable, and a late
    /// matchmaker lands straight in `Launched`.
    #[test]
    fn joinable_in_game_keeps_the_lobby_open_after_launch() {
        let hub = LocalLobbyHub::new();
        let mut owner = lobbies(&hub, 1);
        owner.set_settings(LobbySettings {
            joinable_in_game: true,
            ..owner.settings().clone()
        });
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let server = LobbyServer::Steam(9);
        owner.launch(server).expect("launch");
        owner.pump();
        let snap = owner.snapshot().expect("snapshot");
        assert!(snap.joinable, "open in game");
        assert_eq!(snap.data[keys::STATE], "launching");
        let mut late = lobbies(&hub, 2);
        late.start_matchmaking(late.default_request())
            .expect("matchmaking");
        late.pump();
        late.pump();
        assert_eq!(*late.matchmaker().state(), MatchState::Launched { lobby, server });
    }

    /// Spec 89 § 5 (review rev-claude-6-k finding 1): a join-only request
    /// (`create_if_none: false`) never sends its create spec, so a bad `lobby.max_members` in the
    /// settings must not refuse it.
    #[test]
    fn rev_join_only_matchmaking_ignores_the_unused_create_spec() {
        let hub = LocalLobbyHub::new();
        let mut a = lobbies(&hub, 1);
        a.set_settings(LobbySettings {
            max_members: 0,
            create_if_none: false,
            ..a.settings().clone()
        });
        let req = a.default_request();
        assert!(!req.create_if_none);
        assert_eq!(a.start_matchmaking(req), Ok(()));
    }

    /// Spec 89 § 5: a join-only matchmaker with an unusable create spec (members 0 or over the
    /// limit, too many keys) still lists and joins an open lobby; the same spec with
    /// `create_if_none` is still refused up front.
    #[test]
    fn join_only_matchmaking_with_a_bad_create_spec_lists_and_joins() {
        let hub = LocalLobbyHub::new();
        let mut owner = lobbies(&hub, 1);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let too_many: BTreeMap<String, String> = (0..crate::lobby::MAX_KEYS)
            .map(|i| (format!("k{i}"), "v".to_owned()))
            .collect();
        let bad = [
            (0, BTreeMap::new(), LobbyError::MembersInvalid),
            (
                crate::lobby::MAX_LOBBY_MEMBERS + 1,
                BTreeMap::new(),
                LobbyError::MembersInvalid,
            ),
            (4, too_many, LobbyError::TooManyKeys),
        ];
        for (i, (max_members, data, code)) in bad.into_iter().enumerate() {
            let mut joiner = lobbies(&hub, 10 + i as u64);
            let mut req = joiner.default_request();
            req.create.max_members = max_members;
            req.create.data = data;
            let mut creating = req.clone();
            creating.create_if_none = true;
            assert_eq!(
                joiner.start_matchmaking(creating),
                Err(code),
                "{i}: create_if_none"
            );
            assert_eq!(*joiner.matchmaker().state(), MatchState::Idle, "{i}");
            req.create_if_none = false;
            assert_eq!(joiner.start_matchmaking(req), Ok(()), "{i}: join only");
            for _ in 0..4 {
                joiner.pump();
            }
            assert_eq!(joiner.current(), Some(lobby), "{i}: joined the open lobby");
            joiner.leave();
            joiner.pump();
        }
    }

    /// A backend wrapper that refuses one scripted publish step, the way Steam refuses a
    /// `SetLobbyData`/`SetLobbyJoinable` write (rate limit, per-key limit) mid-launch.
    enum Refusal {
        /// Refuse the first `set_data` whose value equals the string.
        Data(&'static str),
        /// Refuse the first `set_joinable(false)`.
        Close,
    }

    struct ScriptBackend {
        inner: Arc<dyn LobbyBackend>,
        refusal: std::sync::Mutex<Option<Refusal>>,
        /// A platform game-server field, written by `set_server` like Steam's
        /// `SetLobbyGameServer` (KI-NT-050).
        platform_server: std::sync::Mutex<Option<LobbyServer>>,
    }

    impl LobbyBackend for ScriptBackend {
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn local_member(&self) -> Option<MemberId> {
            self.inner.local_member()
        }
        fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError> {
            self.inner.create(spec)
        }
        fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
            self.inner.join(lobby)
        }
        fn leave(&self, lobby: LobbyId) {
            self.inner.leave(lobby);
        }
        fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError> {
            self.inner.request_list(query)
        }
        fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo> {
            self.inner.lobby(lobby)
        }
        fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
            let refuse = value.is_some_and(|v| {
                matches!(
                    self.refusal.lock().unwrap_or_else(|e| e.into_inner()).as_ref(),
                    Some(Refusal::Data(needle)) if *needle == v
                )
            });
            if refuse {
                self.refusal.lock().unwrap_or_else(|e| e.into_inner()).take();
                return Err(LobbyError::Platform("SetLobbyData refused".into()));
            }
            self.inner.set_data(lobby, key, value)
        }
        fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
            self.inner.set_member_data(lobby, key, value)
        }
        fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError> {
            let refuse = !joinable
                && matches!(
                    self.refusal.lock().unwrap_or_else(|e| e.into_inner()).as_ref(),
                    Some(Refusal::Close)
                );
            if refuse {
                self.refusal.lock().unwrap_or_else(|e| e.into_inner()).take();
                return Err(LobbyError::Platform("SetLobbyJoinable refused".into()));
            }
            self.inner.set_joinable(lobby, joinable)
        }
        fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError> {
            *self.platform_server.lock().unwrap_or_else(|e| e.into_inner()) = Some(server);
            self.inner.set_server(lobby, server)
        }
        fn clear_server(&self, lobby: LobbyId) -> Result<(), LobbyError> {
            self.platform_server
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            self.inner.clear_server(lobby)
        }
        fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError> {
            self.inner.send_chat(lobby, text)
        }
        fn poll(&self) -> Vec<LobbyEvent> {
            self.inner.poll()
        }
    }

    fn scripted(hub: &LocalLobbyHub, id: u64, refusal: Refusal) -> (Lobbies, LobbyId) {
        let (l, lobby, _) = scripted_backend(hub, id, refusal);
        (l, lobby)
    }

    fn scripted_backend(
        hub: &LocalLobbyHub,
        id: u64,
        refusal: Refusal,
    ) -> (Lobbies, LobbyId, Arc<ScriptBackend>) {
        let inner: Arc<dyn LobbyBackend> = Arc::new(hub.member(id, &format!("p{id}")));
        let backend = Arc::new(ScriptBackend {
            inner,
            refusal: std::sync::Mutex::new(Some(refusal)),
            platform_server: std::sync::Mutex::new(None),
        });
        let mut l = Lobbies::new(backend.clone());
        l.allow_unidentified(true);
        l.create(l.default_spec()).expect("create");
        l.pump();
        let lobby = l.current().expect("in lobby");
        assert!(l.is_owner());
        (l, lobby, backend)
    }

    /// KI-NT-050 (ADR-10069): a backend that also writes a platform field in `set_server` (the
    /// real Steam backend fills `SetLobbyGameServer`, which the overlay and friends list read) has
    /// that field withdrawn by the rollback too, not only the `regia.server` key.
    #[test]
    fn a_rolled_back_launch_clears_the_platform_game_server_field() {
        for refusal in [Refusal::Data("launching"), Refusal::Close] {
            let hub = LocalLobbyHub::new();
            let (mut owner, lobby, backend) = scripted_backend(&hub, 1, refusal);
            owner
                .launch(LobbyServer::Steam(9))
                .expect_err("a publish step is refused");
            assert_eq!(
                *backend.platform_server.lock().unwrap_or_else(|e| e.into_inner()),
                None,
                "the platform game-server field still names the dead session"
            );
            assert!(
                hub.member(1, "owner")
                    .lobby(lobby)
                    .expect("lobby")
                    .server
                    .is_none()
            );
        }
    }

    /// Finding 2 (deep-zai net-lobby): `launch` publishes in three backend writes (server key
    /// first, then `regia.state = launching`, then the joinable flag). A refusal after the server
    /// key went out must roll the publish back — otherwise members spend their single
    /// auto-connect attempt on a session the caller is tearing down, and the lobby keeps
    /// advertising a dead server.
    #[test]
    fn a_refused_state_write_rolls_back_the_server_key() {
        let hub = LocalLobbyHub::new();
        let (mut owner, lobby) = scripted(&hub, 1, Refusal::Data("launching"));
        let err = owner
            .launch(LobbyServer::Steam(9))
            .expect_err("the state write is refused");
        assert_eq!(err.code(), "lobby_platform");
        let seen = hub.member(1, "owner").lobby(lobby).expect("the lobby");
        assert!(
            seen.server.is_none(),
            "no dead server key stays behind for members to auto-connect to"
        );
        assert_eq!(
            seen.data.get(keys::STATE).map(String::as_str),
            Some("open"),
            "the state stays what it was"
        );
    }

    /// Finding 2 (deep-zai net-lobby): the third publish step (closing the lobby to joins) failing
    /// must roll the whole publish back too — not leave a `launching` lobby that still carries the
    /// server key and its joinable flag set.
    #[test]
    fn a_refused_joinable_write_rolls_back_the_publish() {
        let hub = LocalLobbyHub::new();
        let (mut owner, lobby) = scripted(&hub, 1, Refusal::Close);
        let err = owner
            .launch(LobbyServer::Steam(9))
            .expect_err("the joinable write is refused");
        assert_eq!(err.code(), "lobby_platform");
        let seen = hub.member(1, "owner").lobby(lobby).expect("the lobby");
        assert!(seen.server.is_none(), "the server key is rolled back");
        assert_eq!(
            seen.data.get(keys::STATE).map(String::as_str),
            Some("open"),
            "not left `launching` without a server"
        );
        assert!(seen.joinable, "joinable stays as it was");
    }

    /// Finding 2 (deep-zai net-lobby): `launch_hosting` tears the just-hosted session down when
    /// the publish fails; the lobby must not advertise that dead session either.
    #[test]
    fn launch_hosting_unpublishes_a_failed_launch() {
        let hub = LocalLobbyHub::new();
        let inner: Arc<dyn LobbyBackend> = Arc::new(hub.member(1, "owner"));
        let mut owner = Lobbies::new(Arc::new(ScriptBackend {
            inner,
            refusal: std::sync::Mutex::new(Some(Refusal::Data("launching"))),
            platform_server: std::sync::Mutex::new(None),
        }));
        owner.set_transport(Arc::new(LoopbackLobbyTransport::default()));
        owner.allow_unidentified(true);
        owner.create(owner.default_spec()).expect("create");
        owner.pump();
        let lobby = owner.current().expect("in lobby");
        let mut session = NetSession::default();
        let err = owner
            .launch_hosting(&mut session, HostOptions::default())
            .expect_err("the publish fails");
        assert_eq!(err.code(), "lobby_platform");
        assert!(session.server().is_none(), "no session is left hosting");
        let seen = hub.member(1, "owner").lobby(lobby).expect("the lobby");
        assert!(
            seen.server.is_none(),
            "the lobby does not advertise the torn-down session"
        );
    }
}
