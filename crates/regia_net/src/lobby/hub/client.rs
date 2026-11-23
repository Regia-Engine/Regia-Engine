// SPDX-License-Identifier: MPL-2.0
//! [`HubLobbyBackend`]: the hub client, a [`LobbyBackend`] over a [`NetClient`] pinned to the
//! hub's [`ServerKey`].

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::join::{JoinPlan, ReachKind, ReachOffer};
use super::wire::{ChatScope, DirectoryPage, HubEvent, HubMessage, HubRequest, HubWire, RoomState};
use super::{HubError, HubLobbyCard, HubPresence, InviteCard, PresenceRow};
use crate::client::{ClientConfig, ClientEnd, ClientEvent, NetClient};
use crate::connection::Channel;
use crate::crypto::{CryptoMode, PlayerIdentity, PlayerKey, ServerKey};
use crate::lobby::{
    LeaveReason, LobbyBackend, LobbyError, LobbyEvent, LobbyId, LobbyInfo, LobbyMember, LobbyQuery,
    LobbyServer, LobbySpec, MemberId, RequestId, check_chat, check_key, check_spec, check_value, keys,
};
use crate::socket::DatagramSocket;

/// Events kept for [`LobbyBackend::poll`]; older ones are dropped first.
const MAX_EVENTS: usize = 1024;
/// Invites kept; the oldest goes first.
const MAX_INVITES: usize = 64;
/// Global chat lines kept for [`HubLobbyBackend::take_global_chat`].
const MAX_GLOBAL_CHAT: usize = 256;
/// Requests queued while the link is still connecting.
const MAX_QUEUED: usize = 64;
/// Requests waiting for an answer.
const MAX_PENDING: usize = 256;
/// Reach asks (owner) and plans (both sides) kept until taken.
const MAX_REACH: usize = 256;

/// A member's ask for a path, as the owner sees it ([`HubLobbyBackend::reach_asks`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReachAsk {
    /// The lobby.
    pub lobby: LobbyId,
    /// Who asks.
    pub member: MemberId,
    /// Their handle.
    pub handle: String,
    /// Their player key, as their hub transport proved it.
    pub identity: PlayerKey,
    /// Their game socket's candidates and NAT behavior.
    pub offer: ReachOffer,
}

/// The clock a [`HubLobbyBackend`] pumps on.
#[derive(Debug, Clone)]
pub enum HubClock {
    /// Milliseconds since the clock was made (the runtime default where std has a monotonic
    /// clock).
    Monotonic(Instant),
    /// A shared value the owner sets (tests, replays, a host-driven frame clock, and the default
    /// on wasm32).
    Manual(Arc<AtomicU64>),
}

/// Whether std has a monotonic clock on this target. On `wasm32-unknown-unknown`,
/// `Instant::now` panics (there is no clock without the browser), so the hub client must not
/// read it there (ADR-8891).
const STD_HAS_MONOTONIC_CLOCK: bool = !cfg!(target_arch = "wasm32");

impl Default for HubClock {
    /// [`HubClock::Monotonic`] where std has a monotonic clock. On wasm32 a manual clock at 0:
    /// the host advances it with [`HubClock::set_ms`] (from the browser's frame clock), because
    /// std's `Instant::now` panics there.
    fn default() -> Self {
        Self::for_target(STD_HAS_MONOTONIC_CLOCK)
    }
}

impl HubClock {
    /// The default clock for a target with (`true`) or without std's monotonic clock. Without
    /// one, `Instant::now` is never called.
    fn for_target(std_has_monotonic_clock: bool) -> Self {
        if std_has_monotonic_clock {
            Self::Monotonic(Instant::now())
        } else {
            Self::manual(0)
        }
    }

    /// A manual clock at `ms`.
    pub fn manual(ms: u64) -> Self {
        Self::Manual(Arc::new(AtomicU64::new(ms)))
    }

    /// Sets a manual clock (no-op on a monotonic one).
    pub fn set_ms(&self, ms: u64) {
        if let Self::Manual(v) = self {
            v.store(ms, Ordering::Relaxed);
        }
    }

    /// Now, in milliseconds.
    pub fn now_ms(&self) -> u64 {
        match self {
            Self::Monotonic(start) => u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
            Self::Manual(v) => v.load(Ordering::Relaxed),
        }
    }
}

/// How a [`HubLobbyBackend`] connects.
#[derive(Debug, Clone, Default)]
pub struct HubClientConfig {
    /// The handle this session takes on the hub.
    pub handle: String,
    /// The hub's public key. Required: [`HubLobbyBackend::connect`] refuses `None` with
    /// [`HubError::KeyUnpinned`], and a hub answering under another key ends the link with
    /// `hub_key_mismatch`.
    pub server_key: Option<ServerKey>,
    /// The clock [`LobbyBackend::poll`] pumps on.
    pub clock: HubClock,
    /// Heartbeat period in milliseconds; `None` sends one every third of the hub's idle timeout.
    pub heartbeat_ms: Option<u64>,
    /// Endpoint tuning (timeouts, conditioner). Its name, crypto mode and key are the hub's.
    pub link: ClientConfig,
    /// The player's long-term key pair: the session's identity on the hub. `None` makes a fresh
    /// one for this connection (an anonymous player; a reconnect is then a new player).
    pub identity: Option<PlayerIdentity>,
    /// As a lobby owner, approve every member's ask for a path at once (otherwise
    /// [`HubLobbyBackend::approve`]).
    pub auto_approve: bool,
}

/// The link to the hub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubLinkState {
    /// The handshake or the hello is in flight.
    Connecting,
    /// The hub allocated our session.
    Connected,
    /// The link ended; the code says why (`hub_key_mismatch`, or the transport's code such as
    /// `connect_timeout`, or a hub refusal of the hello such as `hub_handle_taken`).
    Ended(String),
}

enum Pending {
    Create,
    Join,
    List(LobbyQuery),
}

struct Inner<S: DatagramSocket> {
    client: NetClient<HubWire, S>,
    identity: PlayerIdentity,
    server_key: ServerKey,
    auto_approve: bool,
    asks: VecDeque<ReachAsk>,
    plans: VecDeque<JoinPlan>,
    clock: HubClock,
    link: HubLinkState,
    handle: String,
    member: Option<MemberId>,
    heartbeat_ms: Option<u64>,
    idle_ms: u64,
    last_sent_ms: u64,
    next_request: u64,
    pending: BTreeMap<u64, Pending>,
    queued: Vec<HubRequest>,
    rooms: BTreeMap<LobbyId, LobbyInfo>,
    directory: BTreeMap<LobbyId, HubLobbyCard>,
    players: BTreeMap<MemberId, PresenceRow>,
    dir_gen: u64,
    presence_gen: u64,
    invites: VecDeque<InviteCard>,
    global_chat: VecDeque<(MemberId, String, String)>,
    events: VecDeque<LobbyEvent>,
    last_error: Option<String>,
}

/// A [`LobbyBackend`] served by a self-hosted hub ([`super::HubHost`]): lobbies, a directory with
/// presence, and invites, with no platform account. Talks to the hub over a [`NetClient`] sealed and
/// pinned to the hub's key; [`LobbyBackend::poll`] pumps the link on the configured clock.
///
/// Beyond the trait: [`HubLobbyBackend::invite`] and [`HubLobbyBackend::invites`] (cards carry a
/// lobby id and a token, never an address; [`LobbyBackend::join`] presents a held token itself),
/// presence and friends, global chat, and the mirrored directory.
pub struct HubLobbyBackend<S: DatagramSocket> {
    inner: Mutex<Inner<S>>,
}

impl<S: DatagramSocket> std::fmt::Debug for HubLobbyBackend<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        f.debug_struct("HubLobbyBackend")
            .field("link", &inner.link)
            .field("member", &inner.member)
            .field("rooms", &inner.rooms.len())
            .finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> HubLobbyBackend<S> {
    /// Starts connecting to the hub at `hub` over `socket`. Refuses an unpinned key
    /// ([`HubError::KeyUnpinned`]) and an invalid handle before any datagram is sent.
    pub fn connect(socket: S, hub: SocketAddr, config: HubClientConfig) -> Result<Self, HubError> {
        let key = config.server_key.ok_or(HubError::KeyUnpinned)?;
        let handle = super::clean_handle(&config.handle, super::HANDLE_LIMIT_BYTES)?;
        let link = ClientConfig {
            name: handle.clone(),
            crypto: CryptoMode::Required,
            server_key: Some(key),
            ..config.link
        };
        let identity = match config.identity {
            Some(i) => i,
            None => PlayerIdentity::generate().map_err(|_| HubError::Disconnected)?,
        };
        let client = NetClient::connect(socket, hub, link, &()).map_err(|_| HubError::HandleInvalid)?;
        Ok(Self {
            inner: Mutex::new(Inner {
                client,
                identity,
                server_key: key,
                auto_approve: config.auto_approve,
                asks: VecDeque::new(),
                plans: VecDeque::new(),
                clock: config.clock,
                link: HubLinkState::Connecting,
                handle,
                member: None,
                heartbeat_ms: config.heartbeat_ms,
                idle_ms: super::HubSettings::default().idle_ms,
                last_sent_ms: 0,
                next_request: 0,
                pending: BTreeMap::new(),
                queued: Vec::new(),
                rooms: BTreeMap::new(),
                directory: BTreeMap::new(),
                players: BTreeMap::new(),
                dir_gen: 0,
                presence_gen: 0,
                invites: VecDeque::new(),
                global_chat: VecDeque::new(),
                events: VecDeque::new(),
                last_error: None,
            }),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner<S>> {
        // A panic while holding the lock leaves plain data behind; keep serving it.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Receives and applies hub events, heartbeats, and sends. [`LobbyBackend::poll`] calls it on
    /// the configured clock; call it yourself to drive the link at another time.
    pub fn pump(&self, now_ms: u64) {
        self.lock().pump(now_ms);
    }

    /// The link state.
    pub fn link_state(&self) -> HubLinkState {
        self.lock().link.clone()
    }

    /// Whether the link is sealed (always, once connected: the hub requires encryption).
    pub fn is_sealed(&self) -> bool {
        self.lock().client.is_sealed()
    }

    /// The last error code the hub sent (`hub_*` or a lobby code).
    pub fn last_error(&self) -> Option<String> {
        self.lock().last_error.clone()
    }

    /// Invites `handle` to `lobby` (owner only); the hub sends the invitee a card.
    pub fn invite(&self, lobby: LobbyId, handle: &str) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.owned(lobby)?;
        let handle = super::clean_handle(handle, super::HANDLE_LIMIT_BYTES).map_err(|e| e.to_lobby())?;
        inner.send(HubRequest::Invite { lobby, handle })
    }

    /// The invites received and not yet used, oldest first.
    pub fn invites(&self) -> Vec<InviteCard> {
        self.lock().invites.iter().cloned().collect()
    }

    /// Sets this session's presence.
    pub fn set_presence(&self, presence: HubPresence) -> Result<(), LobbyError> {
        self.lock().send(HubRequest::SetPresence(presence))
    }

    /// Sets this session's friends by player key (who may join its friends-only lobbies). A friend's
    /// key is its [`HubLobbyBackend::identity`], shared out of band (a friend code); handles are
    /// display names and never admit anyone (ADR-8633).
    pub fn set_friends(&self, friends: &[crate::crypto::PlayerKey]) -> Result<(), LobbyError> {
        if friends.len() > super::FRIENDS_LIMIT {
            return Err(HubError::TooManyFriends.to_lobby());
        }
        self.lock().send(HubRequest::SetFriends(friends.to_vec()))
    }

    /// Mutes or unmutes a member in lobby chat (owner only).
    pub fn mute(&self, lobby: LobbyId, member: MemberId, muted: bool) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.owned(lobby)?;
        inner.send(HubRequest::Mute { lobby, member, muted })
    }

    /// Sends a line to every session on the hub.
    pub fn send_global_chat(&self, text: &str) -> Result<(), LobbyError> {
        check_chat(text)?;
        self.lock().send(HubRequest::Chat {
            scope: ChatScope::Global,
            text: text.to_owned(),
        })
    }

    /// Drains the global chat lines received: (sender, handle, text).
    pub fn take_global_chat(&self) -> Vec<(MemberId, String, String)> {
        self.lock().global_chat.drain(..).collect()
    }

    /// This session's player key (its identity on the hub).
    pub fn identity(&self) -> PlayerKey {
        self.lock().identity.public()
    }

    /// As the owner of `lobby`, offers how the game session's socket is reached (join brokering,
    /// [`super::join`]): its candidates (a port mapping as a `mapped` candidate), its NAT behavior
    /// when known, the relay, its join code and the join ticket.
    pub fn offer_reach(&self, lobby: LobbyId, offer: ReachOffer) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.owned(lobby)?;
        inner.send(HubRequest::Offer { lobby, offer })
    }

    /// As a member of `lobby`, asks for a path to the owner's game session with this side's game
    /// socket candidates and NAT behavior. The plan arrives in [`HubLobbyBackend::take_plans`] once
    /// the owner approved.
    pub fn ask_reach(&self, lobby: LobbyId, offer: ReachOffer) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.member_of(lobby)?;
        inner.send(HubRequest::AskReach { lobby, offer })
    }

    /// As the owner, approves or denies a member's ask.
    pub fn approve(&self, lobby: LobbyId, member: MemberId, approve: bool) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.owned(lobby)?;
        inner.asks.retain(|a| !(a.lobby == lobby && a.member == member));
        inner.send(HubRequest::Approve {
            lobby,
            member,
            approve,
        })
    }

    /// The asks waiting for this owner's approval, oldest first.
    pub fn reach_asks(&self) -> Vec<ReachAsk> {
        self.lock().asks.iter().cloned().collect()
    }

    /// Drains the plans the hub sent (as a member: how to reach the owner; as the owner: who is
    /// coming, with their candidates).
    pub fn take_plans(&self) -> Vec<JoinPlan> {
        self.lock().plans.drain(..).collect()
    }

    /// As a member, reports the path that carried the sealed handshake to the owner's session;
    /// also emits [`LobbyEvent::Reached`] locally.
    pub fn report_reach(
        &self,
        lobby: LobbyId,
        path: ReachKind,
        punch_attempts: u32,
    ) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.member_of(lobby)?;
        if let Some(member) = inner.member {
            inner.event(LobbyEvent::Reached { lobby, member, path });
        }
        inner.send(HubRequest::ReachDone {
            lobby,
            path,
            punch_attempts,
        })
    }

    /// The mirrored presence rows, in member order.
    pub fn players(&self) -> Vec<PresenceRow> {
        self.lock().players.values().cloned().collect()
    }

    /// The mirrored directory cards, in id order.
    pub fn directory(&self) -> Vec<HubLobbyCard> {
        self.lock().directory.values().cloned().collect()
    }

    /// The directory and presence generations the mirror is at.
    pub fn generations(&self) -> (u64, u64) {
        let inner = self.lock();
        (inner.dir_gen, inner.presence_gen)
    }
}

impl<S: DatagramSocket> Inner<S> {
    fn event(&mut self, e: LobbyEvent) {
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    fn connected(&self) -> Result<(), LobbyError> {
        match self.link {
            HubLinkState::Ended(_) => Err(LobbyError::Unavailable),
            _ => Ok(()),
        }
    }

    fn send(&mut self, rq: HubRequest) -> Result<(), LobbyError> {
        self.connected()?;
        if self.member.is_none() {
            if self.queued.len() >= MAX_QUEUED {
                return Err(LobbyError::Platform("hub_busy".into()));
            }
            self.queued.push(rq);
            return Ok(());
        }
        self.transmit(rq)
    }

    fn transmit(&mut self, rq: HubRequest) -> Result<(), LobbyError> {
        self.last_sent_ms = self.clock.now_ms();
        self.client
            .send(Channel::GAME_RPC, &HubMessage::Request(rq))
            .map_err(|e| LobbyError::Platform(format!("hub_send_failed: {e}")))
    }

    fn request(
        &mut self,
        pending: Pending,
        make: impl FnOnce(u64) -> HubRequest,
    ) -> Result<RequestId, LobbyError> {
        self.connected()?;
        if self.pending.len() >= MAX_PENDING {
            return Err(LobbyError::Platform("hub_busy".into()));
        }
        self.next_request += 1;
        let id = self.next_request;
        self.send(make(id))?;
        self.pending.insert(id, pending);
        Ok(RequestId(id))
    }

    fn member_of(&self, lobby: LobbyId) -> Result<&LobbyInfo, LobbyError> {
        self.rooms.get(&lobby).ok_or(LobbyError::NotInLobby)
    }

    fn owned(&self, lobby: LobbyId) -> Result<(), LobbyError> {
        let room = self.member_of(lobby)?;
        if Some(room.owner) != self.member {
            return Err(LobbyError::NotOwner);
        }
        Ok(())
    }

    fn pump(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("lobby_hub_client_pump").entered();
        self.client.receive(now_ms);
        while let Some(e) = self.client.poll_event() {
            match e {
                ClientEvent::Connected { .. } => {
                    // The proof binds the player key to this sealed link's handshake hash.
                    let proof = self
                        .client
                        .handshake_hash()
                        .and_then(|hash| self.identity.prove(&self.server_key, &hash).ok());
                    let Some(proof) = proof else {
                        self.client.disconnect();
                        self.end(HubError::IdentityMismatch.code().to_owned());
                        continue;
                    };
                    let hello = HubRequest::Hello {
                        handle: self.handle.clone(),
                        proof,
                    };
                    if let Err(e) = self.transmit(hello) {
                        self.last_error = Some(e.to_string());
                    }
                }
                ClientEvent::Ended(end) => {
                    let code = match end {
                        ClientEnd::ServerKeyMismatch => HubError::KeyMismatch.code().to_owned(),
                        other => other.as_str().to_owned(),
                    };
                    self.end(code);
                }
                ClientEvent::Message { message, .. } => match message {
                    HubMessage::Event(ev) => self.apply(ev),
                    HubMessage::Request(_) => self.last_error = Some("hub_unexpected_request".into()),
                },
                _ => {}
            }
        }
        if self.member.is_some() {
            let period = self.heartbeat_ms.unwrap_or(self.idle_ms / 3).max(1);
            if now_ms.saturating_sub(self.last_sent_ms) >= period {
                let _ = self.transmit(HubRequest::Heartbeat);
            }
        }
        self.client.flush(now_ms);
    }

    fn end(&mut self, code: String) {
        if matches!(self.link, HubLinkState::Ended(_)) {
            return;
        }
        self.link = HubLinkState::Ended(code);
        self.member = None;
        self.queued.clear();
        let rooms: Vec<LobbyId> = self.rooms.keys().copied().collect();
        self.rooms.clear();
        for lobby in rooms {
            self.event(LobbyEvent::Left {
                lobby,
                reason: LeaveReason::Lost,
            });
        }
        let pending = std::mem::take(&mut self.pending);
        for (id, p) in pending {
            let request = RequestId(id);
            let e = match p {
                Pending::Create => LobbyEvent::Created {
                    request,
                    result: Err(LobbyError::Unavailable),
                },
                Pending::Join => LobbyEvent::Entered {
                    request,
                    result: Err(LobbyError::Unavailable),
                },
                Pending::List(_) => LobbyEvent::List {
                    request,
                    result: Err(LobbyError::Unavailable),
                },
            };
            self.event(e);
        }
    }

    fn apply(&mut self, ev: HubEvent) {
        match ev {
            HubEvent::Welcome {
                member,
                handle,
                idle_ms,
                identity,
            } => {
                // A Welcome that binds the session to another player key is not ours: whoever sent
                // it (a hub that mixed sessions up, or one that impersonates a player) is refused.
                if identity != self.identity.public() {
                    self.last_error = Some(HubError::IdentityMismatch.code().to_owned());
                    self.client.disconnect();
                    self.end(HubError::IdentityMismatch.code().to_owned());
                    return;
                }
                self.member = Some(member);
                self.handle = handle;
                self.idle_ms = idle_ms;
                self.link = HubLinkState::Connected;
                for rq in std::mem::take(&mut self.queued) {
                    if let Err(e) = self.transmit(rq) {
                        self.last_error = Some(e.to_string());
                    }
                }
            }
            HubEvent::Reply { request, result } => {
                if let Err(code) = &result {
                    self.last_error = Some(code.clone());
                }
                let result = result.map_err(|code| HubError::from_code(&code).to_lobby());
                let request_id = RequestId(request);
                let entered = result.as_ref().ok().copied();
                match self.pending.remove(&request) {
                    Some(Pending::Create) => self.event(LobbyEvent::Created {
                        request: request_id,
                        result,
                    }),
                    Some(Pending::Join) => self.event(LobbyEvent::Entered {
                        request: request_id,
                        result,
                    }),
                    Some(Pending::List(_)) | None => return,
                }
                // A lobby whose session was already confirmed: the member learns where it is now.
                if let Some(lobby) = entered
                    && let Some(server) = self.rooms.get(&lobby).and_then(|r| r.server)
                {
                    self.event(LobbyEvent::ServerSet { lobby, server });
                }
            }
            HubEvent::ListDone { request } => {
                if let Some(Pending::List(query)) = self.pending.remove(&request) {
                    let infos: Vec<LobbyInfo> = self.directory.values().map(HubLobbyCard::to_info).collect();
                    let result = Ok(query.evaluate(&infos));
                    self.event(LobbyEvent::List {
                        request: RequestId(request),
                        result,
                    });
                }
            }
            HubEvent::Directory(page) => self.apply_page(page),
            HubEvent::Room(room) => {
                // A room for a lobby the client already holds is a resync after a gap (ADR-8633):
                // its data may have moved, so the game is told.
                let lobby = room.lobby;
                let resync = self.rooms.contains_key(&lobby);
                self.apply_room(room);
                if resync {
                    self.event(LobbyEvent::DataChanged { lobby, member: None });
                }
            }
            HubEvent::MemberJoined {
                lobby,
                member,
                handle,
            } => {
                if let Some(room) = self.rooms.get_mut(&lobby)
                    && !room.has_member(member)
                {
                    room.members.push(LobbyMember {
                        id: member,
                        name: handle,
                        data: BTreeMap::new(),
                    });
                    room.member_count = room.members.len() as u32;
                    self.event(LobbyEvent::MemberJoined { lobby, member });
                }
            }
            HubEvent::MemberLeft { lobby, member } => {
                if let Some(room) = self.rooms.get_mut(&lobby) {
                    room.members.retain(|m| m.id != member);
                    room.member_count = room.members.len() as u32;
                    self.event(LobbyEvent::MemberLeft { lobby, member });
                }
            }
            HubEvent::Data { lobby, key, value } => {
                if let Some(room) = self.rooms.get_mut(&lobby) {
                    match value {
                        Some(v) => room.data.insert(key, v),
                        None => room.data.remove(&key),
                    };
                    self.event(LobbyEvent::DataChanged { lobby, member: None });
                }
            }
            HubEvent::MemberData {
                lobby,
                member,
                key,
                value,
            } => {
                if let Some(m) = self
                    .rooms
                    .get_mut(&lobby)
                    .and_then(|r| r.members.iter_mut().find(|m| m.id == member))
                {
                    match value {
                        Some(v) => m.data.insert(key, v),
                        None => m.data.remove(&key),
                    };
                    self.event(LobbyEvent::DataChanged {
                        lobby,
                        member: Some(member),
                    });
                }
            }
            HubEvent::Joinable { lobby, joinable } => {
                if let Some(room) = self.rooms.get_mut(&lobby) {
                    room.joinable = joinable;
                    self.event(LobbyEvent::DataChanged { lobby, member: None });
                }
            }
            HubEvent::OwnerChanged { lobby, owner } => {
                if let Some(room) = self.rooms.get_mut(&lobby) {
                    room.owner = owner;
                    self.event(LobbyEvent::OwnerChanged { lobby, owner });
                }
            }
            HubEvent::ServerSet { lobby, server } => {
                if let Some(room) = self.rooms.get_mut(&lobby) {
                    room.server = server;
                    match server {
                        Some(s) => {
                            room.data.insert(keys::SERVER.to_owned(), s.to_key());
                            self.event(LobbyEvent::ServerSet { lobby, server: s });
                        }
                        None => {
                            room.data.remove(keys::SERVER);
                            self.event(LobbyEvent::DataChanged { lobby, member: None });
                        }
                    }
                }
            }
            HubEvent::Chat {
                scope,
                from,
                from_handle,
                text,
            } => match scope {
                ChatScope::Lobby(lobby) => {
                    if self.rooms.contains_key(&lobby) {
                        self.event(LobbyEvent::Chat { lobby, from, text });
                    }
                }
                ChatScope::Global => {
                    if self.global_chat.len() >= MAX_GLOBAL_CHAT {
                        self.global_chat.pop_front();
                    }
                    self.global_chat.push_back((from, from_handle, text));
                }
            },
            HubEvent::Invite(card) => {
                self.invites.retain(|c| c.lobby != card.lobby);
                if self.invites.len() >= MAX_INVITES {
                    self.invites.pop_front();
                }
                self.invites.push_back(card);
            }
            HubEvent::Left { lobby, reason } => {
                if self.rooms.remove(&lobby).is_some() {
                    self.event(LobbyEvent::Left { lobby, reason });
                }
            }
            HubEvent::ReachAsk {
                lobby,
                member,
                handle,
                identity,
                offer,
            } => {
                if !self.rooms.contains_key(&lobby) {
                    return;
                }
                if self.auto_approve {
                    let approve = HubRequest::Approve {
                        lobby,
                        member,
                        approve: true,
                    };
                    if let Err(e) = self.transmit(approve) {
                        self.last_error = Some(e.to_string());
                    }
                    return;
                }
                self.asks.retain(|a| !(a.lobby == lobby && a.member == member));
                if self.asks.len() >= MAX_REACH {
                    self.asks.pop_front();
                }
                self.asks.push_back(ReachAsk {
                    lobby,
                    member,
                    handle,
                    identity,
                    offer,
                });
            }
            HubEvent::Plan(plan) => {
                if self.rooms.contains_key(&plan.lobby) {
                    if self.plans.len() >= MAX_REACH {
                        self.plans.pop_front();
                    }
                    self.plans.push_back(plan);
                }
            }
            HubEvent::ReachDenied { .. } => {
                self.last_error = Some(HubError::ReachDenied.code().to_owned());
            }
            HubEvent::Reached {
                lobby, member, path, ..
            } => {
                if self.rooms.contains_key(&lobby) {
                    self.event(LobbyEvent::Reached { lobby, member, path });
                }
            }
            HubEvent::Refused { code } => {
                if self.member.is_none() {
                    // The hello was refused: no session will come on this link.
                    self.client.disconnect();
                    self.end(code.clone());
                }
                self.last_error = Some(code);
            }
        }
    }

    fn apply_page(&mut self, page: DirectoryPage) {
        if page.reset {
            self.directory.clear();
            self.players.clear();
        }
        for row in page.players {
            self.players.insert(row.member, row);
        }
        for id in page.removed_players {
            self.players.remove(&id);
        }
        for card in page.lobbies {
            self.directory.insert(card.id, card);
        }
        for id in page.removed_lobbies {
            self.directory.remove(&id);
        }
        if page.done {
            self.dir_gen = page.dir_gen;
            self.presence_gen = page.presence_gen;
        }
    }

    fn apply_room(&mut self, room: RoomState) {
        let members: Vec<LobbyMember> = room
            .members
            .into_iter()
            .map(|(id, name, data)| LobbyMember { id, name, data })
            .collect();
        let info = LobbyInfo {
            id: room.lobby,
            owner: room.owner,
            visibility: room.visibility,
            max_members: room.max_members,
            joinable: room.joinable,
            member_count: members.len() as u32,
            members,
            data: room.data,
            server: room.server,
        };
        // A used invite is spent.
        self.invites.retain(|c| c.lobby != room.lobby);
        self.rooms.insert(room.lobby, info);
    }
}

impl<S: DatagramSocket + Send> LobbyBackend for HubLobbyBackend<S> {
    fn name(&self) -> &'static str {
        "hub"
    }

    fn local_member(&self) -> Option<MemberId> {
        self.lock().member
    }

    fn create(&self, spec: &LobbySpec) -> Result<RequestId, LobbyError> {
        check_spec(spec)?;
        let mut inner = self.lock();
        if !inner.rooms.is_empty() {
            return Err(LobbyError::AlreadyInLobby);
        }
        inner.request(Pending::Create, |request| HubRequest::Create {
            request,
            visibility: spec.visibility,
            max_members: spec.max_members,
            joinable: spec.joinable,
            data: spec.data.clone(),
        })
    }

    fn join(&self, lobby: LobbyId) -> Result<RequestId, LobbyError> {
        let mut inner = self.lock();
        let token = inner.invites.iter().find(|c| c.lobby == lobby).map(|c| c.token);
        inner.request(Pending::Join, |request| HubRequest::Join {
            request,
            lobby,
            token,
        })
    }

    fn leave(&self, lobby: LobbyId) {
        let mut inner = self.lock();
        if inner.rooms.contains_key(&lobby) {
            let _ = inner.send(HubRequest::Leave { lobby });
        }
    }

    fn request_list(&self, query: &LobbyQuery) -> Result<RequestId, LobbyError> {
        query.validate()?;
        let mut inner = self.lock();
        let (dir_gen, presence_gen) = (inner.dir_gen, inner.presence_gen);
        inner.request(Pending::List(query.clone()), |request| HubRequest::List {
            request,
            dir_gen,
            presence_gen,
        })
    }

    fn lobby(&self, lobby: LobbyId) -> Option<LobbyInfo> {
        self.lock().rooms.get(&lobby).cloned()
    }

    fn still_in(&self, lobby: LobbyId) -> bool {
        self.lock().rooms.contains_key(&lobby)
    }

    fn set_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
        check_key(key)?;
        if let Some(v) = value {
            check_value(v)?;
        }
        let mut inner = self.lock();
        inner.owned(lobby)?;
        inner.send(HubRequest::SetData {
            lobby,
            key: key.to_owned(),
            value: value.map(str::to_owned),
        })
    }

    fn set_member_data(&self, lobby: LobbyId, key: &str, value: Option<&str>) -> Result<(), LobbyError> {
        check_key(key)?;
        if let Some(v) = value {
            check_value(v)?;
        }
        let mut inner = self.lock();
        inner.member_of(lobby)?;
        inner.send(HubRequest::SetMemberData {
            lobby,
            key: key.to_owned(),
            value: value.map(str::to_owned),
        })
    }

    fn set_joinable(&self, lobby: LobbyId, joinable: bool) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.owned(lobby)?;
        inner.send(HubRequest::SetJoinable { lobby, joinable })
    }

    fn set_server(&self, lobby: LobbyId, server: LobbyServer) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.owned(lobby)?;
        inner.send(HubRequest::SetServer {
            lobby,
            server: Some(server),
        })
    }

    fn send_chat(&self, lobby: LobbyId, text: &str) -> Result<(), LobbyError> {
        check_chat(text)?;
        let mut inner = self.lock();
        inner.member_of(lobby)?;
        inner.send(HubRequest::Chat {
            scope: ChatScope::Lobby(lobby),
            text: text.to_owned(),
        })
    }

    fn poll(&self) -> Vec<LobbyEvent> {
        let mut inner = self.lock();
        let now = inner.clock.now_ms();
        inner.pump(now);
        inner.events.drain(..).collect()
    }
}

#[cfg(test)]
mod clock_tests {
    use super::HubClock;

    /// ADR-8891: the default clock of a target without std's monotonic clock (wasm32, where
    /// `Instant::now` panics) is a manual clock the host advances, never `Instant`.
    #[test]
    fn a_target_without_a_monotonic_clock_defaults_to_a_manual_clock() {
        let clock = HubClock::for_target(false);
        assert!(matches!(clock, HubClock::Manual(_)), "{clock:?}");
        assert_eq!(clock.now_ms(), 0);
        clock.set_ms(1_250);
        assert_eq!(clock.now_ms(), 1_250);
        assert!(matches!(HubClock::for_target(true), HubClock::Monotonic(_)));
    }

    /// The host's default is still monotonic (every native target has the clock).
    #[test]
    fn the_native_default_is_monotonic() {
        assert!(matches!(HubClock::default(), HubClock::Monotonic(_)));
    }
}
