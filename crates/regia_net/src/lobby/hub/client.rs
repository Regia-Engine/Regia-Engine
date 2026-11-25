// SPDX-License-Identifier: MPL-2.0
//! [`HubLobbyBackend`]: the hub client, a [`LobbyBackend`] over a [`NetClient`] pinned to the
//! hub's [`ServerKey`].

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use super::join::{JoinPlan, ReachKind, ReachOffer};
use super::wire::{
    BindingState, ChatScope, DirectoryPage, HubEvent, HubMessage, HubRequest, HubWire, RoomState, ServerCard,
    ServerNotice,
};
use super::{HubError, HubLobbyCard, HubPresence, InviteCard, PresenceRow};
use crate::client::{ClientConfig, ClientEnd, ClientEvent, NetClient};
use crate::connection::Channel;
use crate::crypto::{CryptoMode, PlayerIdentity, PlayerKey, ServerIdentity, ServerKey};
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
/// Version-2 notices kept until [`HubLobbyBackend::take_notices`].
const MAX_NOTICES: usize = 64;

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

/// What this session publishes when it registers as a dedicated server.
///
/// `grace` of `None` is [`super::GRACE_MS_DEFAULT`] (600_000 ms). `Some(0)` stays 0: no deadline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRegister {
    /// Seats the server will take.
    pub capacity: u16,
    /// Seats in use.
    pub load: u16,
    /// The server's game protocol number.
    pub protocol: u32,
    /// The server's game version text.
    pub game_version: String,
    /// When set, the server offers only the relay. The planner still ignores it.
    pub relay_only: bool,
    /// Grace after the owner leaves. `None` is the default; `Some(0)` is no deadline.
    pub grace: Option<u32>,
    /// Player keys this server refuses. The hub enforces this list on approve.
    pub bans: Vec<PlayerKey>,
    /// Owner binding proofs. Empty is an ownerless server.
    pub owner_proofs: Vec<Vec<u8>>,
}

impl Default for ServerRegister {
    fn default() -> Self {
        Self {
            capacity: 1,
            load: 0,
            protocol: 1,
            game_version: String::new(),
            relay_only: false,
            grace: None,
            bans: Vec::new(),
            owner_proofs: Vec::new(),
        }
    }
}

/// A binding proof this player built with [`HubLobbyBackend::bind_server`].
///
/// `lobby.hub_bindings` is this list, not a hub-wide dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalBinding {
    /// The dedicated server.
    pub server_key: ServerKey,
    /// The owner's revocation id.
    pub serial: u64,
    /// Expiry on the hub clock, milliseconds. `0` means no expiry.
    pub expiry_ms: u64,
    /// The Noise proof bytes to hand to the server.
    pub proof: Vec<u8>,
}

/// A version-2 notice. None of these carry an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HubNotice {
    /// A bound server came online.
    ServerOnline(ServerNotice),
    /// A bound server went offline.
    ServerOffline(ServerNotice),
    /// A delegated lobby named this server session.
    LobbyAssigned {
        /// The lobby.
        lobby: LobbyId,
        /// The player key that owns it.
        owner: PlayerKey,
    },
    /// The owner's session is gone and the lobby is in grace.
    GraceStarted {
        /// The lobby.
        lobby: LobbyId,
        /// Absolute deadline, or 0 for no deadline.
        deadline_ms: u64,
    },
    /// Grace ended and the lobby closed.
    GraceEnded {
        /// The lobby.
        lobby: LobbyId,
    },
    /// One of this session's bindings changed.
    BindingChanged {
        /// The dedicated server.
        server_key: ServerKey,
        /// The revocation id.
        serial: u64,
        /// Live, expired, or revoked.
        state: BindingState,
    },
    /// The session address was confirmed or withdrawn. The address itself is not here.
    SessionConfirmed {
        /// The lobby.
        lobby: LobbyId,
        /// Whether a session address is confirmed.
        confirmed: bool,
    },
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
    /// Codec generation the hub speaks. `1` until [`HubLobbyBackend::note_hub_game_version`]
    /// sees game version `"2"`.
    hub_wire: u32,
    /// Whether this session has sent `AnnounceWire` for generation 2.
    announced: bool,
    /// Lobbies this session hosts as the dedicated server (`LobbyAssigned`).
    assigned: BTreeSet<LobbyId>,
    /// Dedicated servers from the directory. Empty on a wire-1 mirror.
    servers: Vec<ServerCard>,
    /// Binding proofs this player built.
    bindings: Vec<LocalBinding>,
    /// Version-2 notices not yet taken.
    notices: VecDeque<HubNotice>,
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
                hub_wire: super::wire::HUB_WIRE_VERSION_V1,
                announced: false,
                assigned: BTreeSet::new(),
                servers: Vec::new(),
                bindings: Vec::new(),
                notices: VecDeque::new(),
            }),
        })
    }

    /// Records the hub's game version from a version probe.
    ///
    /// `"2"` is a v0.7.2 hub: version-2 calls may be sent, and the first one (or this call, once
    /// the session is up) is preceded by `AnnounceWire`. Any other text, including empty and
    /// `"2.0"`, keeps wire 1. A version-2 call then fails with `hub_needs_wire_v2` and nothing
    /// is sent (ADR-10378).
    pub fn note_hub_game_version(&self, game_version: &str) {
        self.lock().note_hub_game_version(game_version);
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

    /// As the owner of `lobby`, or as the dedicated server the hub assigned to it, offers how the
    /// game session's socket is reached (join brokering, [`super::join`]): its candidates (a port
    /// mapping as a `mapped` candidate), its NAT behavior when known, the relay, its join code and
    /// the join ticket.
    ///
    /// On a delegated lobby the server session is the caller. The owning player's offer is refused
    /// by the hub with `hub_not_session`.
    pub fn offer_reach(&self, lobby: LobbyId, offer: ReachOffer) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.may_host(lobby)?;
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

    /// As the owner, or as the dedicated server assigned to `lobby`, approves or denies a member's
    /// ask.
    ///
    /// Password, `bans.ron` and `JoinTicketCheck` stay the game server's job before it sends this.
    /// The hub forces a denial when the published ban list names the member or `load >= capacity`.
    pub fn approve(&self, lobby: LobbyId, member: MemberId, approve: bool) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        inner.may_host(lobby)?;
        inner.asks.retain(|a| !(a.lobby == lobby && a.member == member));
        inner.send(HubRequest::Approve {
            lobby,
            member,
            approve,
        })
    }

    /// Builds an owner binding proof for `server` and keeps it on this session (`lobby.hub_bindings`).
    ///
    /// The proof is not sent. Hand the bytes to the dedicated server, which puts them on
    /// `RegisterServer`.
    ///
    /// # Errors
    ///
    /// [`LobbyError::Platform`] when the proof cannot be built.
    pub fn bind_server(
        &self,
        server: &ServerKey,
        expiry_ms: u64,
        serial: u64,
    ) -> Result<Vec<u8>, LobbyError> {
        let mut inner = self.lock();
        let claims = super::BindingClaims {
            version: 1,
            server_key: *server,
            expiry_ms,
            serial,
        };
        let payload = super::encode_binding_payload(&claims)
            .map_err(|_| LobbyError::Platform("hub_binding_invalid".into()))?;
        let proof = crate::crypto::binding::prove_binding(&inner.identity, &inner.server_key, &payload)
            .map_err(|_| LobbyError::Platform("hub_binding_invalid".into()))?;
        inner
            .bindings
            .retain(|b| !(b.server_key == *server && b.serial == serial));
        inner.bindings.push(LocalBinding {
            server_key: *server,
            serial,
            expiry_ms,
            proof: proof.clone(),
        });
        Ok(proof)
    }

    /// Binding proofs this session has built.
    pub fn bindings(&self) -> Vec<LocalBinding> {
        self.lock().bindings.clone()
    }

    /// Registers this session as the dedicated server `server`.
    ///
    /// The presence proof binds `server` to this link's handshake hash. `register.grace` of `None`
    /// is 600_000 ms; `Some(0)` stays 0.
    ///
    /// # Errors
    ///
    /// [`LobbyError::Unavailable`] before the link is sealed, [`LobbyError::Platform`] when the
    /// presence proof cannot be built, or [`HubError::NeedsWireV2`] against a version-1 hub.
    pub fn register_server(
        &self,
        server: &ServerIdentity,
        register: &ServerRegister,
    ) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        let hash = inner.client.handshake_hash().ok_or(LobbyError::Unavailable)?;
        let server_proof = crate::crypto::binding::prove_server_presence(server, &inner.server_key, &hash)
            .map_err(|_| LobbyError::Platform("hub_binding_invalid".into()))?;
        let grace_ms = register.grace.unwrap_or(super::GRACE_MS_DEFAULT);
        inner.send(HubRequest::RegisterServer {
            presence: super::ServerPresence {
                server_proof,
                proofs: register.owner_proofs.clone(),
                capacity: register.capacity,
                load: register.load,
                protocol: register.protocol,
                game_version: register.game_version.clone(),
                relay_only: register.relay_only,
                grace_ms,
                bans: register.bans.clone(),
            },
        })
    }

    /// Creates a delegated lobby on `server_key`. The answer is [`LobbyEvent::Created`].
    ///
    /// # Errors
    ///
    /// [`LobbyError::AlreadyInLobby`] when this session is already in a lobby, a spec error, or
    /// [`HubError::NeedsWireV2`] against a version-1 hub.
    pub fn create_delegated(&self, spec: &LobbySpec, server_key: ServerKey) -> Result<RequestId, LobbyError> {
        check_spec(spec)?;
        let mut inner = self.lock();
        if !inner.rooms.is_empty() {
            return Err(LobbyError::AlreadyInLobby);
        }
        inner.request(Pending::Create, |request| HubRequest::CreateDelegated {
            request,
            visibility: spec.visibility,
            max_members: spec.max_members,
            joinable: spec.joinable,
            data: spec.data.clone(),
            server_key,
        })
    }

    /// Revokes the binding `(this player, server_key, serial)`.
    ///
    /// # Errors
    ///
    /// [`HubError::NeedsWireV2`] against a version-1 hub.
    pub fn revoke_binding(&self, server_key: ServerKey, serial: u64) -> Result<(), LobbyError> {
        self.lock().send(HubRequest::RevokeBinding { server_key, serial })
    }

    /// Closes a delegated or ownerless lobby. A listen lobby stays on [`LobbyBackend::leave`].
    ///
    /// # Errors
    ///
    /// [`LobbyError::NotInLobby`] when this session neither holds nor hosts `lobby`, or
    /// [`HubError::NeedsWireV2`] against a version-1 hub.
    pub fn close_lobby(&self, lobby: LobbyId) -> Result<(), LobbyError> {
        let mut inner = self.lock();
        if !inner.rooms.contains_key(&lobby) && !inner.assigned.contains(&lobby) {
            return Err(LobbyError::NotInLobby);
        }
        inner.send(HubRequest::CloseLobby { lobby })
    }

    /// Drains version-2 notices (server online, grace, binding). None carry an address.
    pub fn take_notices(&self) -> Vec<HubNotice> {
        self.lock().notices.drain(..).collect()
    }

    /// Dedicated servers the directory listed. `lobby.hub_servers` on the client mirror.
    pub fn servers(&self) -> Vec<ServerCard> {
        self.lock().servers.clone()
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

    fn note_hub_game_version(&mut self, game_version: &str) {
        self.hub_wire = super::wire::hub_wire_from_game_version(game_version);
        if self.hub_wire < super::wire::HUB_WIRE_VERSION
            || self.announced
            || !matches!(self.link, HubLinkState::Connected)
        {
            return;
        }
        if self
            .transmit(HubRequest::AnnounceWire {
                version: super::wire::HUB_WIRE_VERSION as u8,
            })
            .is_ok()
        {
            self.announced = true;
        }
    }

    fn transmit(&mut self, rq: HubRequest) -> Result<(), LobbyError> {
        let v2 = super::wire::request_is_v2_only(&rq);
        let announce = matches!(rq, HubRequest::AnnounceWire { .. });
        if (v2 || announce) && self.hub_wire < super::wire::HUB_WIRE_VERSION {
            // A version-1 hub: say so and send nothing (ADR-10378).
            self.last_error = Some(HubError::NeedsWireV2.code().to_owned());
            return Err(HubError::NeedsWireV2.to_lobby());
        }
        if v2 && !self.announced {
            self.transmit(HubRequest::AnnounceWire {
                version: super::wire::HUB_WIRE_VERSION as u8,
            })?;
            self.announced = true;
        }
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

    /// The listen owner, or the dedicated server this hub assigned to `lobby`.
    fn may_host(&self, lobby: LobbyId) -> Result<(), LobbyError> {
        if self.assigned.contains(&lobby) {
            return Ok(());
        }
        self.owned(lobby)
    }

    fn push_notice(&mut self, notice: HubNotice) {
        if self.notices.len() >= MAX_NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(notice);
    }

    /// A lobby the hub assigned to this server, before any room snapshot. The owner id stays
    /// empty so a player-owner check still fails; the ask and the plan are not dropped.
    fn note_assigned(&mut self, lobby: LobbyId) {
        self.assigned.insert(lobby);
        if self.rooms.contains_key(&lobby) {
            return;
        }
        self.rooms.insert(
            lobby,
            LobbyInfo {
                id: lobby,
                owner: MemberId(0),
                visibility: crate::lobby::LobbyVisibility::Public,
                max_members: 0,
                joinable: true,
                members: Vec::new(),
                member_count: 0,
                data: BTreeMap::new(),
                server: None,
            },
        );
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
                self.assigned.remove(&lobby);
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
            HubEvent::ServerOnline(notice) => self.push_notice(HubNotice::ServerOnline(notice)),
            HubEvent::ServerOffline(notice) => self.push_notice(HubNotice::ServerOffline(notice)),
            HubEvent::LobbyAssigned { lobby, owner } => {
                self.note_assigned(lobby);
                self.push_notice(HubNotice::LobbyAssigned { lobby, owner });
            }
            HubEvent::GraceStarted { lobby, deadline_ms } => {
                self.push_notice(HubNotice::GraceStarted { lobby, deadline_ms });
            }
            HubEvent::GraceEnded { lobby } => {
                self.assigned.remove(&lobby);
                if self.rooms.remove(&lobby).is_some() {
                    self.event(LobbyEvent::Left {
                        lobby,
                        reason: LeaveReason::Lost,
                    });
                }
                self.push_notice(HubNotice::GraceEnded { lobby });
            }
            HubEvent::BindingChanged {
                server_key,
                serial,
                state,
            } => self.push_notice(HubNotice::BindingChanged {
                server_key,
                serial,
                state,
            }),
            HubEvent::SessionConfirmed { lobby, confirmed } => {
                self.push_notice(HubNotice::SessionConfirmed { lobby, confirmed });
            }
        }
    }

    fn apply_page(&mut self, page: DirectoryPage) {
        if page.reset {
            self.directory.clear();
            self.players.clear();
            self.servers.clear();
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
            self.servers = page.servers;
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

#[cfg(test)]
mod wire_gate_tests {
    use std::net::SocketAddr;

    use super::{HubClientConfig, HubClock, HubLobbyBackend, HubRequest};
    use crate::crypto::ServerIdentity;
    use crate::lobby::hub::hub_wire_from_game_version;
    use crate::lobby::{LobbyError, LobbyId};
    use crate::socket::LoopbackNet;

    fn backend() -> HubLobbyBackend<crate::socket::LoopbackSocket> {
        let net = LoopbackNet::new();
        let socket = net.bind(SocketAddr::from(([192, 0, 2, 40], 1))).expect("bind");
        let key = ServerIdentity::from_private([7; 32]).expect("curve").public();
        HubLobbyBackend::connect(
            socket,
            SocketAddr::from(([192, 0, 2, 9], 7810)),
            HubClientConfig {
                handle: "ann".into(),
                server_key: Some(key),
                clock: HubClock::manual(0),
                ..HubClientConfig::default()
            },
        )
        .expect("client")
    }

    /// A v2 client that has seen a v1 hub (empty game version) gets `hub_needs_wire_v2` and
    /// does not attempt a send. A send would fail as `hub_send_failed` because the link is down.
    #[test]
    fn a_v1_hub_refuses_a_v2_call_before_any_byte_is_sent() {
        let backend = backend();
        backend.note_hub_game_version("");
        let mut inner = backend.lock();
        assert_eq!(inner.hub_wire, hub_wire_from_game_version(""));
        let err = inner
            .transmit(HubRequest::CloseLobby { lobby: LobbyId(1) })
            .expect_err("refused");
        assert_eq!(err, LobbyError::Platform("hub_needs_wire_v2".into()));
        assert_eq!(inner.last_error.as_deref(), Some("hub_needs_wire_v2"));
    }

    /// Game version `"2"` opens the gate. The link is not up, so the attempt is a send failure,
    /// which is how this test shows the bytes were not refused locally.
    #[test]
    fn a_v2_hub_lets_the_call_reach_the_socket() {
        let backend = backend();
        backend.note_hub_game_version("2");
        let mut inner = backend.lock();
        assert_eq!(inner.hub_wire, 2);
        let err = inner
            .transmit(HubRequest::CloseLobby { lobby: LobbyId(1) })
            .expect_err("not connected");
        let LobbyError::Platform(code) = err else {
            panic!("a platform error");
        };
        assert!(code.starts_with("hub_send_failed"), "{code}");
    }

    #[test]
    fn two_dot_zero_stays_on_wire_1() {
        let backend = backend();
        backend.note_hub_game_version("2.0");
        assert_eq!(backend.lock().hub_wire, 1);
    }

    /// `relay_only` is a version-2 tail on `Offer` and `AskReach`. A v0.7.0 hub rejects that
    /// trailing byte (`message_malformed`) and, after enough of them, drops the link. The client
    /// refuses locally with `hub_needs_wire_v2` and sends nothing. A version-1 offer still attempts
    /// the send.
    #[test]
    fn a_v1_hub_refuses_a_relay_only_offer_before_any_byte_is_sent() {
        let backend = backend();
        backend.note_hub_game_version("");
        let mut inner = backend.lock();
        let plain = inner
            .transmit(HubRequest::Offer {
                lobby: LobbyId(1),
                offer: crate::lobby::hub::ReachOffer::default(),
            })
            .expect_err("link is down");
        let LobbyError::Platform(code) = plain else {
            panic!("a platform error");
        };
        assert!(
            code.starts_with("hub_send_failed"),
            "a version-1 offer is still sent, got {code}"
        );

        let flagged = inner
            .transmit(HubRequest::AskReach {
                lobby: LobbyId(1),
                offer: crate::lobby::hub::ReachOffer {
                    relay_only: true,
                    ..crate::lobby::hub::ReachOffer::default()
                },
            })
            .expect_err("refused");
        assert_eq!(flagged, LobbyError::Platform("hub_needs_wire_v2".into()));
        assert_eq!(inner.last_error.as_deref(), Some("hub_needs_wire_v2"));
    }
}
