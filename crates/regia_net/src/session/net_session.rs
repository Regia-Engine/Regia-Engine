// SPDX-License-Identifier: MPL-2.0
//! `NetSession`: the ECS resource a game drives (spec 77 § 11), plus `SessionRules` (§ 9) and the
//! plugin that runs the session in `PreUpdate` after receive (§ 13).
//!
//! A `NetSession` is idle, a server (listen or dedicated) or a client. A server owns a
//! [`SessionServer`] core, an optional datagram transport (the `regia_net` endpoints over UDP or the
//! in-memory network) and its simulated clients; a client owns a [`SessionClient`] core and its
//! transport. Remote players get a `Player`-side entity carrying [`NetPlayer`] from `Welcome` until
//! `Gone`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use regia_app::{App, Lifecycle, Plugin, PreUpdate};
use regia_ecs::prelude::*;

use crate::client::{ClientConfig, ClientEnd, ClientEvent, NetClient};
use crate::connection::{Channel, LinkStats};
use crate::crypto::{ResumeCredential, ServerIdentity, ServerKey, secret_would_be_plain};
use crate::handshake::DisconnectReason as TransportReason;
use crate::handshake::RESUME_TICKET_BYTES;
use crate::known_hosts::{FirstUsePin, KnownHostsError};
use crate::model::{NetPlayer, NetPlayerId, PeerId};
use crate::payload::NetGame;
use crate::plugin::NetSet;
use crate::reconnect::{ReconnectPolicy, ReconnectStep, Reconnector};
use crate::server::{NetServer, ServerConfig, ServerEvent};
use crate::server_host::join::{JoinError, RelaySpec};
use crate::server_host::relay_path::{JoinPath, JoinRoute, JoinSocket, fresh_allocation_key};
use crate::session::bans::{BanError, BanId, BanKind};
use crate::session::conditioner::SessionConditioner;
use crate::session::hub::{LOOPBACK_LINK_BASE, LoopbackClients, MAX_SIMULATED_CLIENTS, SimInput};
use crate::session::lifecycle::{
    ClientSessionEvent, ClientSessionState, HostOptions, JoinDecision, LinkId, PeerState, ServerSessionEvent,
    SessionClient, SessionServer, SessionSettings,
};
use crate::session::manifest::{ContentManifest, ManifestMod};
use crate::session::meter::{BandwidthView, ConnectionStats};
use crate::session::players::{
    NetHandOffs, NetPlayerHooks, NetPlayerStats, NetStatsOverlay, OwnedHandOff, PlayerHook, PlayerLinkStats,
    apply_hand_off, default_hand_off, owned_by,
};
use crate::session::proto::{DisconnectReason, Hello};
use crate::session::streams::{StreamEvent, StreamId, StreamRefusal};
use crate::session::travel::{
    JoinRecord, JoinSnapshotCounts, NetPersistAcrossTravel, TravelCounts, despawn_for_travel,
    drop_copies_for_travel, join_snapshots_for, travel_keep_for,
};
use crate::socket::{DatagramSocket, UdpTransport};

/// Most undrained session events kept per side; older ones are dropped (a host that never reads
/// its events must not grow without bound).
pub const MAX_PENDING_SESSION_EVENTS: usize = 4096;

fn truncate_text(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Data events (game messages, stream progress) stop here, so a flood of them cannot evict a join
/// or a leave: the last quarter of the queue is for lifecycle events.
const MAX_PENDING_DATA_EVENTS: usize = MAX_PENDING_SESSION_EVENTS / 4 * 3;

/// An event that carries traffic rather than a state change of the session. A completed stream
/// is traffic too: a zero-byte `StreamBegin` completes at once and holds no stream slot, so a
/// peer could send thousands per frame and, counted as lifecycle, evict other peers' joins and
/// leaves (ADR-5870).
trait SessionEventKind {
    fn is_data(&self) -> bool;
}

impl SessionEventKind for ServerSessionEvent {
    fn is_data(&self) -> bool {
        matches!(
            self,
            Self::Game { .. }
                | Self::Stream {
                    event: StreamEvent::Progress { .. } | StreamEvent::Complete { .. },
                    ..
                }
        )
    }
}

impl SessionEventKind for ClientSessionEvent {
    fn is_data(&self) -> bool {
        matches!(
            self,
            Self::Game(_)
                | Self::Stream(StreamEvent::Progress { .. } | StreamEvent::Complete { .. })
                | Self::AdminOutput(_)
        )
    }
}

fn push_bounded<T: SessionEventKind>(queue: &mut Vec<T>, e: T) {
    if e.is_data() && queue.len() >= MAX_PENDING_DATA_EVENTS {
        return;
    }
    if queue.len() >= MAX_PENDING_SESSION_EVENTS {
        queue.remove(0);
    }
    queue.push(e);
}

/// The datagram game carrying session messages over the `regia_net` endpoints.
#[derive(Debug, Clone, Copy)]
pub struct SessionWire;

impl NetGame for SessionWire {
    type Message = Vec<u8>;
    type Input = ();
    type JoinData = ();
    const GAME_ID: &'static str = "regia.session";
    const PROTOCOL_VERSION: u32 = 1;
}

/// Opens a fresh client socket of the transport kind a session connected with (UDP, Steam
/// sockets, QUIC, ...): [`NetSession::resume`] calls it to reconnect after a suspend over the
/// same kind of link (spec 120 § 10.3). Set by [`NetSession::connect_via`] or
/// [`NetSession::set_socket_opener`]; [`NetSession::connect`] installs a UDP one.
pub type SocketOpener = Arc<dyn Fn() -> std::io::Result<Box<dyn DatagramSocket + Send>> + Send + Sync>;

/// A UDP [`SocketOpener`] for a server at `server` (an unspecified bind of its address family).
pub fn udp_socket_opener(server: SocketAddr) -> SocketOpener {
    Arc::new(move || {
        let bind: SocketAddr = if server.is_ipv4() {
            SocketAddr::from(([0, 0, 0, 0], 0))
        } else {
            SocketAddr::from(([0u16; 8], 0))
        };
        UdpTransport::bind(bind).map(|s| Box::new(s) as Box<dyn DatagramSocket + Send>)
    })
}

/// Where a client connects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectTarget {
    /// A server address.
    Address(SocketAddr),
    /// A join code, looked up on a relay (spec 120 § 10.7): `CODE` with the relay of
    /// [`NetSession::set_relay`], or `CODE@HOST:PORT`. The client tries the host's address and
    /// falls back to a relay allocation of its own.
    JoinCode(String),
}

/// Client-side options of [`NetSession::connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectOptions {
    /// One name per local player.
    pub player_names: Vec<String>,
    /// Password.
    pub password: Option<String>,
    /// Opaque authenticator data.
    pub auth: Option<Vec<u8>>,
    /// The client's downstream limit, bytes per second.
    pub client_rate_bytes: u32,
    /// Resume the last session's grace slot with its reconnect token.
    pub resume: bool,
    /// The server's public key. Without it the client trusts the key on first use through the
    /// session's known-hosts store ([`NetSession::set_known_hosts`], ADR-3283), or, with no store,
    /// takes any key, which stops a listener but not an attacker in the middle (ADR-1901). An
    /// explicit key wins: the store is neither read nor written.
    pub server_key: Option<ServerKey>,
    /// Skip the known-hosts store for this connection: neither check nor record the server's key
    /// (LAN development against servers that make a new key every run). Off by default; with it
    /// and no `server_key`, an attacker in the middle can read the `Hello` and its password.
    pub allow_unpinned: bool,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            player_names: vec!["player".into()],
            password: None,
            auth: None,
            client_rate_bytes: 49_152,
            resume: false,
            server_key: None,
            allow_unpinned: false,
        }
    }
}

/// Why a session call failed (spec 77 § 11).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// A session is already running.
    #[error("a session is already running")]
    AlreadyInSession,
    /// Server-only call on a client or idle session.
    #[error("not a server")]
    NotServer,
    /// Client-only call on a server or idle session.
    #[error("not a client")]
    NotClient,
    /// No such player.
    #[error("unknown player")]
    PlayerUnknown,
    /// No such level.
    #[error("level not found")]
    LevelNotFound,
    /// A stream above 64 MiB.
    #[error("stream larger than 64 MiB")]
    StreamTooLarge,
    /// Already 4 streams in flight.
    #[error("too many streams in flight")]
    TooManyStreams,
    /// More than 256 simulated clients.
    #[error("at most 256 simulated clients")]
    SimClientsLimit,
    /// A ban entry that does not parse.
    #[error("bad ban entry: {0}")]
    BadBan(String),
    /// The ban entry was added (it applies for this run) but `bans.ron` could not be saved
    /// (a locked file, a read-only store, a full disk): the caller should still remove the
    /// peer, and the operator keeps the entry only until the next restart.
    #[error("ban list save failed: {0}")]
    BanStoreSaveFailed(String),
    /// The transport failed.
    #[error("transport: {0}")]
    Transport(String),
    /// A password or admin token would cross a non-loopback link in the clear.
    #[error("a password or admin token needs an encrypted link: `encryption` is off")]
    PasswordNeedsEncryption,
    /// The known-hosts store could not be used (damaged, newer, bad host id): the connection is
    /// refused rather than made unpinned. The code is the store's (`net_known_hosts_unreadable`,
    /// `net_known_hosts_newer`, `net_bad_host_id`).
    #[error("known hosts: {0}")]
    KnownHosts(KnownHostsError),
}

impl SessionError {
    /// The lowercase agent error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyInSession => "already_in_session",
            Self::NotServer => "net_not_server",
            Self::NotClient => "net_not_client",
            Self::PlayerUnknown => "player_unknown",
            Self::LevelNotFound => "level_not_found",
            Self::StreamTooLarge => "stream_too_large",
            Self::TooManyStreams => "too_many_streams",
            Self::SimClientsLimit => "sim_clients_limit",
            Self::BadBan(_) => "ban_invalid",
            Self::BanStoreSaveFailed(_) => "ban_store_save_failed",
            Self::Transport(_) => "transport_error",
            Self::PasswordNeedsEncryption => "password_needs_encryption",
            Self::KnownHosts(e) => e.code(),
        }
    }
}

/// Network-only game hooks (spec 77 § 9). Every method has a default; install with
/// [`SessionAppExt::set_session_rules`].
pub trait SessionRules: Send + Sync + 'static {
    /// Runs after the engine's handshake checks; refuse with a text sent as `kicked`.
    fn on_player_joining(&mut self, hello: &Hello) -> JoinDecision {
        let _ = hello;
        JoinDecision::Accept
    }
    /// A remote player dropped unexpectedly and is in the grace period. The engine has already
    /// added [`NetDropped`] to its entity (default: frozen, no input, still relevant).
    fn on_player_dropped(&mut self, world: &mut World, player: Entity) {
        let _ = (world, player);
    }
    /// A player resumed its grace slot.
    fn on_player_resumed(&mut self, world: &mut World, player: Entity) {
        let _ = (world, player);
    }
    /// A player is leaving (`Gone`), before its entity is despawned.
    fn on_player_left_reason(&mut self, world: &mut World, player: Entity, reason: &DisconnectReason) {
        let _ = (world, player, reason);
    }
    /// The server finished a travel.
    fn on_travel_finished(&mut self, world: &mut World, level: &str) {
        let _ = (world, level);
    }
    /// A peer reached `Gone`: what happens to `entity`, whose own `NetOwner` named it (spec 77
    /// § 3.2, ADR-2413). Runs after the leave hooks of the peer's players. `live` answers whether a
    /// peer is still in the session. The default is [`default_hand_off`]: the entity's
    /// `NetSuccessor` when that peer is live, else the server.
    fn owned_hand_off(
        &mut self,
        world: &World,
        entity: Entity,
        peer: PeerId,
        live: &dyn Fn(PeerId) -> bool,
    ) -> OwnedHandOff {
        let _ = peer;
        default_hand_off(world, entity, live)
    }
    /// May the dedicated host lower the tick rate while empty (spec 76 § 5)?
    fn allow_idle_slowdown(&self) -> bool {
        false
    }
    /// Polled once per tick by the dedicated host: when true it rotates the level (spec 76 § 5).
    fn match_over(&self, world: &World) -> bool {
        let _ = world;
        false
    }
}

/// The defaults of every hook.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultSessionRules;

impl SessionRules for DefaultSessionRules {}

/// On a remote player's entity during the reconnect grace: frozen, takes no input.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct NetDropped;

/// Session events of the last update, for game systems to read (cleared every update).
#[derive(Resource, Debug, Default, Clone)]
pub struct NetSessionEvents {
    /// Server events.
    pub server: Vec<ServerSessionEvent>,
    /// Client events.
    pub client: Vec<ClientSessionEvent>,
}

trait ServerLink: Send {
    fn receive(&mut self, now_ms: u64, core: &mut SessionServer);
    fn flush(
        &mut self,
        now_ms: u64,
        core: &mut SessionServer,
        out: Vec<crate::session::lifecycle::Outgoing>,
        closes: Vec<LinkId>,
    );
    fn local_addr(&self) -> Option<SocketAddr>;
    fn server_key(&self) -> Option<ServerKey>;
    /// The transport's estimates for one link (`None` for a link it does not know).
    fn link_stats(&self, link: LinkId) -> Option<LinkStats>;
    /// Marks a relay's IP (see `NetServer::add_relay_ip`).
    fn add_relay_ip(&mut self, ip: std::net::IpAddr);
    /// The transport's connection quality for one link (ADR-8182).
    fn link_quality(&self, _link: LinkId) -> Option<crate::quality::PeerQuality> {
        None
    }
    /// The transport endpoint's counters.
    fn totals(&self) -> crate::status::EndpointTotals;
}

trait ClientLink: Send {
    fn receive(&mut self, now_ms: u64, core: &mut SessionClient);
    fn flush(&mut self, now_ms: u64, core: &mut SessionClient);
    fn server_key(&self) -> Option<ServerKey>;
    fn is_sealed(&self) -> bool;
    /// The transport's estimates for the link to the server.
    fn link_stats(&self) -> Option<LinkStats>;
    /// The join-by-code path, for a client that joined by code.
    fn join_route(&self) -> Option<JoinRoute> {
        None
    }

    /// What the transport's last accept gave this client to come back with; `None` before the
    /// first accept.
    fn rejoin(&self) -> Option<Rejoin> {
        None
    }

    /// The transport endpoint's counters; `None` before there is an endpoint.
    fn totals(&self) -> Option<crate::status::EndpointTotals>;

    /// The socket back, for a reconnect on the same transport (ADR-8352).
    fn into_socket(self: Box<Self>) -> Box<dyn DatagramSocket + Send>;

    /// The transport's connection quality of the link to the server (ADR-8182).
    fn link_quality(&self) -> Option<crate::quality::PeerQuality> {
        None
    }
}

/// What a session client's reconnect presents to the transport beside the session's reconnect
/// token (spec 120 § 10.3): the transport's resume ticket of the last accept and, on a sealed
/// link to a host that keeps tickets, its resumption credential. With the credential the
/// transport connect is a one-round-trip resume (wire 6, ADR-8858, ADR-8902, ADR-10088); with the
/// ticket alone, or when the host refuses the resume, it is the full handshake of three round
/// trips that presents the ticket.
#[derive(Clone)]
struct Rejoin {
    ticket: [u8; RESUME_TICKET_BYTES],
    credential: Option<ResumeCredential>,
}

/// The versions a session endpoint announces at the transport (version probe answers and stamped
/// `protocol_mismatch` denies): this build's wire version and the manifest's game version,
/// protocol version and content hash (ADR-7985).
fn manifest_stamp(manifest: &ContentManifest) -> crate::version_probe::VersionStamp {
    crate::version_probe::VersionStamp::new(
        crate::payload::REGIA_NET_WIRE_VERSION,
        &manifest.game_version,
        manifest.protocol_version,
        manifest.content_hash,
    )
}

fn map_transport(reason: TransportReason) -> DisconnectReason {
    match reason {
        TransportReason::Left => DisconnectReason::ClientQuit,
        TransportReason::Timeout => DisconnectReason::Timeout,
        TransportReason::MalformedPacket => DisconnectReason::MalformedPacket,
        TransportReason::Flood => DisconnectReason::Flood,
        TransportReason::ReliableOverflow => DisconnectReason::ReliableOverflow,
        other => DisconnectReason::TransportError(other.as_str().to_owned()),
    }
}

/// Most bytes of reliable session messages one link may hold back while its transport window is
/// full. Below it a full window is flow control (streams pause, the messages wait); past it the
/// backlog is a true overflow and the link ends (spec 120 § 10.3).
pub const MAX_DEFERRED_RELIABLE_BYTES: usize = 8 * 1024 * 1024;

/// Reliable session messages a link's transport refused for now, in send order.
#[derive(Debug, Default)]
struct Deferred {
    queue: std::collections::VecDeque<Vec<u8>>,
    bytes: usize,
}

impl Deferred {
    fn push(&mut self, bytes: Vec<u8>) {
        self.bytes += bytes.len();
        self.queue.push_back(bytes);
    }

    fn over_cap(&self) -> bool {
        self.bytes > MAX_DEFERRED_RELIABLE_BYTES
    }
}

/// What a refused send means for the session: wait and retry, or end the link.
fn send_refusal_is_flow_control(e: &crate::connection::SendError) -> bool {
    matches!(
        e,
        crate::connection::SendError::Backpressure(_) | crate::connection::SendError::RateLimited
    )
}

/// True when a link's reliable window is at least half full: the session stops adding stream
/// chunks before the transport has to refuse anything.
fn window_backed_up(in_flight: usize, window: usize) -> bool {
    in_flight.saturating_mul(2) >= window.max(1)
}

struct EndpointServer<S: DatagramSocket> {
    server: NetServer<SessionWire, S>,
    /// Reliable messages waiting for room in a link's window.
    deferred: BTreeMap<LinkId, Deferred>,
    /// Old links of token takeovers, dropped without a goodbye at the next flush.
    quiet: Vec<LinkId>,
}

impl<S: DatagramSocket + Send> EndpointServer<S> {
    /// Closes a link and tells the far end why. A link whose player the session holds in grace
    /// (a suspend's goodbye, the session's own silence timeout, an overflow on a silent link)
    /// keeps its transport ticket for the grace, as a transport timeout does (ADR-10088): the
    /// player's reconnect is then a one-round-trip resume of the same link.
    fn close(&mut self, core: &SessionServer, link: LinkId, reason: TransportReason) {
        if core.grace_holds_link(link) {
            self.server.kick_held(PeerId(link.0), reason);
        } else {
            self.server.kick(PeerId(link.0), reason);
        }
    }
}

impl<S: DatagramSocket + Send> ServerLink for EndpointServer<S> {
    fn receive(&mut self, now_ms: u64, core: &mut SessionServer) {
        // Ban decisions read the core's injected clock (ADR-4772), not the system clock.
        self.server.set_unix_now_s(core.now_unix_s());
        for peer in core.peer_ids() {
            if let Some(link) = core.link_of(peer) {
                self.server
                    .set_started(PeerId(link.0), core.peer_state(peer) == Some(PeerState::InGame));
            }
        }
        self.server.receive(now_ms);
        for p in self.server.session().peers() {
            core.note_link_activity(LinkId(p.id.0), p.last_seen_ms);
            // An authenticated rebind moved the peer: bans and rows follow the new address.
            core.set_link_addr(LinkId(p.id.0), p.addr);
        }
        while let Some(e) = self.server.poll_event() {
            match e {
                ServerEvent::PeerInterrupted { peer } => core.set_interrupted(LinkId(peer.0), true),
                ServerEvent::PeerRestored { peer } => core.set_interrupted(LinkId(peer.0), false),
                // A resumed transport peer keeps its id (ADR-10088): the core sees the link come up
                // again and waits for the hello with the session's reconnect token. Nothing held
                // for the connection the id had before may reach the new one.
                ServerEvent::PeerJoined { peer, .. } | ServerEvent::PeerResumed { peer, .. } => {
                    self.deferred.remove(&LinkId(peer.0));
                    let addr = self
                        .server
                        .session()
                        .peer(peer)
                        .map_or(SocketAddr::from(([0, 0, 0, 0], 0)), |p| p.addr);
                    core.link_up(LinkId(peer.0), addr, now_ms);
                    if !self.server.peer_sealed(peer) {
                        core.mark_link_plain(LinkId(peer.0));
                    }
                }
                ServerEvent::PeerLeft { peer, reason } => {
                    core.link_down(LinkId(peer.0), map_transport(reason), now_ms)
                }
                // The transport holds the timed-out player's id and ticket for the grace
                // (ADR-10088). The session holds the player itself only when it enters grace: for
                // any other player (one still loading its first join) the transport's hold has
                // nothing to come back to and ends here, so it never outlives the session's.
                ServerEvent::PeerDormant { peer, reason } => {
                    let link = LinkId(peer.0);
                    self.deferred.remove(&link);
                    core.link_down(link, map_transport(reason), now_ms);
                    if !core.grace_holds_link(link) {
                        self.server.forget_dormant(peer);
                    }
                }
                ServerEvent::Message { peer, message, .. } => core.receive(LinkId(peer.0), &message, now_ms),
                // Scored by the transport; a fatal one is followed by `PeerLeft` (ADR-7892).
                ServerEvent::Rejected { .. } => {}
                // Read through `NetSession::peer_quality` (ADR-8182).
                ServerEvent::QualityChanged { .. } => {}
            }
        }
        self.quiet.extend(core.take_quiet_closes());
    }

    fn flush(
        &mut self,
        now_ms: u64,
        core: &mut SessionServer,
        out: Vec<crate::session::lifecycle::Outgoing>,
        closes: Vec<LinkId>,
    ) {
        let mut failed: Vec<LinkId> = Vec::new();
        // Messages held back on earlier flushes go first, in order.
        for (link, d) in &mut self.deferred {
            while let Some(front) = d.queue.front() {
                let wire = crate::session::proto::wire_channel(Channel::GAME_RPC, front);
                match self.server.send(PeerId(link.0), wire, front) {
                    Ok(()) => {
                        d.bytes -= front.len();
                        d.queue.pop_front();
                    }
                    Err(e) if send_refusal_is_flow_control(&e) => break,
                    Err(e) => {
                        tracing::warn!(link = link.0, error = ?e, "net session: held reliable send failed; disconnecting");
                        failed.push(*link);
                        break;
                    }
                }
            }
        }
        self.deferred.retain(|_, d| !d.queue.is_empty());
        // The transport would hold a reliable message for a dormant player and deliver it after
        // its resume, ahead of the new welcome: nothing is sent to a link that is down.
        let dormant: Vec<PeerId> = self.server.dormant_peers().map(|(p, _)| p).collect();
        for o in out {
            if failed.contains(&o.link) || dormant.contains(&PeerId(o.link.0)) {
                continue;
            }
            // Order on the reliable channel: behind anything already held for this link.
            if o.channel == Channel::GAME_RPC
                && let Some(d) = self.deferred.get_mut(&o.link)
            {
                d.push(o.bytes);
                continue;
            }
            // Session control on stream 0, session streams on the bulk stream (ADR-8302).
            let wire = crate::session::proto::wire_channel(o.channel, &o.bytes);
            if let Err(e) = self.server.send(PeerId(o.link.0), wire, &o.bytes) {
                if o.channel != Channel::GAME_RPC {
                    tracing::debug!(link = o.link.0, error = ?e, "net session: send failed");
                } else if send_refusal_is_flow_control(&e) {
                    // A full window is flow control, not a broken link (spec 120 § 10.3): hold
                    // the message and retry on the next flush.
                    self.deferred.entry(o.link).or_default().push(o.bytes);
                } else {
                    // A lost reliable session message (a join record, the InGame marker, a stream
                    // chunk) would leave the peer stuck: end the connection instead.
                    tracing::warn!(link = o.link.0, error = ?e, "net session: reliable send failed; disconnecting");
                    failed.push(o.link);
                }
            }
        }
        for (link, d) in &self.deferred {
            if d.over_cap() && !failed.contains(link) {
                tracing::warn!(
                    link = link.0,
                    held = d.bytes,
                    "net session: reliable backlog past its cap; disconnecting"
                );
                failed.push(*link);
            }
        }
        for link in failed {
            self.deferred.remove(&link);
            // A backlog that grew while the link was silent enters grace (`peer_lost`).
            core.link_down(link, DisconnectReason::ReliableOverflow, now_ms);
            self.close(core, link, TransportReason::ReliableOverflow);
        }
        for link in std::mem::take(&mut self.quiet) {
            self.deferred.remove(&link);
            self.server.drop_quietly(PeerId(link.0), TransportReason::Kicked);
        }
        for link in closes {
            self.deferred.remove(&link);
            self.close(core, link, TransportReason::Kicked);
        }
        // The flow-control signal for the next tick's streams.
        let window = self.server.config().link.max_reliable_in_flight;
        for peer in core.peer_ids() {
            let Some(link) = core.link_of(peer) else {
                continue;
            };
            let in_flight = self
                .server
                .session()
                .peer(PeerId(link.0))
                .map_or(0, |p| p.link.reliable_in_flight);
            let backed_up = self.deferred.contains_key(&link) || window_backed_up(in_flight, window);
            core.set_link_backpressure(link, backed_up);
        }
        self.server.flush(now_ms);
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.server.local_addr()
    }

    fn server_key(&self) -> Option<ServerKey> {
        self.server.server_key()
    }

    fn link_stats(&self, link: LinkId) -> Option<LinkStats> {
        self.server.session().peer(PeerId(link.0)).map(|p| p.link)
    }

    fn add_relay_ip(&mut self, ip: std::net::IpAddr) {
        self.server.add_relay_ip(ip);
    }

    fn link_quality(&self, link: LinkId) -> Option<crate::quality::PeerQuality> {
        self.server.peer_quality(PeerId(link.0))
    }

    fn totals(&self) -> crate::status::EndpointTotals {
        self.server.totals()
    }
}

/// The code a client ends with when the server proved a key other than the one pinned for its
/// host in the known-hosts store (ADR-3283; the `NetLink` code of ADR-2416).
pub const HOST_KEY_CHANGED: &str = "net_host_key_changed";

struct EndpointClient<S: DatagramSocket> {
    client: NetClient<SessionWire, S>,
    /// Trust on first use for this connection; `None` with no store, an explicit key or
    /// `allow_unpinned`.
    pin: Option<FirstUsePin>,
    /// The store refused the key after the transport connected, and the connection was closed.
    refused: bool,
    /// Reliable messages waiting for room in the window (spec 120 § 10.3).
    deferred: Deferred,
    /// The transport's reliable window, fragments.
    window: usize,
}

impl<S: DatagramSocket + Send> EndpointClient<S> {
    /// The transport is up and the server proved its key: check it against the store before the
    /// session sends its `Hello` (which carries the password). `Err` = refuse with this code.
    fn trust_server(&mut self) -> Result<(), &'static str> {
        let (Some(pin), Some(key)) = (self.pin.as_mut(), self.client.server_key()) else {
            // No store, or a plain link: no key to pin.
            return Ok(());
        };
        match pin.on_connected(key) {
            Ok(_) => Ok(()),
            Err(e) => {
                // Another process pinned a different key between our load and this connection, or
                // the store went bad: do not stay connected to a key the store disagrees with.
                tracing::warn!(target: "regia_net", error = %e, "net session: known hosts refused the server's key");
                Err(e.code())
            }
        }
    }
}

impl<S: DatagramSocket + Send + 'static> ClientLink for EndpointClient<S> {
    fn into_socket(self: Box<Self>) -> Box<dyn DatagramSocket + Send> {
        Box::new(self.client.into_socket())
    }

    fn rejoin(&self) -> Option<Rejoin> {
        self.client.resume_ticket().map(|ticket| Rejoin {
            ticket,
            credential: self.client.resumption(),
        })
    }

    fn totals(&self) -> Option<crate::status::EndpointTotals> {
        Some(self.client.totals())
    }

    fn receive(&mut self, now_ms: u64, core: &mut SessionClient) {
        self.client
            .set_started(core.state() == &ClientSessionState::InGame);
        self.client.receive(now_ms);
        core.note_link_activity(self.client.last_heard_ms());
        while let Some(e) = self.client.poll_event() {
            match e {
                ClientEvent::PeerInterrupted => core.set_interrupted(true),
                ClientEvent::PeerRestored => core.set_interrupted(false),
                // The client has already raised its input lead (ADR-8077); the grade is read
                // through `NetSession::link_quality` (ADR-8182).
                ClientEvent::InputsLate { .. } | ClientEvent::QualityChanged { .. } => {}
                ClientEvent::Connected { .. } => match self.trust_server() {
                    Ok(()) => core.link_up(now_ms),
                    Err(code) => {
                        self.refused = true;
                        self.client.disconnect();
                        core.link_down(DisconnectReason::TransportError(code.to_owned()), now_ms);
                    }
                },
                ClientEvent::Message { message, .. } => core.receive(&message, now_ms),
                // The refusal's detail already named the side when the server stamped it; a
                // probe answer (or its absence) arrives after the link went down.
                ClientEvent::VersionMismatch { side, message, .. } => {
                    tracing::info!(target: "regia_net", side = side.as_str(), %message, "net session: version mismatch");
                }
                // The store already ended this connection and said why.
                ClientEvent::Ended(_) if self.refused => {}
                ClientEvent::Ended(end) => {
                    let pinned_by_store = self.pin.as_ref().is_some_and(FirstUsePin::pinned_by_store);
                    let reason = match end {
                        ClientEnd::Disconnected(r) => map_transport(r),
                        ClientEnd::ConnectTimeout => DisconnectReason::Timeout,
                        // Keep the transport's detail: it names both versions and the older side.
                        ClientEnd::Denied {
                            reason: crate::handshake::DenyReason::ProtocolMismatch,
                            detail,
                        } => DisconnectReason::ProtocolMismatch(detail),
                        // The key pinned on an earlier visit is not the one the server proved.
                        ClientEnd::ServerKeyMismatch if pinned_by_store => {
                            DisconnectReason::TransportError(HOST_KEY_CHANGED.to_owned())
                        }
                        other => DisconnectReason::TransportError(other.as_str().to_owned()),
                    };
                    core.link_down(reason, now_ms);
                }
            }
        }
    }

    fn flush(&mut self, now_ms: u64, core: &mut SessionClient) {
        let mut failed = false;
        while let Some(front) = self.deferred.queue.front() {
            let wire = crate::session::proto::wire_channel(Channel::GAME_RPC, front);
            match self.client.send(wire, front) {
                Ok(()) => {
                    self.deferred.bytes -= front.len();
                    self.deferred.queue.pop_front();
                }
                Err(e) if send_refusal_is_flow_control(&e) => break,
                Err(e) => {
                    tracing::warn!(error = ?e, "net session: held reliable send failed; disconnecting");
                    failed = true;
                    break;
                }
            }
        }
        for (channel, bytes) in core.take_outgoing() {
            if failed {
                break;
            }
            if channel == Channel::GAME_RPC && !self.deferred.queue.is_empty() {
                self.deferred.push(bytes);
                continue;
            }
            let wire = crate::session::proto::wire_channel(channel, &bytes);
            if let Err(e) = self.client.send(wire, &bytes) {
                if channel != Channel::GAME_RPC {
                    tracing::debug!(error = ?e, "net session: send failed");
                } else if send_refusal_is_flow_control(&e) {
                    self.deferred.push(bytes);
                } else {
                    tracing::warn!(error = ?e, "net session: reliable send failed; disconnecting");
                    failed = true;
                }
            }
        }
        if failed || self.deferred.over_cap() {
            self.deferred = Deferred::default();
            core.link_down(DisconnectReason::ReliableOverflow, now_ms);
            self.client.disconnect();
        }
        let in_flight = self.client.status().link.map_or(0, |l| l.reliable_in_flight);
        core.set_backpressure(!self.deferred.queue.is_empty() || window_backed_up(in_flight, self.window));
        if core.take_close() {
            self.client.disconnect();
        }
        self.client.flush(now_ms);
    }

    fn server_key(&self) -> Option<ServerKey> {
        self.client.server_key()
    }

    fn is_sealed(&self) -> bool {
        self.client.is_sealed()
    }

    fn link_stats(&self) -> Option<LinkStats> {
        self.client.status().link
    }

    fn link_quality(&self) -> Option<crate::quality::PeerQuality> {
        self.client.quality()
    }
}

/// A client that joins by code (spec 120 § 10.7, ADR-8183): the relay's `Lookup`, then the
/// direct attempt raced against a relay allocation ([`JoinPath`]); the endpoint starts once the
/// ticket is in, on the same socket.
struct JoinLink<S: DatagramSocket + Send + 'static> {
    path: JoinPath<S>,
    config: ClientConfig,
    client: Option<EndpointClient<JoinSocket<S>>>,
    failed: bool,
}

impl<S: DatagramSocket + Send + 'static> JoinLink<S> {
    fn fail(&mut self, code: &str, now_ms: u64, core: &mut SessionClient) {
        self.failed = true;
        if let Some(c) = &mut self.client {
            c.refused = true;
            c.client.disconnect();
        }
        core.link_down(DisconnectReason::TransportError(code.to_owned()), now_ms);
    }
}

impl<S: DatagramSocket + Send + 'static> ClientLink for JoinLink<S> {
    fn rejoin(&self) -> Option<Rejoin> {
        self.client.as_ref().and_then(|c| c.rejoin())
    }

    fn totals(&self) -> Option<crate::status::EndpointTotals> {
        self.client.as_ref().and_then(|c| c.totals())
    }

    fn receive(&mut self, now_ms: u64, core: &mut SessionClient) {
        if self.failed {
            return;
        }
        self.path.update(now_ms);
        if let Some(code) = self.path.error().map(JoinError::code) {
            return self.fail(code, now_ms, core);
        }
        if self.client.is_none()
            && let Some(ticket) = self.path.ticket().cloned()
            && let Some(socket) = self.path.take_socket()
        {
            // An explicit key wins; otherwise the key the host deposited with its join code.
            let config = ClientConfig {
                server_key: self.config.server_key.or(ticket.server_key),
                ..self.config.clone()
            };
            let window = config.link.max_reliable_in_flight;
            match NetClient::connect(socket, ticket.addr, config, &()) {
                Ok(client) => {
                    self.client = Some(EndpointClient {
                        client,
                        pin: None,
                        refused: false,
                        deferred: Deferred::default(),
                        window,
                    });
                }
                Err(e) => return self.fail(&format!("join_connect_failed: {e}"), now_ms, core),
            }
        }
        if let Some(c) = &mut self.client {
            c.receive(now_ms, core);
        }
    }

    fn flush(&mut self, now_ms: u64, core: &mut SessionClient) {
        match &mut self.client {
            Some(c) if !self.failed => c.flush(now_ms, core),
            _ => {
                if core.take_close() {
                    self.failed = true;
                }
            }
        }
    }

    fn server_key(&self) -> Option<ServerKey> {
        self.client.as_ref().and_then(|c| c.server_key())
    }

    fn is_sealed(&self) -> bool {
        self.client.as_ref().is_some_and(|c| c.is_sealed())
    }

    fn link_stats(&self) -> Option<LinkStats> {
        self.client.as_ref().and_then(|c| c.link_stats())
    }

    fn join_route(&self) -> Option<JoinRoute> {
        Some(self.path.route())
    }

    /// A join by code keeps no `last_connect`, so the reconnect never asks for this socket; it is
    /// the endpoint's once the path chose a route, else a closed one (ADR-8352, ADR-8487).
    fn into_socket(self: Box<Self>) -> Box<dyn DatagramSocket + Send> {
        match self.client {
            Some(c) => Box::new(c.client.into_socket()),
            None => Box::new(ClosedSocket),
        }
    }
}

/// The socket of a join by code that never reached its host: every call fails `NotConnected`.
struct ClosedSocket;

impl DatagramSocket for ClosedSocket {
    fn send_to(&mut self, _bytes: &[u8], _dest: SocketAddr) -> std::io::Result<()> {
        Err(std::io::ErrorKind::NotConnected.into())
    }

    fn recv_from(&mut self, _buf: &mut [u8]) -> std::io::Result<Option<(usize, SocketAddr)>> {
        Err(std::io::ErrorKind::NotConnected.into())
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        Err(std::io::ErrorKind::NotConnected.into())
    }
}

enum Role {
    Idle,
    Server {
        core: Box<SessionServer>,
        sims: Box<LoopbackClients>,
        link: Option<Mutex<Box<dyn ServerLink>>>,
        dedicated: bool,
    },
    Client {
        core: Box<SessionClient>,
        link: Option<Mutex<Box<dyn ClientLink>>>,
        server: Option<SocketAddr>,
    },
}

/// The session resource (spec 77 § 11).
#[derive(Resource)]
pub struct NetSession {
    settings: SessionSettings,
    manifest: ContentManifest,
    role: Role,
    start: std::time::Instant,
    /// The unix-epoch second `start` was taken at: with the monotonic session clock it gives the
    /// wall time of any step without reading the system clock mid-step (ADR-4772).
    start_unix_s: u64,
    manual_ms: Option<u64>,
    rules: Arc<Mutex<Box<dyn SessionRules>>>,
    /// True after [`Self::set_rules`]. The session bridge installs its default rules only while
    /// this is false, so an explicit `set_session_rules` is never replaced.
    rules_explicit: bool,
    conditioner: SessionConditioner,
    /// Clients send `LevelReady` as soon as a level is named (games with real level loading turn
    /// this off and call [`NetSession::level_loaded`]).
    pub auto_level_ready: bool,
    server_events: Vec<ServerSessionEvent>,
    client_events: Vec<ClientSessionEvent>,
    players: BTreeMap<NetPlayerId, Entity>,
    last_session_id: Option<u64>,
    last_token: Option<[u8; 16]>,
    /// What the transport's last accept gave this client to come back with (ADR-10088): kept
    /// beside `last_token` and presented by every connect with `resume: true`.
    last_rejoin: Option<Rejoin>,
    /// The last client connect (server address and options), so an Android `resumed` can
    /// reconnect after a suspend.
    last_connect: Option<(SocketAddr, ConnectOptions)>,
    /// The last client connect went through [`Self::connect_on`] with a caller-owned datagram
    /// socket (a Steam lobby socket with a synthetic address, a loopback test socket): plain UDP
    /// `resume` cannot rebuild that socket, so `resume` refuses and the caller reconnects with
    /// [`Self::resume_on`]. `connect` (UDP) clears it.
    last_connect_custom: bool,
    /// How [`Self::resume`] opens its socket: the transport kind of the last connect (`None` after
    /// a [`Self::connect_on`] with no [`Self::set_socket_opener`]: UDP, the old behavior).
    opener: Option<SocketOpener>,
    /// Set by [`Self::suspend`]: [`Self::resume`] reconnects with the token while the server's
    /// grace slot lasts.
    suspended: bool,
    /// How a client reconnects by itself after a timeout (ADR-8352).
    reconnect_policy: ReconnectPolicy,
    /// The automatic reconnect of the current outage.
    reconnecting: Option<Reconnector>,
    sim_endings: std::collections::VecDeque<DisconnectReason>,
    identity: Option<ServerIdentity>,
    /// The store's join ticket hook every host of this session installs (KI-NT-047).
    ticket_check: Option<Arc<dyn crate::ticket::JoinTicketCheck>>,
    /// `replication.android_listen_max_clients` (spec 72 § 14), synced from the `NetSettings`
    /// resource every frame until a host reads it; the spec default until then.
    android_listen_max_clients: u32,
    /// The known-hosts store clients pin server keys in (ADR-3283).
    known_hosts: Option<PathBuf>,
    /// [`Self::set_known_hosts`] was called, so the plugin leaves the store alone.
    known_hosts_set: bool,
    /// Where a listen host loads or creates its persistent identity when the game set none
    /// ([`Self::set_listen_identity_path`], ADR-4936).
    listen_identity_path: Option<PathBuf>,
    /// [`Self::set_listen_identity_path`] was called, so the plugin leaves the path alone.
    listen_identity_set: bool,
    /// Dev builds: the registered protocol types, named in a `protocol_mismatch` (spec 77 § 4).
    protocol_types: Vec<(String, u64)>,
    /// The mods the mod manager has mounted (spec 77 § 4.1), added to the manifest at session
    /// start; `None` without a mod manager.
    mounted_mods: Option<Vec<ManifestMod>>,
    /// The match rules' content hash the manifest carries (ADR-8694); `None` adds no entry.
    rules_hash: Option<u64>,
    /// The `ModsVfs` revision `mounted_mods` was read at.
    mods_revision: Option<u64>,
    /// The server core's join source reads its snapshots here (spec 77 § 6.1, ADR-7212).
    join_feed: Arc<Mutex<JoinFeed>>,
    /// Client join snapshot counters.
    client_join: JoinSnapshotCounts,
    /// What the last travel despawned and kept (server).
    travel_counts: TravelCounts,
    /// The `NetId`s kept across the last travel (server), at most [`MAX_PERSISTENT_IDS`].
    persistent_ids: Vec<u32>,
    /// What the last `TravelBegin` dropped and kept on this client (ADR-7259).
    client_travel: TravelCounts,
    /// The relay join codes resolve through ([`Self::set_relay`]).
    relay: Option<SocketAddr>,
    /// The last join by code (relay, code, options), so [`Self::resume`] joins the same way.
    last_code: Option<(SocketAddr, String, ConnectOptions)>,
}

/// Persistent `NetId`s a session remembers from its last travel (the `net.session` status).
pub const MAX_PERSISTENT_IDS: usize = 64;

/// The snapshots `net_session_system` captured from the world for peers that may join this step,
/// read by the server core's join source.
#[derive(Debug, Default)]
struct JoinFeed {
    /// The server tick the snapshots were captured at.
    tick: u64,
    pending: BTreeMap<PeerId, crate::session::lifecycle::JoinSnapshot>,
    counts: JoinSnapshotCounts,
    /// The replication inputs `pending` was captured from. A peer reuses its capture until the
    /// stamp changes (and `pending` holds no entry for it until its first capture).
    stamp: JoinStamp,
}

/// The inputs of a join snapshot capture: the replicated entity set, the resolved owners, the
/// recorded level removals and the profiles. A peer's snapshot is re-captured only when one of
/// these changes; an entity gaining or losing `NetProfileRef` between changes is reflected with
/// the next change (profiles themselves bump `NetProfiles::generation`).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct JoinStamp {
    entity_gen: u64,
    owners: (u64, u64),
    removed: Vec<u128>,
    profiles: u64,
}

/// The stamp of `world`'s replication state (what a join snapshot capture reads).
fn join_stamp(world: &World) -> JoinStamp {
    let rep = world.get_resource::<crate::replication::ServerReplication>();
    JoinStamp {
        entity_gen: rep.map(|r| r.entity_generation()).unwrap_or(0),
        owners: rep.map(|r| r.owners_signature()).unwrap_or((0, 0)),
        removed: rep
            .map(crate::replication::ServerReplication::level_removed_guids)
            .unwrap_or_default(),
        profiles: world
            .get_resource::<crate::profile::NetProfiles>()
            .map(|p| p.generation())
            .unwrap_or(u64::MAX),
    }
}

impl std::fmt::Debug for NetSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetSession")
            .field("role", &self.role_name())
            .finish()
    }
}

impl Default for NetSession {
    fn default() -> Self {
        Self::new(
            SessionSettings::default(),
            ContentManifest::build("", "", crate::settings::NET_PROTOCOL_VERSION, 0, &[]),
        )
    }
}

fn lock<T: ?Sized>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl NetSession {
    /// An idle session with these settings and this process's content manifest.
    pub fn new(settings: SessionSettings, manifest: ContentManifest) -> Self {
        Self {
            settings,
            manifest,
            role: Role::Idle,
            start: std::time::Instant::now(),
            start_unix_s: crate::session::unix_now_s(),
            manual_ms: None,
            rules: Arc::new(Mutex::new(Box::new(DefaultSessionRules))),
            rules_explicit: false,
            conditioner: SessionConditioner::default(),
            auto_level_ready: true,
            server_events: Vec::new(),
            client_events: Vec::new(),
            players: BTreeMap::new(),
            last_session_id: None,
            last_token: None,
            last_rejoin: None,
            last_connect: None,
            last_connect_custom: false,
            opener: None,
            suspended: false,
            reconnect_policy: ReconnectPolicy {
                window_ms: 0,
                ..ReconnectPolicy::default()
            },
            reconnecting: None,
            sim_endings: std::collections::VecDeque::new(),
            identity: None,
            ticket_check: None,
            android_listen_max_clients: crate::settings::ANDROID_LISTEN_MAX_CLIENTS_DEFAULT,
            known_hosts: None,
            known_hosts_set: false,
            listen_identity_path: None,
            listen_identity_set: false,
            protocol_types: Vec::new(),
            mounted_mods: None,
            rules_hash: None,
            mods_revision: None,
            join_feed: Arc::new(Mutex::new(JoinFeed::default())),
            client_join: JoinSnapshotCounts::default(),
            travel_counts: TravelCounts::default(),
            persistent_ids: Vec::new(),
            client_travel: TravelCounts::default(),
            relay: None,
            last_code: None,
        }
    }

    /// The manifest this process presents at session start: its packaged (or set) manifest plus
    /// the mods the mod manager has mounted (spec 77 § 4.1), with the content hash recomputed.
    pub fn session_manifest(&self) -> ContentManifest {
        let manifest = self.mods_manifest();
        match self.rules_hash {
            Some(hash) => manifest.with_rules(hash),
            None => manifest,
        }
    }

    /// Sets the match rules' content hash the session manifest carries (ADR-8694): the host and every
    /// client set the hash of the rule sets they resolved (`ResolvedRules::content_hash`, or
    /// `regia_rules::ruleset::combined_hash` for a playlist), so a client whose rules differ is
    /// refused with `content_mismatch`. `None` (the default) adds no entry. A running session keeps
    /// the manifest it started with.
    pub fn set_rules_hash(&mut self, hash: Option<u64>) {
        self.rules_hash = hash;
    }

    /// The match rules' content hash the next session start adds to the manifest.
    pub fn rules_hash(&self) -> Option<u64> {
        self.rules_hash
    }

    fn mods_manifest(&self) -> ContentManifest {
        match &self.mounted_mods {
            Some(mods) if !mods.is_empty() => {
                let mut all = self.manifest.mods.clone();
                for m in mods {
                    if !all.iter().any(|have| have.id == m.id) {
                        all.push(m.clone());
                    }
                }
                self.manifest.clone().with_mods(all)
            }
            _ => self.manifest.clone(),
        }
    }

    /// Sets the mounted mods added to the manifest at session start (the plugin reads them from
    /// `ModsVfs`; `None` = no mod manager). A running session keeps the manifest it started with.
    pub fn set_mounted_mods(&mut self, mods: Option<Vec<ManifestMod>>) {
        self.mounted_mods = mods;
    }

    /// The mounted mods the next session start adds to the manifest.
    pub fn mounted_mods(&self) -> Option<&[ManifestMod]> {
        self.mounted_mods.as_deref()
    }

    /// Sets the registered protocol types (`runtime::protocol_types`). Dev builds exchange them on
    /// a `protocol_mismatch` to name the differing types (spec 77 § 4 item 2); the plugin sets them
    /// at `finish`.
    pub fn set_protocol_types(&mut self, types: Vec<(String, u64)>) {
        self.protocol_types = types;
    }

    /// The registered protocol types this session compares on a mismatch (dev builds).
    pub fn protocol_types(&self) -> &[(String, u64)] {
        &self.protocol_types
    }

    /// Join snapshot counters (spec 77 § 6.1): on a server what it sent, on a client what it
    /// received and applied.
    pub fn join_counts(&self) -> JoinSnapshotCounts {
        match &self.role {
            Role::Server { .. } => lock(&self.join_feed).counts,
            _ => self.client_join,
        }
    }

    /// What the last travel despawned and kept (server, spec 77 § 6.2).
    pub fn travel_counts(&self) -> TravelCounts {
        self.travel_counts
    }

    /// The `NetId`s kept across the last travel (server), sorted, at most [`MAX_PERSISTENT_IDS`].
    pub fn persistent_ids(&self) -> &[u32] {
        &self.persistent_ids
    }

    /// What the last `TravelBegin` did on this client (spec 77 § 6.2 step 3, ADR-7259): the
    /// replicated copies it dropped at once and the persistent copies it kept. Zero when the server
    /// sent no keep list (the despawn records then remove the copies).
    pub fn client_travel_counts(&self) -> TravelCounts {
        self.client_travel
    }

    /// The known-hosts store (`known_hosts.ron`, ADR-2416) a client checks the server's key
    /// against: trust on first use, a changed key refused with [`HOST_KEY_CHANGED`] before the
    /// `Hello` (and its password) is sent (ADR-3283). `None` turns pinning off. [`NetSessionPlugin`]
    /// sets `user://known_hosts.ron` when the app has a `user://` mount and this was never called.
    pub fn set_known_hosts(&mut self, path: Option<PathBuf>) {
        self.known_hosts = path;
        self.known_hosts_set = true;
    }

    /// The known-hosts store, if pinning is on.
    pub fn known_hosts(&self) -> Option<&Path> {
        self.known_hosts.as_deref()
    }

    /// The static key the next [`Self::host`] uses; `None` makes a new one per run unless a
    /// listen identity path is set ([`Self::set_listen_identity_path`], ADR-4936). Clients pin
    /// the public half, so a server they should recognize keeps one.
    pub fn set_identity(&mut self, identity: Option<ServerIdentity>) {
        self.identity = identity;
    }

    /// Installs (`Some`) or removes the store's join ticket hook (Steam's `BeginAuthSession`
    /// through `regia_steam::SteamAuthSessions`, or any [`crate::JoinTicketCheck`]; KI-NT-047,
    /// ADR-10069). Every later host of this session uses it, and a running server takes it at
    /// once. The client's ticket is [`ConnectOptions::auth`]; the rules are
    /// [`SessionServer::set_ticket_check`]'s (verdict after the password, a late refusal kicks
    /// with `ticket_refused`, `end_ticket` when the player is gone).
    pub fn set_join_ticket_check(&mut self, check: Option<Arc<dyn crate::ticket::JoinTicketCheck>>) {
        if let Some(core) = self.server_mut() {
            core.set_ticket_check(check.clone());
        }
        self.ticket_check = check;
    }

    /// Where a listen host loads or creates its persistent identity when [`Self::set_identity`]
    /// was never called: the file is created on the first host and reused on later runs, so
    /// returning clients whose store pins the key are not locked out by a fresh per-run key
    /// (ADR-4936). `None` keeps the ephemeral per-run key (the default for a bare
    /// [`NetSession`]). [`NetSessionPlugin`] sets `user://listen-server.key` when the app has a
    /// `user://` mount and this was never called; a store that cannot be read or written logs a
    /// warning and the host falls back to the ephemeral key.
    pub fn set_listen_identity_path(&mut self, path: Option<PathBuf>) {
        self.listen_identity_path = path;
        self.listen_identity_set = true;
    }

    /// The listen identity path, if one is set.
    pub fn listen_identity_path(&self) -> Option<&Path> {
        self.listen_identity_path.as_deref()
    }

    /// Server: its public key. Client: the key the server proved, once it did.
    pub fn server_key(&self) -> Option<ServerKey> {
        match &self.role {
            Role::Server { link: Some(l), .. } => lock(l).server_key(),
            Role::Client { link: Some(l), .. } => lock(l).server_key(),
            _ => None,
        }
    }

    /// The counters of the session's transport endpoint (spec 120 § 14): a host's server
    /// endpoint, a client's connection to it. `None` while idle, on a host without a network
    /// transport, and for a client that joins by code until the path is chosen. A host's
    /// `resumed_one_rtt` counts the reconnects that were one-round-trip transport resumes
    /// (wire 6, ADR-10088).
    pub fn transport_totals(&self) -> Option<crate::status::EndpointTotals> {
        match &self.role {
            Role::Server { link: Some(l), .. } => Some(lock(l).totals()),
            Role::Client { link: Some(l), .. } => lock(l).totals(),
            _ => None,
        }
    }

    /// True for a client whose link to the server is sealed.
    pub fn link_sealed(&self) -> bool {
        matches!(&self.role, Role::Client { link: Some(l), .. } if lock(l).is_sealed())
    }

    /// Sends a remote admin message (client, spec 76 § 7).
    pub fn send_admin(&mut self, message: crate::session::proto::SessionMsg) {
        if let Role::Client { core, .. } = &mut self.role {
            core.send_admin(message);
        }
    }

    /// Uses simulated time from now on (tests): [`Self::update`] reads this clock.
    pub fn set_manual_clock(&mut self, now_ms: u64) {
        self.manual_ms = Some(now_ms);
    }

    /// Replaces the unix base [`Self::now_unix_s`] builds on (tests, replays). The default base
    /// is the wall second at construction; a replay of a recorded session supplies the recorded
    /// value so its ban and join decisions land at the recorded times on any host (ADR-4772).
    /// Pair with [`Self::set_manual_clock`], which supplies the recorded session clock.
    pub fn set_unix_base_s(&mut self, unix_s: u64) {
        self.start_unix_s = unix_s;
    }

    /// Advances the simulated clock.
    pub fn advance_manual_clock(&mut self, ms: u64) {
        let now = self.manual_ms.unwrap_or(0) + ms;
        self.manual_ms = Some(now);
    }

    /// Milliseconds on the session clock (monotonic, or the simulated one).
    pub fn now_ms(&self) -> u64 {
        self.manual_ms
            .unwrap_or_else(|| self.start.elapsed().as_millis() as u64)
    }

    /// Unix-epoch seconds on the session clock: the base [`Self::set_unix_base_s`] set (the wall
    /// second at construction by default) plus the session clock. Ban and join decisions read
    /// this — never the system clock mid-step — so two hosts fed the same inputs agree on who is
    /// banned (ADR-4772).
    pub fn now_unix_s(&self) -> u64 {
        self.start_unix_s.saturating_add(self.now_ms() / 1000)
    }

    /// The settings.
    pub fn settings(&self) -> &SessionSettings {
        &self.settings
    }

    /// Replaces the settings (idle sessions only take effect at the next host/connect).
    pub fn set_settings(&mut self, settings: SessionSettings) {
        self.settings = settings;
    }

    /// This process's content manifest.
    pub fn manifest(&self) -> &ContentManifest {
        &self.manifest
    }

    /// Sets the protocol hash of this process's manifest (the types the game registered).
    pub fn set_protocol_hash(&mut self, hash: u64) {
        self.manifest.protocol_hash = hash;
    }

    /// Replaces the manifest (before hosting or connecting).
    pub fn set_manifest(&mut self, manifest: ContentManifest) {
        self.manifest = manifest;
    }

    /// Installs the game's session rules. An explicit install wins over the session bridge.
    pub fn set_rules(&mut self, rules: impl SessionRules) {
        self.rules = Arc::new(Mutex::new(Box::new(rules)));
        self.rules_explicit = true;
    }

    /// True when the game installed rules with [`Self::set_rules`].
    pub fn has_explicit_rules(&self) -> bool {
        self.rules_explicit
    }

    /// Installs `rules` when the game has not called [`Self::set_rules`].
    ///
    /// Returns whether the rules were installed. The installed rules stay implicit, so a later
    /// [`Self::set_rules`] still replaces them.
    pub fn install_rules_unless_explicit(&mut self, rules: impl SessionRules) -> bool {
        if self.rules_explicit {
            return false;
        }
        self.rules = Arc::new(Mutex::new(Box::new(rules)));
        true
    }

    /// The rules (shared with the core's join hook).
    pub fn rules(&self) -> Arc<Mutex<Box<dyn SessionRules>>> {
        self.rules.clone()
    }

    /// `server`, `client` or `idle`.
    pub fn role_name(&self) -> &'static str {
        match self.role {
            Role::Idle => "idle",
            Role::Server { .. } => "server",
            Role::Client { .. } => "client",
        }
    }

    /// True for a dedicated server.
    pub fn is_dedicated(&self) -> bool {
        matches!(self.role, Role::Server { dedicated: true, .. })
    }

    /// The server core.
    pub fn server(&self) -> Option<&SessionServer> {
        match &self.role {
            Role::Server { core, .. } => Some(core),
            _ => None,
        }
    }

    /// The server core, mutably.
    pub fn server_mut(&mut self) -> Option<&mut SessionServer> {
        match &mut self.role {
            Role::Server { core, .. } => Some(core),
            _ => None,
        }
    }

    /// The client core.
    pub fn client(&self) -> Option<&SessionClient> {
        match &self.role {
            Role::Client { core, .. } => Some(core),
            _ => None,
        }
    }

    /// The server's bound transport address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.role {
            Role::Server { link: Some(l), .. } => lock(l).local_addr(),
            _ => None,
        }
    }

    /// The address a client connects to.
    pub fn server_addr(&self) -> Option<SocketAddr> {
        match &self.role {
            Role::Client { server, .. } => *server,
            _ => None,
        }
    }

    fn make_server(&self, opts: HostOptions, dedicated: bool) -> Box<SessionServer> {
        let mut core = SessionServer::new(
            self.settings.clone(),
            opts,
            self.session_manifest(),
            crate::handshake::fresh_secret(),
        );
        if cfg!(debug_assertions) {
            core.set_protocol_types(self.protocol_types.clone());
        }
        core.set_ticket_check(self.ticket_check.clone());
        // Spec 77 § 6.1 (ADR-7212): the join snapshot comes from replication; `net_session_system`
        // captures it from the world for the peers that may join this step.
        {
            let mut feed = lock(&self.join_feed);
            feed.pending.clear();
            feed.counts = JoinSnapshotCounts::default();
            feed.stamp = JoinStamp::default();
        }
        let feed = self.join_feed.clone();
        core.set_join_source(Box::new(move |peer| {
            let mut feed = lock(&feed);
            let tick = feed.tick;
            let snapshot =
                feed.pending
                    .remove(&peer)
                    .unwrap_or_else(|| crate::session::lifecycle::JoinSnapshot {
                        tick,
                        ..Default::default()
                    });
            feed.counts.snapshots += 1;
            feed.counts.removed += snapshot.removed.len() as u64;
            feed.counts.records += snapshot.records.len() as u64;
            snapshot
        }));
        let rules = self.rules.clone();
        core.set_join_rule(Box::new(move |hello: &Hello| {
            lock(&rules).on_player_joining(hello)
        }));
        // The capacity check clamps to this on Android even when a later `set_options` raises
        // `max_players` (spec 72 § 14).
        if cfg!(target_os = "android") && !dedicated {
            core.set_android_listen_cap(Some(self.android_listen_max_clients));
        }
        Box::new(core)
    }

    /// Hosts without a network transport: only loopback and simulated clients can join (tests,
    /// play-in-editor in one process).
    pub fn host_local(&mut self, opts: HostOptions, dedicated: bool) -> Result<u64, SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        // `Welcome` carries these in length-limited fields: a longer level is refused, longer texts
        // are cut (at a character boundary).
        if opts.level.len() > 255 {
            return Err(SessionError::LevelNotFound);
        }
        let mut opts = opts;
        truncate_text(&mut opts.name, 255);
        truncate_text(&mut opts.motd, 4096);
        // Android listen servers serve at most `replication.android_listen_max_clients` players
        // (spec 72 § 14): cap the options so the reported capacity matches what the capacity
        // check enforces. No-op on other platforms and on dedicated servers.
        opts.max_players = crate::settings::listen_capacity(
            cfg!(target_os = "android"),
            dedicated,
            opts.max_players,
            self.android_listen_max_clients,
        );
        let core = self.make_server(opts, dedicated);
        let id = core.session_id();
        let mut sims = LoopbackClients::new(self.settings.clone());
        sims.set_conditioner(self.conditioner);
        self.role = Role::Server {
            core,
            sims: Box::new(sims),
            link: None,
            dedicated,
        };
        self.last_session_id = Some(id);
        Ok(id)
    }

    /// Hosts on a datagram socket (UDP or the in-memory network). Returns the session id.
    pub fn host_on<S: DatagramSocket + Send + 'static>(
        &mut self,
        socket: S,
        opts: HostOptions,
        dedicated: bool,
    ) -> Result<u64, SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        let has_secret = !opts.password.is_empty() || opts.admin_token.is_some();
        if has_secret
            && let Ok(bind) = socket.local_addr()
            && secret_would_be_plain(self.settings.encryption, bind)
        {
            return Err(SessionError::PasswordNeedsEncryption);
        }
        // A listen host without an explicit identity persists one, the way the dedicated host
        // does, so returning clients whose known-hosts store pins the key are not locked out by a
        // fresh per-run key (ADR-4936). Unreadable or unwritable store: warn and stay ephemeral.
        let mut identity = self.identity.clone();
        if identity.is_none()
            && self.settings.encryption.can_encrypt()
            && let Some(path) = self.listen_identity_path.as_deref()
        {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match ServerIdentity::load_or_create(path) {
                Ok(id) => identity = Some(id),
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "net session: listen identity unavailable; hosting with a fresh per-run key")
                }
            }
        }
        let config = ServerConfig {
            health: crate::session::HealthPolicy::default().with_timeouts(self.settings.timeout_policy()),
            max_peers: (opts.max_players + self.settings.max_handshaking) as usize,
            tick_rate_hz: opts.tick_hz,
            crypto: self.settings.encryption,
            identity,
            link: crate::LinkConfig {
                rate: self.settings.rate_control.server(),
                ..crate::LinkConfig::default()
            },
            // The transport keeps a timed-out player's ticket for the session's reconnect grace
            // (ADR-10088): its reconnect is then a one-round-trip transport resume under the same
            // link, followed by the session's own token resume. 0 keeps none, as the session's
            // grace of 0 keeps no slot.
            resume_grace_ms: (self.settings.reconnect_grace_s.max(0.0) * 1000.0) as u64,
            // A listen server's clients share the host's home uplink (spec 120 § 4.2 item 8).
            shared_uplink: !dedicated && self.settings.rate_control.listen_shared_uplink,
            uplink_bytes_per_second: self.settings.rate_control.uplink_bytes_per_second,
            ..ServerConfig::default()
        };
        let mut server = NetServer::new(socket, config);
        server.set_version_stamp(manifest_stamp(&self.manifest));
        let id = self.host_local(opts, dedicated)?;
        if let Role::Server { link, .. } = &mut self.role {
            *link = Some(Mutex::new(Box::new(EndpointServer {
                server,
                deferred: BTreeMap::new(),
                quiet: Vec::new(),
            })));
        }
        Ok(id)
    }

    /// Hosts on UDP `bind` (spec 77 § 11 `host`).
    pub fn host(
        &mut self,
        bind: SocketAddr,
        opts: HostOptions,
        dedicated: bool,
    ) -> Result<u64, SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        let socket = UdpTransport::bind(bind).map_err(|e| SessionError::Transport(e.to_string()))?;
        self.host_on(socket, opts, dedicated)
    }

    /// What a connect with these options presents to the transport: the last accept's ticket and
    /// credential on a `resume: true` connect (every reconnect path), nothing on a fresh join.
    fn rejoin_for(&self, opts: &ConnectOptions) -> Option<Rejoin> {
        if opts.resume {
            self.last_rejoin.clone()
        } else {
            None
        }
    }

    fn hello(&self, opts: &ConnectOptions) -> Hello {
        Hello {
            protocol_version: crate::settings::MIN_NET_PROTOCOL_VERSION,
            protocol_hash: self.manifest.protocol_hash,
            manifest: self.session_manifest(),
            player_names: opts.player_names.clone(),
            password: opts.password.clone(),
            auth: opts.auth.clone(),
            client_rate_bytes: opts.client_rate_bytes,
            reconnect_token: if opts.resume { self.last_token } else { None },
        }
    }

    /// Connects on a datagram socket.
    ///
    /// The socket is caller-owned (a Steam lobby socket, a loopback test socket), so a later
    /// [`Self::resume`] cannot rebuild it unless [`Self::set_socket_opener`] names the socket kind
    /// (or [`Self::connect_via`] connected): after a [`Self::suspend`] the caller otherwise
    /// reconnects with [`Self::resume_on`].
    pub fn connect_on<S: DatagramSocket + Send + 'static>(
        &mut self,
        socket: S,
        server: SocketAddr,
        opts: ConnectOptions,
    ) -> Result<(), SessionError> {
        self.connect_on_keep_opener(socket, server, opts)?;
        self.opener = None;
        self.last_connect_custom = true;
        self.reconnecting = None;
        Ok(())
    }

    /// Connects on a socket `opener` opens, and keeps `opener` so [`Self::resume`] reconnects
    /// over the same transport kind (a Steam lobby client resumes over Steam sockets, a QUIC
    /// client over QUIC; spec 120 § 10.3).
    pub fn connect_via(
        &mut self,
        opener: SocketOpener,
        server: SocketAddr,
        opts: ConnectOptions,
    ) -> Result<(), SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        let socket = opener().map_err(|e| SessionError::Transport(e.to_string()))?;
        self.connect_on_keep_opener(socket, server, opts)?;
        self.opener = Some(opener);
        self.last_connect_custom = false;
        self.reconnecting = None;
        Ok(())
    }

    /// How a client reconnects by itself after a timeout (spec 120 § 10.3, ADR-8352). The default
    /// is on: fresh connects with the reconnect token, 1 s doubling to 8 s apart with jitter, over
    /// the transport kind the session connected with (its [`SocketOpener`], UDP, or the
    /// caller's own socket after [`Self::connect_on`]), for `window_ms` (0, the default: the
    /// session's `reconnect_grace_s`). [`ReconnectPolicy::off`]: a timeout ends the client.
    pub fn set_reconnect_policy(&mut self, policy: ReconnectPolicy) {
        self.reconnect_policy = policy;
    }

    /// The reconnect policy ([`Self::set_reconnect_policy`]).
    pub fn reconnect_policy(&self) -> ReconnectPolicy {
        self.reconnect_policy
    }

    /// True while the client reconnects by itself after a timeout (ADR-8352).
    pub fn is_reconnecting(&self) -> bool {
        self.reconnecting.is_some()
    }

    /// The automatic reconnect's progress, while it runs (ADR-8352).
    pub fn reconnect_status(&self) -> Option<crate::status::ReconnectStatus> {
        let r = self.reconnecting.as_ref()?;
        let now = self.now_ms();
        Some(crate::status::ReconnectStatus {
            attempt: r.attempts(),
            since_ms: now.saturating_sub(r.since_ms()),
            next_in_ms: r.next_at_ms().saturating_sub(now),
            window_left_ms: r.deadline_ms().saturating_sub(now),
        })
    }

    /// Names how [`Self::resume`] opens its socket (after a [`Self::connect_on`] on a socket that
    /// is not UDP); `None` goes back to UDP.
    pub fn set_socket_opener(&mut self, opener: Option<SocketOpener>) {
        if opener.is_some() {
            self.last_connect_custom = false;
        }
        self.opener = opener;
    }

    /// The transport kind [`Self::resume`] will use: `true` when a [`SocketOpener`] is set,
    /// `false` for the UDP fallback.
    pub fn has_socket_opener(&self) -> bool {
        self.opener.is_some()
    }

    fn connect_on_keep_opener<S: DatagramSocket + Send + 'static>(
        &mut self,
        socket: S,
        server: SocketAddr,
        opts: ConnectOptions,
    ) -> Result<(), SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        let has_password = opts.password.as_deref().is_some_and(|p| !p.is_empty());
        if has_password && secret_would_be_plain(self.settings.encryption, server) {
            return Err(SessionError::PasswordNeedsEncryption);
        }
        // Trust on first use (finding 22, ADR-3283): a key the store holds for this host becomes
        // the transport's pin, so a different key fails the handshake before any secret is sent.
        let (server_key, pin) = match (opts.server_key, self.known_hosts.as_deref()) {
            (None, Some(path)) if !opts.allow_unpinned => {
                let (pin, pinned) =
                    FirstUsePin::start(path, &server.to_string()).map_err(SessionError::KnownHosts)?;
                (pinned, Some(pin))
            }
            (key, _) => (key, None),
        };
        let rejoin = self.rejoin_for(&opts);
        let config = ClientConfig {
            name: opts.player_names.first().cloned().unwrap_or_default(),
            connect_timeout_ms: (self.settings.connect_timeout_s * 1000.0) as u64,
            crypto: self.settings.encryption,
            server_key,
            resume: rejoin.as_ref().map(|r| r.ticket),
            resumption: rejoin.and_then(|r| r.credential),
            link: crate::LinkConfig {
                rate: self.settings.rate_control.client(),
                ..crate::LinkConfig::default()
            },
            ..ClientConfig::default()
        }
        .with_timeouts(self.settings.timeout_policy());
        let window = config.link.max_reliable_in_flight;
        let mut client = NetClient::connect(socket, server, config, &())
            .map_err(|e| SessionError::Transport(e.to_string()))?;
        client.set_version_stamp(manifest_stamp(&self.manifest));
        let mut core = SessionClient::new(self.settings.clone());
        if cfg!(debug_assertions) {
            core.set_protocol_types(self.protocol_types.clone());
        }
        self.client_join = JoinSnapshotCounts::default();
        let now = self.now_ms();
        core.connect(self.hello(&opts), now);
        self.last_connect = Some((server, opts));
        self.last_code = None;
        self.suspended = false;
        self.role = Role::Client {
            core: Box::new(core),
            link: Some(Mutex::new(Box::new(EndpointClient {
                client,
                pin,
                refused: false,
                deferred: Deferred::default(),
                window,
            }))),
            server: Some(server),
        };
        Ok(())
    }

    /// The relay [`ConnectTarget::JoinCode`] resolves through when the code names none
    /// (`CODE@HOST:PORT` names one). `None` clears it.
    pub fn set_relay(&mut self, relay: Option<SocketAddr>) {
        self.relay = relay;
    }

    /// The relay set with [`Self::set_relay`].
    pub fn relay(&self) -> Option<SocketAddr> {
        self.relay
    }

    /// A server marks a relay's IP (spec 120 § 10.6, ADR-8183): every client that relay
    /// forwards shares the IP, so the per-IP caps count them by address. False when this
    /// session is not a networked server.
    pub fn add_relay_ip(&mut self, ip: std::net::IpAddr) -> bool {
        match &mut self.role {
            Role::Server { link: Some(l), .. } => {
                lock(l).add_relay_ip(ip);
                true
            }
            _ => false,
        }
    }

    /// The join-by-code path of a client that connected with [`ConnectTarget::JoinCode`]
    /// (`resolving`, `direct`, `direct_chosen`, `relayed`, `failed`); `None` otherwise.
    pub fn join_route(&self) -> Option<JoinRoute> {
        match &self.role {
            Role::Client { link: Some(l), .. } => lock(l).join_route(),
            _ => None,
        }
    }

    /// Joins by code on a datagram socket (spec 120 § 10.7, ADR-8183): looks `code` up on
    /// `relay`, tries the host's address and, when nothing answers within
    /// `server_host::RELAY_FALLBACK_MS`, plays through a relay allocation of its own, on this one
    /// socket. A failure ends the connection with a lowercase code: `join_timeout`,
    /// `join_refused`, `join_ticket_invalid`, `relay_too_old` (a protocol 1 relay cannot relay),
    /// `relay_full`, `relay_no_allocation`.
    ///
    /// # Errors
    ///
    /// Already in a session, a code that is not one (`transport_error` with
    /// `join_code_invalid`), or a password with encryption off.
    pub fn connect_by_code_on<S: DatagramSocket + Send + 'static>(
        &mut self,
        socket: S,
        relay: SocketAddr,
        code: &str,
        opts: ConnectOptions,
    ) -> Result<(), SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        let has_password = opts.password.as_deref().is_some_and(|p| !p.is_empty());
        if has_password && secret_would_be_plain(self.settings.encryption, relay) {
            return Err(SessionError::PasswordNeedsEncryption);
        }
        let path = JoinPath::new(socket, relay, code, fresh_allocation_key())
            .map_err(|e| SessionError::Transport(format!("{}: {e}", e.code())))?;
        let rejoin = self.rejoin_for(&opts);
        let config = ClientConfig {
            name: opts.player_names.first().cloned().unwrap_or_default(),
            connect_timeout_ms: (self.settings.connect_timeout_s * 1000.0) as u64,
            crypto: self.settings.encryption,
            server_key: opts.server_key,
            resume: rejoin.as_ref().map(|r| r.ticket),
            resumption: rejoin.and_then(|r| r.credential),
            ..ClientConfig::default()
        };
        let mut core = SessionClient::new(self.settings.clone());
        let now = self.now_ms();
        core.connect(self.hello(&opts), now);
        self.suspended = false;
        self.last_connect = None;
        self.last_code = Some((relay, code.to_owned(), opts));
        self.role = Role::Client {
            core: Box::new(core),
            link: Some(Mutex::new(Box::new(JoinLink {
                path,
                config,
                client: None,
                failed: false,
            }))),
            server: None,
        };
        Ok(())
    }

    /// Connects over UDP (spec 77 § 11 `connect`).
    pub fn connect(&mut self, target: ConnectTarget, opts: ConnectOptions) -> Result<(), SessionError> {
        if !matches!(self.role, Role::Idle) {
            return Err(SessionError::AlreadyInSession);
        }
        let addr = match target {
            ConnectTarget::Address(a) => a,
            ConnectTarget::JoinCode(text) => {
                // `CODE@HOST:PORT` names the relay; a bare code uses `set_relay`'s.
                let (code, relay) = match text.split_once('@') {
                    Some((code, at)) => {
                        let relay = match RelaySpec::parse(at) {
                            Ok(RelaySpec::Remote(a)) => a,
                            Ok(RelaySpec::Local(port)) => SocketAddr::from(([127, 0, 0, 1], port)),
                            Err(e) => return Err(SessionError::Transport(format!("{}: {e}", e.code()))),
                        };
                        (code.to_owned(), relay)
                    }
                    None => match self.relay {
                        Some(relay) => (text, relay),
                        None => {
                            return Err(SessionError::Transport(format!(
                                "join_relay_invalid: join code {text} needs a relay: NetSession::set_relay or CODE@HOST:PORT"
                            )));
                        }
                    },
                };
                let socket = UdpTransport::bind(crate::server_host::join::registrar_bind(relay))
                    .map_err(|e| SessionError::Transport(e.to_string()))?;
                return self.connect_by_code_on(socket, relay, &code, opts);
            }
        };
        self.connect_via(udp_socket_opener(addr), addr, opts)
    }

    /// Leaves: a client says goodbye (`client_quit`); a server drains and stops.
    pub fn disconnect(&mut self) {
        let now = self.now_ms();
        match &mut self.role {
            Role::Client { core, .. } => {
                // A quit cancels a pending auto-resume: the core keeps an in-flight goodbye's
                // reason, so without this a suspend-then-quit would still reconnect on `resume`.
                self.suspended = false;
                self.reconnecting = None;
                core.disconnect(DisconnectReason::ClientQuit, now)
            }
            Role::Server { core, .. } => core.drain(),
            Role::Idle => {}
        }
    }

    /// Suspends a client (Android `suspended`): goodbye with `suspended`, so the server keeps the
    /// grace slot. Every goodbye copy is written and flushed to the socket at once: the
    /// `Lifecycle` schedule that runs this keeps going while frames stop, so copies queued for
    /// later ticks would never leave. A client already saying goodbye keeps its reason (a
    /// `client_quit` must not become an auto-resume), and a second `suspend` while one is in
    /// flight keeps the pending auto-resume.
    pub fn suspend(&mut self) {
        let now = self.now_ms();
        // A suspend takes over from an automatic reconnect: `resume` reconnects instead.
        self.reconnecting = None;
        let mut suspended_client = false;
        if let Role::Client { core, link, .. } = &mut self.role {
            if self.suspended {
                // Already saying goodbye with `suspended` (a second suspend event): keep the
                // flag and the reason so `resume` still reconnects.
                return;
            }
            if matches!(
                core.state(),
                ClientSessionState::Disconnecting | ClientSessionState::Disconnected(_)
            ) {
                self.suspended = false;
                return;
            }
            core.disconnect(DisconnectReason::Suspended, now);
            // One goodbye copy leaves per `update` (GOODBYE_COPIES on the unreliable channel),
            // and frames stop at this transition: a hook that drives one copy out loses the
            // redundancy to a single lost datagram. Drive every copy out now.
            for _ in 0..crate::session::lifecycle::GOODBYE_COPIES {
                core.update(now);
                if let Some(l) = link {
                    lock(l).flush(now, core);
                }
            }
            suspended_client = true;
        }
        self.suspended = suspended_client;
    }

    /// Reconnects a client that [`Self::suspend`] suspended, now that the process survived the
    /// pause (Android `resumed`): reconnects with the last connect's options and `resume: true`,
    /// so the welcome's reconnect token reaches the server while it holds the slot (spec 77
    /// § 16), over the transport kind the session connected with: the [`SocketOpener`] of
    /// [`Self::connect_via`] / [`Self::connect`] / [`Self::set_socket_opener`], or UDP when none
    /// was given (spec 120 § 10.3). No-op (`Ok(false)`) when nothing was suspended.
    ///
    /// `Err(transport_error)` when the suspended session was opened with [`Self::connect_on`]
    /// (a Steam lobby socket, a loopback socket) and no [`Self::set_socket_opener`]: plain UDP
    /// cannot reach its synthetic address, and the session stays suspended so the caller
    /// reconnects it with [`Self::resume_on`].
    pub fn resume(&mut self) -> Result<bool, SessionError> {
        if !self.suspended {
            return Ok(false);
        }
        if let Some((relay, _, _)) = &self.last_code {
            let socket = UdpTransport::bind(crate::server_host::join::registrar_bind(*relay))
                .map_err(|e| SessionError::Transport(e.to_string()))?;
            return self.resume_on(socket);
        }
        if self.last_connect_custom {
            let error = SessionError::Transport(
                "the suspended session ran on a custom datagram socket; reconnect it with resume_on"
                    .to_owned(),
            );
            tracing::warn!(error = %error, "net session: cannot auto-resume a custom-socket session over UDP");
            return Err(error);
        }
        let Some((server, _)) = self.last_connect.clone() else {
            self.suspended = false;
            return Ok(false);
        };
        let opener = self.opener.clone().unwrap_or_else(|| udp_socket_opener(server));
        // The old socket closes before the new one opens: a Steam socket is one per channel.
        self.reset();
        let socket = opener().map_err(|e| SessionError::Transport(e.to_string()))?;
        self.resume_on(socket)
    }

    /// [`Self::resume`] on a custom datagram socket (tests, custom transports). The session
    /// keeps its [`SocketOpener`] for later resumes.
    pub fn resume_on<S: DatagramSocket + Send + 'static>(&mut self, socket: S) -> Result<bool, SessionError> {
        if !self.suspended {
            return Ok(false);
        }
        self.suspended = false;
        if let Some((relay, code, mut opts)) = self.last_code.clone() {
            // A join by code comes back by code: the path (direct or relayed) is chosen again.
            self.reset();
            opts.resume = true;
            self.connect_by_code_on(socket, relay, &code, opts)?;
            return Ok(true);
        }
        let Some((server, mut opts)) = self.last_connect.clone() else {
            return Ok(false);
        };
        // The goodbye (if it was not fully delivered) is dropped with the old role; the token
        // survives in `last_token` through `reset`.
        self.reset();
        opts.resume = true;
        self.connect_on_keep_opener(socket, server, opts)?;
        Ok(true)
    }

    /// Back to idle once a client is disconnected or a server finished draining.
    pub fn reset(&mut self) {
        if let Role::Client { core, link, .. } = &self.role {
            self.last_token = core.reconnect_token().or(self.last_token);
            // An endpoint that never connected has no ticket: the last accept's stays.
            if let Some(rejoin) = link.as_ref().and_then(|l| lock(l).rejoin()) {
                self.last_rejoin = Some(rejoin);
            }
        }
        self.role = Role::Idle;
        self.players.clear();
    }

    /// The client's state (`Disconnected(None)` when not a client).
    pub fn state(&self) -> ClientSessionState {
        match &self.role {
            Role::Client { core, .. } => core.state().clone(),
            _ => ClientSessionState::Disconnected(None),
        }
    }

    /// A peer's state on the server.
    pub fn peer_state(&self, peer: PeerId) -> Option<PeerState> {
        self.server().and_then(|s| s.peer_state(peer))
    }

    /// Travels (server).
    pub fn travel(&mut self, level: &str) -> Result<u32, SessionError> {
        // Level paths travel in a u8-length field (`Welcome`, `TravelBegin`).
        if level.trim().is_empty() || level.len() > 255 {
            return Err(SessionError::LevelNotFound);
        }
        let core = self.server_mut().ok_or(SessionError::NotServer)?;
        Ok(core.travel(level))
    }

    /// Kicks one player (server).
    pub fn kick(&mut self, player: NetPlayerId, reason: &str) -> Result<(), SessionError> {
        let core = self.server_mut().ok_or(SessionError::NotServer)?;
        if core.kick(player, reason) {
            Ok(())
        } else {
            Err(SessionError::PlayerUnknown)
        }
    }

    /// Kicks every player of a peer (server).
    pub fn kick_peer(&mut self, peer: PeerId, reason: &str) -> Result<(), SessionError> {
        let core = self.server_mut().ok_or(SessionError::NotServer)?;
        if core.kick_peer(peer, reason) {
            Ok(())
        } else {
            Err(SessionError::PlayerUnknown)
        }
    }

    /// Adds a ban entry and saves `bans.ron` (server). `BanStoreSaveFailed` when the entry was
    /// added to the in-memory list (it applies for this run) but the save failed.
    pub fn ban(
        &mut self,
        kind: BanKind,
        value: &str,
        until: Option<u64>,
        reason: &str,
    ) -> Result<BanId, SessionError> {
        let added = self.now_unix_s();
        let core = self.server_mut().ok_or(SessionError::NotServer)?;
        core.bans_mut()
            .add(kind, value, until, reason, added)
            .map_err(|e| match e {
                // The list rejected the entry (a bad address): nothing was added.
                BanError::BadAddress(v) => SessionError::BadBan(v),
                // The entry is in memory for this run; only the save failed.
                e => SessionError::BanStoreSaveFailed(e.to_string()),
            })
    }

    /// Starts a stream. On a client `to` is ignored (streams go to the server).
    pub fn send_stream(&mut self, to: PeerId, kind: &str, bytes: Vec<u8>) -> Result<StreamId, SessionError> {
        let map = |e| match e {
            StreamRefusal::TooLarge => SessionError::StreamTooLarge,
            StreamRefusal::TooMany => SessionError::TooManyStreams,
        };
        match &mut self.role {
            Role::Server { core, .. } => {
                if core.peer_state(to).is_none() {
                    return Err(SessionError::PlayerUnknown);
                }
                core.send_stream(to, kind, bytes).map_err(map)
            }
            Role::Client { core, .. } => core.send_stream(kind, bytes).map_err(map),
            Role::Idle => Err(SessionError::NotClient),
        }
    }

    /// The level of `travel_id` finished loading (client; with `auto_level_ready` off).
    pub fn level_loaded(&mut self, travel_id: u32) {
        let now = self.now_ms();
        if let Role::Client { core, .. } = &mut self.role {
            core.level_loaded(travel_id, now);
        }
    }

    /// The conditioner settings.
    pub fn conditioner(&self) -> &SessionConditioner {
        &self.conditioner
    }

    /// Sets the conditioner: applies to simulated and loopback clients added from now on, and
    /// (approximately: latency, jitter, loss, duplication, reorder) to the datagram transport.
    pub fn set_conditioner(&mut self, conditioner: SessionConditioner) {
        self.conditioner = conditioner;
        if let Role::Server { sims, .. } = &mut self.role {
            sims.set_conditioner(conditioner);
        }
    }

    /// Adds `n` simulated clients (server, dev builds). Returns the number now live.
    pub fn add_simulated_clients(&mut self, n: usize, input: SimInput) -> Result<usize, SessionError> {
        let now = self.now_ms();
        let manifest = self.session_manifest();
        let Role::Server { core, sims, .. } = &mut self.role else {
            return Err(SessionError::NotServer);
        };
        if sims.simulated_live().saturating_add(n) > MAX_SIMULATED_CLIENTS {
            return Err(SessionError::SimClientsLimit);
        }
        let password = core.options().password.clone();
        for i in 0..n {
            let input = match &input {
                SimInput::Random { seed } => SimInput::Random {
                    seed: seed.wrapping_add(i as u64),
                },
                other => other.clone(),
            };
            let hello = Hello {
                protocol_version: crate::settings::MIN_NET_PROTOCOL_VERSION,
                protocol_hash: self.manifest.protocol_hash,
                manifest: manifest.clone(),
                player_names: vec![format!("sim-{}", sims.clients().len() + 1)],
                password: (!password.is_empty()).then(|| password.clone()),
                auth: None,
                client_rate_bytes: 0,
                reconnect_token: None,
            };
            // Batches of 16 every 100 ms stay under `max_handshaking` (32 by default).
            let delay = (i / 16) as u64 * 100;
            sims.add_delayed(hello, now, delay, input, true);
        }
        Ok(sims.simulated_live())
    }

    /// Removes the newest `n` simulated clients. Returns the number still live.
    pub fn remove_simulated_clients(&mut self, n: usize) -> Result<usize, SessionError> {
        let now = self.now_ms();
        let Role::Server { sims, .. } = &mut self.role else {
            return Err(SessionError::NotServer);
        };
        sims.remove_simulated(n, now);
        Ok(sims.simulated_live() - n.min(sims.simulated_live()))
    }

    /// Removes every simulated client.
    pub fn clear_simulated_clients(&mut self) -> Result<usize, SessionError> {
        self.remove_simulated_clients(MAX_SIMULATED_CLIENTS).map(|_| 0)
    }

    /// The loopback/simulated clients of a server.
    pub fn simulated(&self) -> Option<&LoopbackClients> {
        match &self.role {
            Role::Server { sims, .. } => Some(sims),
            _ => None,
        }
    }

    /// The loopback/simulated clients of a server, mutably (tests drive their cores).
    pub fn simulated_mut(&mut self) -> Option<&mut LoopbackClients> {
        match &mut self.role {
            Role::Server { sims, .. } => Some(sims),
            _ => None,
        }
    }

    /// Why the most recent finished loopback/simulated clients ended (newest last, at most 256).
    pub fn simulated_endings(&self) -> impl Iterator<Item = &DisconnectReason> {
        self.sim_endings.iter()
    }

    /// Simulated clients connected or connecting.
    pub fn simulated_live(&self) -> usize {
        self.simulated().map_or(0, LoopbackClients::simulated_live)
    }

    /// Adds `n` in-memory clients that join like players and are not simulated (play-in-editor
    /// with several clients, spec 77 § 10.4). Returns the number of loopback clients now present.
    pub fn add_loopback_clients(&mut self, n: usize, input: SimInput) -> Result<usize, SessionError> {
        let now = self.now_ms();
        let manifest = self.session_manifest();
        let Role::Server { core, sims, .. } = &mut self.role else {
            return Err(SessionError::NotServer);
        };
        if sims.clients().len().saturating_add(n) > MAX_SIMULATED_CLIENTS {
            return Err(SessionError::SimClientsLimit);
        }
        let password = core.options().password.clone();
        for _ in 0..n {
            let hello = Hello {
                protocol_version: self.manifest.protocol_version,
                protocol_hash: self.manifest.protocol_hash,
                manifest: manifest.clone(),
                player_names: vec![format!("player-{}", sims.clients().len() + 1)],
                password: (!password.is_empty()).then(|| password.clone()),
                auth: None,
                client_rate_bytes: 0,
                reconnect_token: None,
            };
            sims.add(hello, now, input.clone(), false);
        }
        Ok(sims.clients().len())
    }

    /// One stats-overlay row per loopback or simulated client (spec 77 § 10.2). Empty unless this
    /// is a server.
    pub fn connection_stats(&self) -> Vec<ConnectionStats> {
        self.simulated()
            .map_or_else(Vec::new, |sims| sims.stats(self.now_ms()))
    }

    /// Ping and loss per connected peer on a server, whether or not its players have entities yet
    /// (the console's `players`, spec 76 § 6, ADR-5650); empty on a client or idle session.
    /// Datagram peers read their connection's `LinkStats`; loopback and simulated clients read
    /// their meters.
    pub fn peer_link_stats(&self) -> BTreeMap<PeerId, PlayerLinkStats> {
        let mut by_peer: BTreeMap<PeerId, PlayerLinkStats> = BTreeMap::new();
        let Role::Server { core, sims, link, .. } = &self.role else {
            return by_peer;
        };
        for c in sims.clients() {
            if let Some(peer) = c.core.peer_id() {
                by_peer.insert(peer, PlayerLinkStats::from_meter(c.meter(), c.simulated));
            }
        }
        if let Some(l) = link {
            let l = lock(l);
            for peer in core.peer_ids() {
                if by_peer.contains_key(&peer) {
                    continue;
                }
                if let Some(stats) = core.link_of(peer).and_then(|id| l.link_stats(id)) {
                    by_peer.insert(peer, PlayerLinkStats::from_link(&stats));
                }
            }
        }
        by_peer
    }

    /// Connection quality per datagram peer on a server (spec 120 § 13.1, ADR-8182); empty on a
    /// client, an idle session, and for loopback or simulated clients (they have no transport).
    pub fn peer_quality(&self) -> BTreeMap<PeerId, crate::quality::PeerQuality> {
        let mut by_peer = BTreeMap::new();
        if let Role::Server {
            core, link: Some(l), ..
        } = &self.role
        {
            let l = lock(l);
            for peer in core.peer_ids() {
                if let Some(q) = core.link_of(peer).and_then(|id| l.link_quality(id)) {
                    by_peer.insert(peer, q);
                }
            }
        }
        by_peer
    }

    /// The operator stats samples of every datagram peer on a server: address, quality and
    /// receive rate (spec 120 § 13.2, ADR-8182); empty otherwise.
    pub fn peer_stats_samples(&self) -> Vec<crate::stats::PeerSample> {
        let mut out = Vec::new();
        if let Role::Server {
            core, link: Some(l), ..
        } = &self.role
        {
            let l = lock(l);
            for row in core.peer_rows() {
                let peer = PeerId(row.peer);
                let Some(id) = core.link_of(peer) else {
                    continue;
                };
                if let Some(quality) = l.link_quality(id) {
                    let recv_kbps = l.link_stats(id).map_or(0.0, |s| s.recv_kbps);
                    out.push(crate::stats::PeerSample {
                        peer: row.peer,
                        addr: row.addr,
                        quality,
                        recv_bps: (recv_kbps * 125.0).round().clamp(0.0, f64::from(u32::MAX)) as u32,
                    });
                }
            }
        }
        out
    }

    /// Connection quality of the link to the server on a datagram client (ADR-8182); `None`
    /// otherwise.
    pub fn link_quality(&self) -> Option<crate::quality::PeerQuality> {
        match &self.role {
            Role::Client { link: Some(l), .. } => lock(l).link_quality(),
            _ => None,
        }
    }

    /// Ping and loss per remote player (server) and of the link to the server (client), from the
    /// transport's own estimates (spec 77 § 10.2, ADR-2413). Datagram peers read their connection's
    /// `LinkStats`; loopback and simulated clients read their meters.
    pub fn player_link_stats(&self) -> (Vec<(NetPlayerId, PlayerLinkStats)>, Option<PlayerLinkStats>) {
        match &self.role {
            Role::Idle => (Vec::new(), None),
            Role::Server { .. } => {
                let by_peer = self.peer_link_stats();
                let rows = self
                    .players
                    .keys()
                    .filter_map(|p| by_peer.get(&p.peer).map(|s| (*p, *s)))
                    .collect();
                (rows, None)
            }
            Role::Client { link, .. } => (
                Vec::new(),
                link.as_ref()
                    .and_then(|l| lock(l).link_stats())
                    .map(|s| PlayerLinkStats::from_link(&s)),
            ),
        }
    }

    /// The per-message-kind bandwidth table of the loopback and simulated clients (spec 77 § 10.2).
    pub fn bandwidth_view(&self) -> BandwidthView {
        match self.simulated() {
            Some(sims) => BandwidthView::from_meters(sims.clients().iter().map(|c| c.meter())),
            None => BandwidthView::default(),
        }
    }

    /// One session step at the session clock: transport receive, core update, transport send.
    pub fn update(&mut self) {
        let now = self.now_ms();
        self.update_at(now);
    }

    /// One session step at `now_ms`.
    pub fn update_at(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("net_session").entered();
        let mut fresh = Vec::new();
        let client = matches!(self.role, Role::Client { .. });
        match &mut self.role {
            Role::Idle => {}
            Role::Server { core, sims, link, .. } => {
                // Ban and join decisions read this clock (ADR-4772), not the system clock.
                core.set_unix_now_s(self.start_unix_s.saturating_add(now_ms / 1000));
                if let Some(l) = link {
                    lock(l).receive(now_ms, core);
                }
                let (out, closes) = sims.step(core, now_ms);
                if let Some(l) = link {
                    lock(l).flush(now_ms, core, out, closes);
                }
                for gone in sims.prune() {
                    if self.sim_endings.len() >= MAX_SIMULATED_CLIENTS {
                        self.sim_endings.pop_front();
                    }
                    self.sim_endings.push_back(
                        gone.disconnect_reason()
                            .cloned()
                            .unwrap_or(DisconnectReason::ClientQuit),
                    );
                }
                while let Some(e) = core.poll_event() {
                    push_bounded(&mut self.server_events, e);
                }
            }
            Role::Client { core, link, .. } => {
                if let Some(l) = link {
                    lock(l).receive(now_ms, core);
                }
                core.update(now_ms);
                let mut load = None;
                while let Some(e) = core.poll_event() {
                    if let ClientSessionEvent::LoadLevel { travel_id, .. } = &e {
                        load = Some(*travel_id);
                    }
                    if let ClientSessionEvent::Welcomed(w) = &e {
                        self.last_token = Some(w.reconnect_token);
                    }
                    fresh.push(e);
                }
                if self.auto_level_ready
                    && let Some(t) = load
                {
                    core.level_loaded(t, now_ms);
                }
                if let Some(l) = link {
                    lock(l).flush(now_ms, core);
                }
            }
        }
        // An attempt whose socket could not open leaves the role idle; the timeline still runs.
        if client || self.reconnecting.is_some() {
            self.drive_reconnect(now_ms, fresh);
        }
    }

    /// Reconnect bookkeeping after a client step (spec 120 § 10.3, ADR-8352): a timeout with a
    /// token starts the timeline instead of ending the session, attempts' own ends are swallowed,
    /// the first welcome ends the outage with `PeerRestored`, and the window's end gives up once.
    fn drive_reconnect(&mut self, now_ms: u64, events: Vec<ClientSessionEvent>) {
        let policy = ReconnectPolicy {
            window_ms: if self.reconnect_policy.window_ms == 0 {
                (self.settings.reconnect_grace_s.max(0.0) * 1000.0) as u64
            } else {
                self.reconnect_policy.window_ms
            },
            ..self.reconnect_policy
        };
        let can = policy.enabled
            && policy.window_ms > 0
            && self.last_token.is_some()
            && self.last_connect.is_some();
        for e in events {
            match e {
                // The timeout that starts the reconnect, and an attempt's own end, are not the
                // session's end.
                ClientSessionEvent::StateChanged {
                    to: ClientSessionState::Disconnected(ref why),
                    ..
                } if self.reconnecting.is_some()
                    || (can && matches!(why, Some(DisconnectReason::Timeout))) => {}
                ClientSessionEvent::Disconnected { reason } if self.reconnecting.is_some() => {
                    if !matches!(reason, DisconnectReason::Timeout | DisconnectReason::ServerFull) {
                        let attempts = self.reconnecting.take().map_or(0, |r| r.attempts());
                        push_bounded(
                            &mut self.client_events,
                            ClientSessionEvent::ReconnectFailed {
                                attempts,
                                reason: reason.clone(),
                            },
                        );
                        push_bounded(
                            &mut self.client_events,
                            ClientSessionEvent::Disconnected { reason },
                        );
                    }
                }
                ClientSessionEvent::Disconnected {
                    reason: DisconnectReason::Timeout,
                } if can => {
                    tracing::info!(
                        window_ms = policy.window_ms,
                        "net session: client timed out; reconnecting"
                    );
                    let seed = self.last_token.map_or(0, |t| crate::reconnect::seed_from(&t));
                    self.reconnecting = Some(Reconnector::start(policy, now_ms, seed));
                }
                ClientSessionEvent::Welcomed(w) if self.reconnecting.is_some() => {
                    let attempts = self.reconnecting.take().map_or(0, |r| r.attempts());
                    tracing::info!(attempts, peer = w.peer_id.0, "net session: client reconnected");
                    push_bounded(&mut self.client_events, ClientSessionEvent::Welcomed(w));
                    push_bounded(&mut self.client_events, ClientSessionEvent::PeerRestored);
                }
                other => push_bounded(&mut self.client_events, other),
            }
        }
        let step = match self.reconnecting.as_mut() {
            Some(r) => r.poll(now_ms),
            None => return,
        };
        match step {
            ReconnectStep::Wait => {}
            ReconnectStep::Attempt(attempt) => {
                push_bounded(
                    &mut self.client_events,
                    ClientSessionEvent::Reconnecting { attempt },
                );
                if let Err(e) = self.start_reconnect_attempt() {
                    tracing::warn!(error = %e, attempt, "net session: reconnect attempt could not start");
                }
            }
            ReconnectStep::GiveUp => {
                let attempts = self.reconnecting.take().map_or(0, |r| r.attempts());
                tracing::warn!(attempts, "net session: reconnect window ended");
                // The last attempt stops here: the session is over, nothing more goes out.
                self.reset();
                push_bounded(
                    &mut self.client_events,
                    ClientSessionEvent::ReconnectFailed {
                        attempts,
                        reason: DisconnectReason::Timeout,
                    },
                );
                push_bounded(
                    &mut self.client_events,
                    ClientSessionEvent::Disconnected {
                        reason: DisconnectReason::Timeout,
                    },
                );
            }
        }
    }

    /// One reconnect attempt: a fresh connect with `resume: true` (the reconnect token), on a
    /// socket of the kind the session connected with.
    fn start_reconnect_attempt(&mut self) -> Result<(), SessionError> {
        let Some((server, mut opts)) = self.last_connect.clone() else {
            return Ok(());
        };
        let old = match std::mem::replace(&mut self.role, Role::Idle) {
            Role::Client { core, link, .. } => {
                self.last_token = core.reconnect_token().or(self.last_token);
                let link = link.map(|l| l.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner));
                // An attempt that never connected has no ticket: every attempt of one outage
                // presents the last accept's, each with a newer flight.
                if let Some(rejoin) = link.as_ref().and_then(|l| l.rejoin()) {
                    self.last_rejoin = Some(rejoin);
                }
                link
            }
            // The last attempt's socket could not open.
            Role::Idle => None,
            other => {
                self.role = other;
                return Ok(());
            }
        };
        self.players.clear();
        let socket: Box<dyn DatagramSocket + Send> = match (&self.opener, old) {
            (Some(open), old) => {
                // The old socket closes first: a Steam socket is one per channel.
                drop(old);
                open().map_err(|e| SessionError::Transport(e.to_string()))?
            }
            (None, old) if self.last_connect_custom => match old {
                Some(old) => old.into_socket(),
                None => return Err(SessionError::Transport("no socket to reconnect on".to_owned())),
            },
            (None, old) => {
                drop(old);
                udp_socket_opener(server)().map_err(|e| SessionError::Transport(e.to_string()))?
            }
        };
        opts.resume = true;
        self.connect_on_keep_opener(socket, server, opts)
    }

    /// Takes the server events of the last updates.
    pub fn take_server_events(&mut self) -> Vec<ServerSessionEvent> {
        std::mem::take(&mut self.server_events)
    }

    /// Takes the client events of the last updates.
    pub fn take_client_events(&mut self) -> Vec<ClientSessionEvent> {
        std::mem::take(&mut self.client_events)
    }

    /// Test hook: records a declared player slot without a handshake.
    #[cfg(test)]
    pub(crate) fn declare_player_for_test(&mut self, player: NetPlayerId, entity: Entity) {
        self.players.insert(player, entity);
    }

    /// The entity of a remote player on the server.
    pub fn player_entity(&self, player: NetPlayerId) -> Option<Entity> {
        self.players.get(&player).copied()
    }

    /// The id of the current (or last) session.
    pub fn session_id(&self) -> Option<u64> {
        match &self.role {
            Role::Server { core, .. } => Some(core.session_id()),
            Role::Client { core, .. } => core.welcome().map(|w| w.session_id).or(self.last_session_id),
            Role::Idle => self.last_session_id,
        }
    }

    /// The loopback link base (ids at or above it are loopback/simulated clients).
    pub const LOOPBACK_LINK_BASE: u32 = LOOPBACK_LINK_BASE;
}

/// Syncs `replication.android_listen_max_clients` into the session, and into a live listen
/// server's core, from `NetSettings`. Runs at [`NetSessionPlugin::finish`] — before any system,
/// so a host from `Startup` cannot outrun the config — and every frame in
/// [`net_session_system`], so a changed config applies to a live server without a re-host.
fn sync_android_listen_cap(session: &mut NetSession, cap: Option<u32>) {
    if let Some(cap) = cap {
        session.android_listen_max_clients = cap;
    }
    // `make_server` installs the cap at host time on a non-dedicated Android server only; push it
    // into a live core so a config change (or a host that ran before the first sync) cannot keep
    // enforcing a stale value.
    if cfg!(target_os = "android")
        && let Role::Server {
            core,
            dedicated: false,
            ..
        } = &mut session.role
    {
        core.set_android_listen_cap(Some(session.android_listen_max_clients));
    }
}

/// Server: copies the session's peer states and declared rates into
/// [`crate::relevance::NetSessionPeers`] for replication (`sent_to_all` waits for `InGame`, the
/// declared rate lowers the budget; spec 72 §§ 5.1, 7, ADR-7213). Not a server: the resource is
/// left alone (a test or a game may fill it). The map keeps its capacity: no steady allocation.
fn publish_session_peers(world: &mut World) {
    use crate::relevance::{NetSessionPeers, SessionPeer};
    let row = |state: PeerState, declared: Option<u32>| SessionPeer {
        in_game: state == PeerState::InGame,
        declared_rate_bytes: declared,
    };
    let Some(server) = world.get_resource::<NetSession>().and_then(NetSession::server) else {
        return;
    };
    // Compared in place first: a steady frame allocates nothing.
    let same = world.get_resource::<NetSessionPeers>().is_some_and(|out| {
        out.peers.len() == server.peer_rates().count()
            && server
                .peer_rates()
                .all(|(peer, state, declared)| out.peers.get(&peer) == Some(&row(state, declared)))
    });
    if same {
        return;
    }
    let rows: Vec<(PeerId, SessionPeer)> = server
        .peer_rates()
        .map(|(peer, state, declared)| (peer, row(state, declared)))
        .collect();
    let mut out = world.get_resource_or_insert_with(NetSessionPeers::default);
    out.peers.clear();
    out.peers.extend(rows);
}

/// Runs the session once per frame and applies its events to the world: player entities from
/// `Welcome` to `Gone`, [`NetDropped`] during the grace, and the [`SessionRules`] hooks.
pub fn net_session_system(world: &mut World) {
    let _span = tracing::trace_span!("net_session").entered();
    let android_listen_max_clients = world
        .get_resource::<crate::settings::NetSettings>()
        .map(|s| s.replication.android_listen_max_clients);
    let Some(mut session) = world.get_resource_mut::<NetSession>() else {
        return;
    };
    sync_android_listen_cap(&mut session, android_listen_max_clients);
    sync_mounted_mods(world);
    feed_join_snapshots(world);
    feed_travel_keep(world);
    let Some(mut session) = world.get_resource_mut::<NetSession>() else {
        return;
    };
    session.update();
    publish_session_peers(world);
    let Some(mut session) = world.get_resource_mut::<NetSession>() else {
        return;
    };
    let server = session.take_server_events();
    let client = session.take_client_events();
    let rules = session.rules();
    for e in &client {
        if let ClientSessionEvent::Welcomed(w) = e
            && let Some(mut n) = world.get_resource_mut::<crate::interp::NetInterpolation>()
        {
            n.clock
                .set_server_rate(f64::from(w.tick_hz), f64::from(w.send_every_ticks));
        }
        apply_client_event(world, e);
    }
    // Cloned only when there is something to apply (the hooks are a few function pointers; an
    // empty frame allocates nothing).
    let hooks = if server.is_empty() {
        NetPlayerHooks::default()
    } else {
        world
            .get_resource::<NetPlayerHooks>()
            .cloned()
            .unwrap_or_default()
    };
    let mut gone: Vec<PeerId> = Vec::new();
    for e in &server {
        if let ServerSessionEvent::PeerStateChanged {
            peer,
            to: PeerState::Gone,
            ..
        } = e
        {
            gone.push(*peer);
        }
        apply_server_event(world, &rules, &hooks, e);
    }
    // After every leave hook of the batch: the peer's players are gone, its entities move on.
    for peer in gone {
        hand_off_owned(world, &rules, peer);
    }
    refresh_player_stats(world);
    let mut log = world.get_resource_or_insert_with(NetSessionEvents::default);
    log.server = server;
    log.client = client;
}

/// Reads the mounted mods from the mod manager (`ModsVfs`) into the session when they changed, so
/// the next session start adds them to the manifest (spec 77 § 4.1).
fn sync_mounted_mods(world: &mut World) {
    let revision = world
        .get_resource::<regia_asset::mods_vfs::ModsVfs>()
        .map(|vfs| vfs.revision());
    let Some(session) = world.get_resource::<NetSession>() else {
        return;
    };
    if session.mods_revision == revision && (revision.is_some() || session.mounted_mods.is_none()) {
        return;
    }
    let mods = world
        .get_resource::<regia_asset::mods_vfs::ModsVfs>()
        .map(manifest_mods);
    if let Some(mut session) = world.get_resource_mut::<NetSession>() {
        session.mods_revision = revision;
        session.mounted_mods = mods;
    }
}

/// The mounted mods as manifest entries: id, the mod's version text (its own `luamod.ron`
/// `version` or what the mod manager set, empty when it declares none; ADR-7259), and the 64-bit
/// short form of the mod's content hash (BLAKE3 over its files, so changed files change it).
fn manifest_mods(vfs: &regia_asset::mods_vfs::ModsVfs) -> Vec<ManifestMod> {
    vfs.mods()
        .into_iter()
        .map(|m| ManifestMod {
            id: m.id,
            version: m.version,
            hash: m.content_hash.short(),
        })
        .collect()
}

/// Captures the join snapshots (spec 77 § 6.1) of the server's loading peers from the replication
/// state, for the core's join source to hand out when a peer's `LevelReady` arrives this step. A
/// peer is re-captured only when the replication state changed since its capture, so a quiet
/// frame costs no full pass over the replicated entities.
fn feed_join_snapshots(world: &mut World) {
    let Some(core) = world.get_resource::<NetSession>().and_then(NetSession::server) else {
        return;
    };
    let tick = core.tick();
    let loading: Vec<PeerId> = core
        .peer_ids()
        .into_iter()
        .filter(|p| core.peer_state(*p) == Some(PeerState::Loading))
        .collect();
    if loading.is_empty() {
        if let Some(session) = world.get_resource::<NetSession>() {
            let mut feed = lock(&session.join_feed);
            feed.tick = tick;
            feed.pending.clear();
        }
        return;
    }
    let stamp = join_stamp(world);
    // Peers without a capture (new to Loading) and peers whose capture predates a change.
    let dirty: Vec<PeerId> = {
        let session = world.resource::<NetSession>();
        let feed = lock(&session.join_feed);
        loading
            .iter()
            .copied()
            .filter(|p| feed.pending.get(p).is_none_or(|_| feed.stamp != stamp))
            .collect()
    };
    if !dirty.is_empty() {
        let captured = join_snapshots_for(world, &dirty, tick);
        if let Some(session) = world.get_resource::<NetSession>() {
            let mut feed = lock(&session.join_feed);
            feed.pending.extend(captured);
            feed.stamp = stamp.clone();
            feed.counts.captures += 1;
        }
    }
    if let Some(session) = world.get_resource::<NetSession>() {
        let mut feed = lock(&session.join_feed);
        feed.tick = tick;
        // Reused captures keep up with the session tick; their items are unchanged.
        for snapshot in feed.pending.values_mut() {
            snapshot.tick = tick;
        }
        feed.pending.retain(|p, _| loading.contains(p));
    }
}

/// Gives a travel the server started this frame its keep list (spec 77 § 6.2 step 3, ADR-7259):
/// read here, in the same step that sends the `TravelBegin` and despawns the old level, so the list
/// names exactly the entities the despawn keeps. Reads the world only while a travel waits.
fn feed_travel_keep(world: &mut World) {
    if !world
        .get_resource::<NetSession>()
        .and_then(NetSession::server)
        .is_some_and(SessionServer::has_pending_travel)
    {
        return;
    }
    let keep = travel_keep_for(world);
    if let Some(mut s) = world.get_resource_mut::<NetSession>()
        && let Some(core) = s.server_mut()
    {
        core.set_travel_keep(keep);
    }
}

/// Applies a client session event to the world: `LocalPeer` from the welcome (spec 70 § 4), the
/// join snapshot's removed level entities before `InGame` (spec 77 § 6.1) and its counters, and a
/// travel's early drop of the old level's copies (spec 77 § 6.2 step 3).
fn apply_client_event(world: &mut World, e: &ClientSessionEvent) {
    match e {
        ClientSessionEvent::Welcomed(w) => {
            world.insert_resource(crate::model::LocalPeer(Some(w.peer_id)));
            if let Some(mut s) = world.get_resource_mut::<NetSession>() {
                s.client_join = JoinSnapshotCounts::default();
                s.client_travel = TravelCounts::default();
            }
        }
        ClientSessionEvent::TravelStarted { keep: Some(keep), .. } => {
            let counts = drop_copies_for_travel(world, keep);
            if let Some(mut s) = world.get_resource_mut::<NetSession>() {
                s.client_travel = counts;
            }
        }
        ClientSessionEvent::JoinRemoved(guids) => {
            let applied = despawn_level_guids(world, guids);
            if let Some(mut s) = world.get_resource_mut::<NetSession>() {
                s.client_join.removed += guids.len() as u64;
                s.client_join.removed_applied += applied;
            }
        }
        ClientSessionEvent::JoinRecords(bytes) => {
            if JoinRecord::decode(bytes).is_some()
                && let Some(mut s) = world.get_resource_mut::<NetSession>()
            {
                s.client_join.records += 1;
            }
        }
        ClientSessionEvent::StateChanged {
            to: ClientSessionState::InGame,
            ..
        } => {
            if let Some(mut s) = world.get_resource_mut::<NetSession>() {
                s.client_join.snapshots += 1;
            }
        }
        ClientSessionEvent::Disconnected { .. } => {
            world.insert_resource(crate::model::LocalPeer(None));
        }
        _ => {}
    }
}

/// Despawns this client's level entities whose `EntityGuid` the server removed (join snapshot item
/// 1): they never show in the client's first `InGame` frame. Returns how many it found.
fn despawn_level_guids(world: &mut World, guids: &[u128]) -> u64 {
    if guids.is_empty() {
        return 0;
    }
    let wanted: std::collections::BTreeSet<u128> = guids.iter().copied().collect();
    let mut q = world.query::<(Entity, &regia_scene::components::EntityGuid)>();
    let found: Vec<Entity> = q
        .iter(world)
        .filter(|(_, g)| wanted.contains(&g.0.as_u128()))
        .map(|(e, _)| e)
        .collect();
    let mut applied = 0;
    for e in found {
        if world.get_entity(e).is_ok() {
            world.despawn(e);
            applied += 1;
        }
    }
    applied
}

/// Hands every entity a `Gone` peer owned to its successor or the server (spec 77 § 3.2, ADR-2413).
fn hand_off_owned(world: &mut World, rules: &Arc<Mutex<Box<dyn SessionRules>>>, peer: PeerId) {
    let owned = owned_by(world, peer);
    if owned.is_empty() {
        return;
    }
    let live_peers: Vec<PeerId> = world
        .get_resource::<NetSession>()
        .and_then(NetSession::server)
        .map(|core| {
            core.peer_ids()
                .into_iter()
                .filter(|p| core.peer_state(*p).is_some_and(|s| s != PeerState::Gone))
                .collect()
        })
        .unwrap_or_default();
    let live = |p: PeerId| live_peers.contains(&p);
    for entity in owned {
        let to = lock(rules).owned_hand_off(world, entity, peer, &live);
        // A rule may name a peer that is not in the session: the server takes the entity then.
        let to = match to {
            OwnedHandOff::Peer(p) if p == PeerId::SERVER || !live(p) => OwnedHandOff::Server,
            other => other,
        };
        apply_hand_off(world, entity, peer, to);
    }
}

/// Refreshes [`NetPlayerStats`] when its interval has passed.
fn refresh_player_stats(world: &mut World) {
    let Some(session) = world.get_resource::<NetSession>() else {
        return;
    };
    if matches!(session.role, Role::Idle) && !world.contains_resource::<NetPlayerStats>() {
        return;
    }
    let now = session.now_ms();
    if world
        .get_resource::<NetPlayerStats>()
        .is_some_and(|s| !s.due(now))
    {
        return;
    }
    let (rows, server) = session.player_link_stats();
    let mut stats = world.get_resource_or_insert_with(NetPlayerStats::default);
    stats.players.clear();
    stats.players.extend(rows);
    stats.server = server;
    stats.refreshed_ms = Some(now);
}

fn run_hooks(world: &mut World, hooks: &[PlayerHook], ent: Entity, player: NetPlayerId) {
    for hook in hooks {
        if world.get_entity(ent).is_err() {
            return;
        }
        hook(world, ent, player);
    }
}

fn player_entities(world: &World, peer: PeerId) -> Vec<(NetPlayerId, Entity)> {
    world
        .get_resource::<NetSession>()
        .map(|s| {
            s.players
                .iter()
                .filter(|(p, _)| p.peer == peer)
                .map(|(p, e)| (*p, *e))
                .collect()
        })
        .unwrap_or_default()
}

fn apply_server_event(
    world: &mut World,
    rules: &Arc<Mutex<Box<dyn SessionRules>>>,
    hooks: &NetPlayerHooks,
    e: &ServerSessionEvent,
) {
    match e {
        ServerSessionEvent::PeerInterrupted { peer } => {
            crate::model::mark_peer_interrupted(world, *peer, true)
        }
        ServerSessionEvent::PeerRestored { peer } => crate::model::mark_peer_interrupted(world, *peer, false),
        // A retired peer is no longer interrupted: its entities lose the marker before the leave
        // hooks and the hand-off of this batch see them (spec 120 § 10.2).
        ServerSessionEvent::PeerStateChanged {
            peer,
            to: PeerState::Gone,
            ..
        } => crate::model::mark_peer_interrupted(world, *peer, false),
        ServerSessionEvent::PlayerJoined { players, .. } => {
            if hooks.joined().is_empty() {
                return;
            }
            for p in players {
                let ent = world
                    .get_resource::<NetSession>()
                    .and_then(|s| s.players.get(p).copied());
                if let Some(ent) = ent {
                    run_hooks(world, hooks.joined(), ent, *p);
                }
            }
        }
        ServerSessionEvent::PeerWelcomed { players, .. } => {
            for p in players {
                let known = world
                    .get_resource::<NetSession>()
                    .and_then(|s| s.players.get(p).copied());
                if known.is_some_and(|ent| world.get_entity(ent).is_ok()) {
                    continue;
                }
                // Player entities persist across travel by default (spec 77 § 6.2).
                let ent = world.spawn((NetPlayer { id: *p }, NetPersistAcrossTravel)).id();
                if let Some(mut s) = world.get_resource_mut::<NetSession>() {
                    s.players.insert(*p, ent);
                }
                run_hooks(world, hooks.welcomed(), ent, *p);
            }
        }
        ServerSessionEvent::PlayerDropped { peer, .. } => {
            for (_, ent) in player_entities(world, *peer) {
                if let Ok(mut em) = world.get_entity_mut(ent) {
                    em.insert(NetDropped);
                }
                lock(rules).on_player_dropped(world, ent);
            }
        }
        ServerSessionEvent::PlayerResumed { peer } => {
            for (_, ent) in player_entities(world, *peer) {
                if let Ok(mut em) = world.get_entity_mut(ent) {
                    em.remove::<NetDropped>();
                }
                lock(rules).on_player_resumed(world, ent);
            }
        }
        ServerSessionEvent::PlayerLeft { player, reason } => {
            let ent = world
                .get_resource_mut::<NetSession>()
                .and_then(|mut s| s.players.remove(player));
            if let Some(ent) = ent {
                // A player that leaves (a split-screen kick too) is not interrupted any more.
                if let Ok(mut em) = world.get_entity_mut(ent) {
                    em.remove::<crate::model::NetInterrupted>();
                }
                lock(rules).on_player_left_reason(world, ent, reason);
                run_hooks(world, hooks.left(), ent, *player);
                if let Ok(em) = world.get_entity_mut(ent) {
                    em.despawn();
                }
            }
        }
        ServerSessionEvent::TravelStarted { .. } => {
            // Spec 77 § 6.2 step 2: everything replicated goes but the persistent entities.
            let counts = despawn_for_travel(world);
            let mut ids: Vec<u32> = {
                let mut q = world.query_filtered::<&crate::model::NetId, With<NetPersistAcrossTravel>>();
                q.iter(world).map(|id| id.0).collect()
            };
            ids.sort_unstable();
            ids.truncate(MAX_PERSISTENT_IDS);
            if let Some(mut s) = world.get_resource_mut::<NetSession>() {
                s.travel_counts = counts;
                s.persistent_ids = ids;
            }
        }
        ServerSessionEvent::TravelFinished { level } => lock(rules).on_travel_finished(world, level),
        _ => {}
    }
}

/// Adds [`NetSession`] (idle) and runs [`net_session_system`] in `PreUpdate` after
/// `NetSet::Receive`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NetSessionPlugin;

impl Plugin for NetSessionPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<NetSession>() {
            app.insert_resource(NetSession::default());
        }
        // The `session` section of a `NetSettings` inserted before this plugin (spec 77 § 12).
        if let Some(settings) = app
            .world()
            .get_resource::<crate::NetSettings>()
            .map(|s| s.session.clone())
        {
            app.world_mut()
                .resource_mut::<NetSession>()
                .set_settings(settings);
        }
        app.init_resource::<NetSessionEvents>()
            .init_resource::<NetPlayerHooks>()
            .init_resource::<NetHandOffs>()
            .init_resource::<NetPlayerStats>()
            .init_resource::<NetStatsOverlay>()
            .register_type::<crate::session::players::NetSuccessor>()
            .register_type::<crate::session::players::NetDespawnWithOwner>()
            .register_type::<NetPersistAcrossTravel>();
        app.add_systems(PreUpdate, net_session_system.after(NetSet::Receive));
        app.add_systems(Lifecycle, net_session_lifecycle);
    }

    fn finish(&self, app: &mut App) {
        // Every type the game registers is in by now: this is the process's protocol.
        let world = app.world_mut();
        let hash = crate::runtime::protocol_hash(world);
        // Dev builds name the differing types on a protocol mismatch (spec 77 § 4 item 2).
        let types = if cfg!(debug_assertions) {
            crate::runtime::protocol_types(world)
        } else {
            Vec::new()
        };
        let explicit = world
            .get_resource::<NetManifestFile>()
            .map(|file| file.0.clone())
            .or_else(|| regia_core::brand::env_var_os(NET_MANIFEST_ENV).map(PathBuf::from));
        let packaged = packaged_manifest(explicit, std::env::current_exe().ok());
        let user_dir = world
            .get_resource::<regia_platform::Vfs>()
            .and_then(|vfs| vfs.host_path(&regia_platform::VfsPath::root(regia_platform::MountPoint::User)));
        let user_store = user_dir
            .as_ref()
            .map(|dir| dir.join(crate::known_hosts::KNOWN_HOSTS_FILE));
        let listen_cap = world
            .get_resource::<crate::settings::NetSettings>()
            .map(|s| s.replication.android_listen_max_clients);
        if let Some(mut session) = world.get_resource_mut::<NetSession>() {
            // Clients pin server keys in the player's store unless the game chose (ADR-3283).
            if !session.known_hosts_set
                && let Some(store) = user_store
            {
                session.known_hosts = Some(store);
            }
            // A listen host keeps a persistent identity unless the game chose (ADR-4936).
            if !session.listen_identity_set
                && let Some(dir) = user_dir
            {
                session.listen_identity_path = Some(dir.join("listen-server.key"));
            }
            // Before any system runs, so hosting from `Startup` (before the first update)
            // enforces the configured cap, not the spec default.
            sync_android_listen_cap(&mut session, listen_cap);
            if let Some(manifest) = packaged {
                session.set_manifest(manifest);
            }
            // A manifest that names a hash (a test, a tool) keeps it.
            if session.manifest().protocol_hash == 0 {
                session.set_protocol_hash(hash);
            }
            if session.protocol_types.is_empty() {
                session.set_protocol_types(types);
            }
        }
        // Mods mounted before the first frame join a session hosted from `Startup` (spec 77 § 4.1).
        sync_mounted_mods(world);
    }
}

/// Where this process's `net-manifest.ron` is when it is not next to the binary: Android extracts
/// it from the APK. Read by [`NetSessionPlugin`] at `finish`.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct NetManifestFile(pub PathBuf);

/// Environment variable naming this process's `net-manifest.ron`, for a dev client started next to
/// a dev server (`regia server --clients`).
pub const NET_MANIFEST_ENV: &str = "REGIA_NET_MANIFEST";

/// The packaged manifest of spec 77 § 4.1: `explicit`, else `content/net-manifest.ron` next to `exe`.
/// `None` (a dev build) without one; an unreadable one is logged and ignored, and the handshake then
/// refuses with `content_mismatch` rather than this process pretending to match.
fn packaged_manifest(explicit: Option<PathBuf>, exe: Option<PathBuf>) -> Option<ContentManifest> {
    let path = explicit.filter(|p| !p.as_os_str().is_empty()).or_else(|| {
        let file = exe?.parent()?.join("content").join("net-manifest.ron");
        file.is_file().then_some(file)
    })?;
    match ContentManifest::load(&path) {
        Ok(manifest) => {
            tracing::info!(
                target: "regia_net",
                path = %path.display(),
                content_hash = manifest.content_hash,
                packs = manifest.packs.len(),
                "net manifest read"
            );
            Some(manifest)
        }
        Err(err) => {
            tracing::warn!(target: "regia_net", "{err}");
            None
        }
    }
}

/// `Lifecycle` system: an Android `suspended` says goodbye now (frames stop at the transition,
/// so a goodbye queued for `PreUpdate` would never leave) and a `resumed` reconnects with the
/// token while the server's grace slot lasts. Inert when the platform plugin is absent (unit
/// tests) or the session is not a client.
///
/// A desktop quit drives the app through `run_clean_exit`'s `[Suspended, WillTerminate]`: the
/// `Suspended` then carries [`regia_platform::CleanExitInProgress`], and the client says a
/// `client_quit` goodbye instead of a `suspended` one (the server used to hold a grace slot for
/// a player who had quit; corr-low, ADR-5789).
pub fn net_session_lifecycle(
    lifecycle: Option<Res<regia_platform::AppLifecycle>>,
    clean_exit: Option<Res<regia_platform::CleanExitInProgress>>,
    mut session: Option<ResMut<NetSession>>,
) {
    let _span = tracing::debug_span!("net_session_lifecycle").entered();
    let Some(lifecycle) = lifecycle else {
        return;
    };
    let Some(session) = session.as_deref_mut() else {
        return;
    };
    match lifecycle.last_event {
        Some(regia_platform::LifecycleEvent::Suspended)
            if clean_exit.is_some() && session.role_name() == "client" =>
        {
            session.disconnect();
        }
        Some(regia_platform::LifecycleEvent::Suspended) => session.suspend(),
        Some(regia_platform::LifecycleEvent::Resumed) => {
            if let Err(e) = session.resume() {
                tracing::warn!(error = %e, "net session: resume after suspend failed");
            }
        }
        _ => {}
    }
}

/// `app.set_session_rules(MyRules)` (spec 77 § 9).
pub trait SessionAppExt {
    /// Installs the game's session rules (inserting an idle [`NetSession`] if there is none).
    fn set_session_rules(&mut self, rules: impl SessionRules) -> &mut Self;
}

impl SessionAppExt for App {
    fn set_session_rules(&mut self, rules: impl SessionRules) -> &mut Self {
        let world = self.world_mut();
        if !world.contains_resource::<NetSession>() {
            world.insert_resource(NetSession::default());
        }
        world.resource_mut::<NetSession>().set_rules(rules);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `session` section of an inserted `NetSettings` reaches the session (spec 77 § 12).
    #[test]
    fn plugin_applies_net_settings_session_section() {
        let mut settings = crate::NetSettings::default();
        settings.session.timeout_s = 2.0;
        let mut app = App::new();
        app.insert_resource(settings);
        app.add_plugins(NetSessionPlugin);
        let session = app.world().resource::<NetSession>();
        assert!((session.settings().timeout_s - 2.0).abs() < f64::EPSILON);
    }

    /// Audit M4 (muse): a flood of game messages cannot evict a join or a leave from the queue.
    #[test]
    fn data_events_never_evict_lifecycle_events() {
        let peer = PeerId(7);
        let mut queue: Vec<ServerSessionEvent> = Vec::new();
        push_bounded(&mut queue, ServerSessionEvent::PlayerResumed { peer });
        for _ in 0..MAX_PENDING_SESSION_EVENTS * 2 {
            push_bounded(
                &mut queue,
                ServerSessionEvent::Game {
                    peer,
                    bytes: Vec::new(),
                },
            );
        }
        assert_eq!(queue.len(), MAX_PENDING_DATA_EVENTS);
        assert!(matches!(queue[0], ServerSessionEvent::PlayerResumed { .. }));
        // Lifecycle events still fit, up to the whole queue, and only then evict.
        for _ in 0..MAX_PENDING_SESSION_EVENTS {
            push_bounded(&mut queue, ServerSessionEvent::PlayerResumed { peer });
        }
        assert_eq!(queue.len(), MAX_PENDING_SESSION_EVENTS);
        let mut client: Vec<ClientSessionEvent> = Vec::new();
        for _ in 0..MAX_PENDING_SESSION_EVENTS * 2 {
            push_bounded(&mut client, ClientSessionEvent::Game(Vec::new()));
        }
        assert_eq!(client.len(), MAX_PENDING_DATA_EVENTS);
    }

    /// Sec review (ADR-5870 § 5): a joined peer's zero-byte `StreamBegin` completes at once,
    /// holds no stream slot and may reuse its id, and `Complete` counted as a lifecycle event, so
    /// a flood of them evicted other peers' joins and leaves from the queue.
    #[test]
    fn completed_streams_never_evict_lifecycle_events() {
        let peer = PeerId(7);
        let mut queue: Vec<ServerSessionEvent> = Vec::new();
        push_bounded(&mut queue, ServerSessionEvent::PlayerResumed { peer });
        for i in 0..MAX_PENDING_SESSION_EVENTS * 2 {
            push_bounded(
                &mut queue,
                ServerSessionEvent::Stream {
                    peer,
                    event: StreamEvent::Complete {
                        id: crate::session::StreamId(i as u32),
                        kind: String::new(),
                        bytes: Vec::new(),
                    },
                },
            );
        }
        assert!(matches!(queue[0], ServerSessionEvent::PlayerResumed { .. }));
        assert_eq!(queue.len(), MAX_PENDING_DATA_EVENTS);
        let mut client: Vec<ClientSessionEvent> = Vec::new();
        push_bounded(&mut client, ClientSessionEvent::JoinRemoved(Vec::new()));
        for i in 0..MAX_PENDING_SESSION_EVENTS * 2 {
            push_bounded(
                &mut client,
                ClientSessionEvent::Stream(StreamEvent::Complete {
                    id: crate::session::StreamId(i as u32),
                    kind: String::new(),
                    bytes: Vec::new(),
                }),
            );
        }
        assert!(matches!(client[0], ClientSessionEvent::JoinRemoved(_)));
    }
    use crate::socket::LoopbackNet;

    #[test]
    fn a_packaged_manifest_sits_next_to_the_binary() {
        let dir = std::env::temp_dir().join(format!("regia_net_pkg_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("content")).expect("dir");
        let exe = dir.join("game");
        assert!(
            packaged_manifest(None, Some(exe.clone())).is_none(),
            "a dev build has none"
        );
        let written = ContentManifest::build("1", "2", 1, 0, &[("base", &[])]);
        std::fs::write(
            dir.join("content/net-manifest.ron"),
            written.to_ron().expect("ron"),
        )
        .expect("write");
        assert_eq!(packaged_manifest(None, Some(exe.clone())), Some(written.clone()));
        let other = dir.join("other.ron");
        std::fs::write(&other, "NetManifest(format_version: 99)").expect("write");
        assert!(
            packaged_manifest(Some(other), Some(exe)).is_none(),
            "a bad explicit file does not fall back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_plugin_stamps_the_registered_protocol_on_the_manifest() {
        let mut app = App::new();
        app.add_plugins((crate::plugin::NetPlugin::default(), NetSessionPlugin));
        app.finish_plugins();
        let expected = crate::runtime::protocol_hash(app.world());
        assert_ne!(expected, 0);
        assert_eq!(
            app.world().resource::<NetSession>().manifest().protocol_hash,
            expected
        );
    }

    /// Muse fix-review (fix-net3-platform, finding 1): the Android listen cap is synced at
    /// `NetSessionPlugin::finish`, before any system runs, so a `Startup`-time host — before the
    /// first `net_session_system` — enforces the configured value instead of the spec default.
    #[test]
    fn finish_syncs_the_android_listen_cap_before_any_update() {
        let mut app = App::new();
        app.add_plugins((crate::plugin::NetPlugin::default(), NetSessionPlugin));
        let mut settings = crate::settings::NetSettings::default();
        settings.replication.android_listen_max_clients = 2;
        app.insert_resource(settings);
        app.finish_plugins();
        assert_eq!(
            app.world().resource::<NetSession>().android_listen_max_clients,
            2,
            "a host before the first update must see the configured cap, not the default"
        );
    }

    #[derive(Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl SessionRules for Recorder {
        fn on_player_dropped(&mut self, world: &mut World, player: Entity) {
            let dropped = world.get::<NetDropped>(player).is_some();
            lock(&self.0).push(format!("dropped:{dropped}"));
        }
        fn on_player_resumed(&mut self, _world: &mut World, _player: Entity) {
            lock(&self.0).push("resumed".into());
        }
        fn on_player_left_reason(&mut self, _world: &mut World, _player: Entity, reason: &DisconnectReason) {
            lock(&self.0).push(format!("left:{}", reason.code()));
        }
        fn on_travel_finished(&mut self, _world: &mut World, level: &str) {
            lock(&self.0).push(format!("travel:{level}"));
        }
    }

    fn frames(app: &mut App, n: usize) {
        for _ in 0..n {
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            app.update();
        }
    }

    /// The plugin spawns a `NetPlayer` entity at `Welcome`, marks it `NetDropped` during the grace,
    /// clears it on resume, runs every hook, and despawns the entity at `Gone`.
    #[test]
    fn plugin_player_entities_and_rules_hooks() {
        let mut app = App::new();
        app.add_plugins(NetSessionPlugin);
        let log = Arc::new(Mutex::new(Vec::new()));
        app.set_session_rules(Recorder(log.clone()));
        {
            let mut s = app.world_mut().resource_mut::<NetSession>();
            s.set_manual_clock(0);
            s.host_local(
                HostOptions {
                    level: "levels/a.level.ron".into(),
                    ..HostOptions::default()
                },
                true,
            )
            .expect("host");
            s.add_simulated_clients(1, SimInput::Idle).expect("sim");
        }
        frames(&mut app, 30);
        let player = NetPlayerId {
            peer: PeerId(1),
            local: 0,
        };
        let ent = app
            .world()
            .resource::<NetSession>()
            .player_entity(player)
            .expect("entity");
        assert_eq!(app.world().get::<NetPlayer>(ent).map(|p| p.id), Some(player));
        // Suspend: grace, NetDropped, hook.
        {
            let mut s = app.world_mut().resource_mut::<NetSession>();
            let now = s.now_ms();
            if let Some(sims) = s.simulated_mut() {
                sims.client_mut(0)
                    .core
                    .disconnect(DisconnectReason::Suspended, now);
            }
        }
        frames(&mut app, 10);
        assert!(app.world().get::<NetDropped>(ent).is_some());
        // Resume with the token.
        {
            let mut s = app.world_mut().resource_mut::<NetSession>();
            let now = s.now_ms();
            if let Some(sims) = s.simulated_mut() {
                sims.client_mut(0).reconnect(now, 1);
            }
        }
        frames(&mut app, 30);
        assert!(app.world().get::<NetDropped>(ent).is_none());
        assert_eq!(
            app.world().resource::<NetSession>().player_entity(player),
            Some(ent)
        );
        // Travel, then a kick: the entity goes away.
        app.world_mut()
            .resource_mut::<NetSession>()
            .travel("levels/b.level.ron")
            .expect("travel");
        frames(&mut app, 30);
        app.world_mut()
            .resource_mut::<NetSession>()
            .kick(player, "bye")
            .expect("kick");
        frames(&mut app, 5);
        assert!(app.world().get_entity(ent).is_err(), "despawned at Gone");
        assert_eq!(
            *lock(&log),
            vec![
                "dropped:true".to_owned(),
                "resumed".to_owned(),
                "travel:levels/b.level.ron".to_owned(),
                "left:kicked".to_owned()
            ]
        );
    }

    /// corr-low: a desktop quit runs `run_clean_exit`'s `[Suspended, WillTerminate]`. The
    /// `Suspended` carries `CleanExitInProgress`; without the check the client said a `suspended`
    /// goodbye and the server held a grace slot (and reported `suspended`) for a player who had
    /// quit. With the marker the reason is `client_quit`.
    #[test]
    fn a_clean_exit_suspend_is_a_quit_not_a_suspend() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7779".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(
                net.bind(saddr).expect("bind"),
                HostOptions {
                    level: "levels/a.level.ron".into(),
                    ..HostOptions::default()
                },
                true,
            )
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        let mut app = App::new();
        app.add_plugins(NetSessionPlugin);
        app.insert_resource(client);
        let mut in_game = false;
        for _ in 0..200 {
            server.advance_manual_clock(16);
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            app.update();
            server.update();
            if app.world().resource::<NetSession>().state() == ClientSessionState::InGame {
                in_game = true;
                break;
            }
        }
        assert!(in_game, "the client joins");
        // The clean-exit sequence marks the world, then suspends.
        app.world_mut()
            .insert_resource(regia_platform::CleanExitInProgress);
        app.world_mut().insert_resource(regia_platform::AppLifecycle {
            last_event: Some(regia_platform::LifecycleEvent::Suspended),
            ..Default::default()
        });
        app.world_mut().run_schedule(Lifecycle);
        for _ in 0..20 {
            server.advance_manual_clock(16);
            app.world_mut()
                .resource_mut::<NetSession>()
                .advance_manual_clock(16);
            app.update();
            server.update();
        }
        let left = server.take_server_events().into_iter().find_map(|e| match e {
            ServerSessionEvent::PlayerLeft { reason, .. } => Some(reason.code()),
            _ => None,
        });
        assert_eq!(
            left,
            Some("client_quit"),
            "the server reads a quit, not a suspend grace slot"
        );
    }

    #[test]
    fn transport_reasons_map_to_session_codes() {
        assert_eq!(map_transport(TransportReason::Flood).code(), "flood");
        assert_eq!(
            map_transport(TransportReason::ReliableOverflow).code(),
            "reliable_overflow"
        );
        assert_eq!(
            map_transport(TransportReason::MalformedPacket).code(),
            "malformed_packet"
        );
        assert_eq!(map_transport(TransportReason::Timeout).code(), "timeout");
        assert_eq!(map_transport(TransportReason::Left).code(), "client_quit");
        assert_eq!(map_transport(TransportReason::Kicked).code(), "transport_error");
    }

    #[test]
    fn server_and_client_over_the_datagram_network() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7777".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(
                net.bind(saddr).expect("bind"),
                HostOptions {
                    level: "levels/a.level.ron".into(),
                    ..HostOptions::default()
                },
                true,
            )
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        let joined = server
            .take_server_events()
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerJoined { .. }));
        assert!(joined);
        assert_eq!(
            server.host_local(HostOptions::default(), false),
            Err(SessionError::AlreadyInSession)
        );
    }

    /// Audit fixrev-15-k net3-determinism M2 (ADR-5444): `NetSession` captured its unix base
    /// from the wall clock at construction with no way to supply the recorded value a replay
    /// holds, so a ban whose `until` sat near the recording's base was judged against the
    /// construction wall time instead. `set_unix_base_s` makes a manual-clock session expire a
    /// ban at the recorded times, identically on every host.
    #[test]
    fn a_manual_clock_session_expires_a_ban_at_the_recorded_base() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7781".parse().expect("addr");
        let base = 1_000_000u64;
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server.set_unix_base_s(base);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        server
            .ban(
                crate::session::bans::BanKind::Address,
                "127.0.0.1",
                Some(base + 60),
                "spam",
            )
            .expect("ban");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..29 {
            server.advance_manual_clock(1000);
            client.advance_manual_clock(1000);
            client.update();
            server.update();
        }
        assert_ne!(
            client.state(),
            ClientSessionState::InGame,
            "the ban holds until its recorded expiry"
        );
        // The refused client has given up; after the expiry a fresh client joins.
        for _ in 0..200 {
            server.advance_manual_clock(1000);
            client.update();
            server.update();
        }
        let mut late = NetSession::default();
        late.set_manual_clock(0);
        late.connect_on(
            net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
            saddr,
            ConnectOptions::default(),
        )
        .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(1000);
            late.advance_manual_clock(1000);
            late.update();
            server.update();
            if late.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(
            late.state(),
            ClientSessionState::InGame,
            "a fresh client joins once the recorded expiry passed"
        );
    }

    /// Audit net1 (platform): an Android `suspended` must put the goodbye on the socket before
    /// frames stop. Until now nothing called `suspend`, and a suspend hook that only queued the
    /// goodbye would never send it: the session writes from `PreUpdate`, which no longer runs.
    #[test]
    fn suspend_flushes_goodbye_before_frames_stop() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7778".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        let welcomed = client.take_client_events().iter().find_map(|e| match e {
            ClientSessionEvent::Welcomed(w) => Some((w.peer_id, w.reconnect_token)),
            _ => None,
        });
        let (peer, _token) = welcomed.expect("the welcome named the peer and carried a token");
        assert_eq!(server.peer_state(peer), Some(PeerState::InGame));

        client.suspend();

        // No client update after this point: the goodbye must already be on the socket. One
        // server step receives it; the server holds the grace slot instead of retiring the peer.
        server.advance_manual_clock(16);
        server.update();
        let dropped = server.take_server_events().iter().any(|e| {
            matches!(e, ServerSessionEvent::PlayerDropped { peer: p, reason } if *p == peer && reason.code() == "suspended")
        });
        assert!(dropped, "the server saw the goodbye");
        assert_eq!(server.peer_state(peer), Some(PeerState::Grace));
    }

    /// Audit net1 (platform): after the flushed `suspended` goodbye, `resume` reconnects with
    /// the welcome token and the server reports `PlayerResumed` with the same `PeerId` while the
    /// grace slot lasts.
    #[test]
    fn resume_after_suspend_keeps_the_peer_id() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7779".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        let welcomed = client.take_client_events().iter().find_map(|e| match e {
            ClientSessionEvent::Welcomed(w) => Some((w.peer_id, w.reconnect_token)),
            _ => None,
        });
        let (peer, _token) = welcomed.expect("the welcome named the peer and carried a token");
        client.suspend();
        server.advance_manual_clock(16);
        server.update();
        assert_eq!(server.peer_state(peer), Some(PeerState::Grace));

        // The process survived the pause: the resume presents the token.
        assert!(
            client
                .resume_on(net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"))
                .expect("resume"),
            "a reconnect began"
        );
        for _ in 0..300 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        // The resumed session welcomes us again with the same peer.
        let resumed_welcome = client.take_client_events().iter().find_map(|e| match e {
            ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
            _ => None,
        });
        assert_eq!(resumed_welcome, Some(peer), "the grace slot resumed");
        let resumed = server
            .take_server_events()
            .iter()
            .any(|e| matches!(e, ServerSessionEvent::PlayerResumed { peer: p } if *p == peer));
        assert!(resumed, "the server reported PlayerResumed");
    }

    /// Finding 4 (deep-zai net-lobby): `resume` rebinds a plain UDP socket and dials the last
    /// connect's address. A session opened with `connect_on` (a Steam lobby socket with a
    /// synthetic `fd52:...` address, a loopback test socket) cannot work that way — the datagrams
    /// go nowhere and the failure is only a silent connect timeout. `resume` must refuse with a
    /// reason and keep the session suspended so the caller reconnects with `resume_on`.
    #[test]
    fn resume_refuses_a_custom_socket_session() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7782".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        client.suspend();
        server.advance_manual_clock(16);
        server.update();

        let err = client
            .resume()
            .expect_err("a custom-socket session must not auto-resume over plain UDP");
        assert_eq!(err.code(), "transport_error");
        // The refusal keeps the session suspended: the caller's recovery is `resume_on`.
        assert!(
            client
                .resume_on(net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"))
                .expect("resume_on reconnects"),
            "a reconnect began"
        );
        for _ in 0..300 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(
            client.state(),
            ClientSessionState::InGame,
            "resume_on recovered it"
        );
    }

    /// Audit net1 (platform): `suspend` must drive every goodbye copy out, not one — the copies
    /// exist because a single unreliable datagram can be lost on a phone network, and no frame
    /// runs after this to send the rest.
    #[test]
    fn suspend_flushes_every_goodbye_copy() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7780".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        // Quiesce both ends so the counter delta isolates the goodbye copies.
        for _ in 0..5 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
        }
        let (delivered, _) = net.counters();
        client.suspend();
        let (after, _) = net.counters();
        assert_eq!(
            after - delivered,
            u64::from(crate::session::lifecycle::GOODBYE_COPIES),
            "every goodbye copy left before frames stop"
        );
    }

    /// Audit net1: `disconnect` after `suspend` cancels the pending auto-resume — the game quit,
    /// so `resume` must not reconnect into a session the player left.
    #[test]
    fn disconnect_after_suspend_cancels_the_auto_resume() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7782".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        client.suspend();
        client.disconnect();
        assert!(
            !client
                .resume_on(net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"))
                .expect("resume"),
            "a quit session must not auto-resume"
        );
    }

    /// Audit net1: a second `suspend` while the first goodbye is in flight keeps the pending
    /// auto-resume instead of cancelling it.
    #[test]
    fn second_suspend_keeps_the_pending_resume() {
        let net = LoopbackNet::new();
        let saddr: SocketAddr = "127.0.0.1:7783".parse().expect("addr");
        let mut server = NetSession::default();
        server.set_manual_clock(0);
        server
            .host_on(net.bind(saddr).expect("bind"), HostOptions::default(), false)
            .expect("host");
        let mut client = NetSession::default();
        client.set_manual_clock(0);
        client
            .connect_on(
                net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"),
                saddr,
                ConnectOptions::default(),
            )
            .expect("connect");
        for _ in 0..200 {
            server.advance_manual_clock(16);
            client.advance_manual_clock(16);
            client.update();
            server.update();
            if client.state() == ClientSessionState::InGame {
                break;
            }
        }
        assert_eq!(client.state(), ClientSessionState::InGame);
        let welcomed = client.take_client_events().iter().find_map(|e| match e {
            ClientSessionEvent::Welcomed(w) => Some(w.peer_id),
            _ => None,
        });
        let peer = welcomed.expect("the welcome named the peer");
        client.suspend();
        client.suspend();
        server.advance_manual_clock(16);
        server.update();
        assert_eq!(server.peer_state(peer), Some(PeerState::Grace));
        assert!(
            client
                .resume_on(net.bind("127.0.0.1:0".parse().expect("addr")).expect("bind"))
                .expect("resume"),
            "the second suspend kept the pending auto-resume"
        );
    }
}
