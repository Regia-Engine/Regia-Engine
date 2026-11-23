// SPDX-License-Identifier: MPL-2.0
//! The session state machines (spec 77 § 3-§ 9): sans-IO cores for the server and the client.
//!
//! The cores never touch a socket or a clock. A driver (the `NetSession`
//! resource, the loopback session hub, or a test) feeds them transport events with the current
//! time in milliseconds and moves their outgoing messages. Every transition happens inside one of
//! those calls, so with simulated time the whole lifecycle replays identically (§ 13).
//!
//! Transport links are [`LinkId`]s; session peers are [`PeerId`]s. A reconnect within the grace
//! period arrives on a new link and resumes the old `PeerId` (§ 7.4).

use std::collections::{BTreeMap, VecDeque};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::addr_block::AddrBlock;
use crate::connection::Channel;
use crate::model::{NetPlayerId, PeerId};
use crate::retry_tokens::UnconfirmedTokens;
use crate::session::bans::{BanStore, BanSubject};
use crate::session::manifest::ContentManifest;
use crate::session::proto::{DisconnectReason, Hello, RECONNECT_TOKEN_BYTES, SessionMsg, Welcome};
use crate::session::streams::{StreamEvent, StreamId, StreamLink, StreamRefusal};
use crate::ticket::{JoinTicketCheck, TicketRefused, TicketVerdict};
use crate::version_probe::{MismatchSide, VersionStamp};
use crate::wire::constant_time_eq;

/// Keepalive interval: either side sends one when it sent nothing for this long.
pub const KEEPALIVE_MS: u64 = 1_000;
/// Wrong session passwords and remote admin tokens one address block (one IPv4 address or one
/// IPv6 /64) may send before it is slowed down (ADR-5870). From this strike on, each wrong try
/// makes the block wait [`auth_backoff_ms`] before its next try is checked; a try inside the wait
/// is refused unchecked and not counted. The wait doubles per wrong try and stops at
/// [`AUTH_BACKOFF_MAX_MS`], so a household behind one NAT address is slowed, never locked out
/// (ADR-5993; before it the block was refused for the rest of a 60 s window).
pub const MAX_AUTH_STRIKES: u32 = 10;
/// A block's strikes are forgotten this long after its last wrong try.
pub const AUTH_STRIKE_WINDOW_MS: u64 = 60_000;
/// The wait after the strike that reaches [`MAX_AUTH_STRIKES`] (ADR-5993).
pub const AUTH_BACKOFF_BASE_MS: u64 = 4_000;
/// The longest wait between two checked tries of one address block (ADR-5993).
pub const AUTH_BACKOFF_MAX_MS: u64 = 30_000;

/// The wait, in ms, after a block's `strikes`-th wrong password or admin token: none below
/// [`MAX_AUTH_STRIKES`], then [`AUTH_BACKOFF_BASE_MS`] doubling per further strike up to
/// [`AUTH_BACKOFF_MAX_MS`].
#[must_use]
pub fn auth_backoff_ms(strikes: u32) -> u64 {
    match strikes.checked_sub(MAX_AUTH_STRIKES) {
        None => 0,
        Some(past) => AUTH_BACKOFF_BASE_MS
            .saturating_mul(1_u64 << past.min(16))
            .min(AUTH_BACKOFF_MAX_MS),
    }
}

/// One address block's wrong passwords and admin tokens.
#[derive(Debug, Clone, Copy)]
struct AuthStrikes {
    /// Wrong tries counted since the block was last forgotten.
    count: u32,
    /// When the last one came, ms.
    last_ms: u64,
    /// No try is checked before this, ms.
    wait_until_ms: u64,
}
/// Address blocks whose strikes are remembered at once; when full, the block with the fewest
/// strikes is forgotten.
const MAX_AUTH_STRIKE_BLOCKS: usize = 4096;
/// Copies of a client's goodbye (one per tick).
pub const GOODBYE_COPIES: u8 = 3;
/// Updates between a server-side goodbye and closing the link (lets the goodbye leave first).
pub const CLOSE_AFTER_UPDATES: u8 = 2;

/// The `session` section of `config/net.ron` (spec 77 § 12). Unfrozen until v0.5.0: every field has
/// a serde default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionSettings {
    /// Legacy silence timeout in seconds. Read from a `config/net.ron` only when the file has no
    /// `timeout_ms`; in code it applies while `timeout_ms` keeps its default. Never written.
    #[serde(skip_serializing)]
    pub timeout_s: f64,
    /// Silence timeout in milliseconds. A file that names it wins over `timeout_s`; in code a
    /// nondefault value wins.
    pub timeout_ms: u64,
    /// Silence before interruption, milliseconds.
    pub interrupt_ms: u64,
    /// Admission grace for a peer that has not sent game data, milliseconds.
    pub start_grace_ms: u64,
    /// Transport connect timeout, seconds.
    pub connect_timeout_s: f64,
    /// `Handshaking` timeout, seconds.
    pub handshake_timeout_s: f64,
    /// `Loading` timeout (`level_load_failed`), seconds.
    pub loading_timeout_s: f64,
    /// `Joining` timeout, seconds.
    pub joining_timeout_s: f64,
    /// How long a pending authenticator may take (`auth_failed`), seconds.
    pub auth_timeout_s: f64,
    /// Most peers in `Handshaking` at once; further transports are closed.
    pub max_handshaking: u32,
    /// Player slots only usable by clients the authenticator marks `reserved`.
    pub reserved_slots: u32,
    /// Reconnect grace after an unexpected drop, seconds (0 disables).
    pub reconnect_grace_s: f64,
    /// Share of the connection budget streams may use.
    pub stream_share: f64,
    /// Incoming streams fail when they move less than 256 bytes per second over this long, seconds
    /// (0 = never).
    pub stream_idle_timeout_s: f64,
    /// Server: bytes of incoming streams held across all peers (0 = no limit). A chunk past it fails
    /// the streams of the peer that sent it.
    pub max_stream_buffer_bytes: u64,
    /// Dev builds only: a content mismatch is a warning, not a refusal.
    pub dev_allow_content_mismatch: bool,
    /// When the datagram transport is encrypted (ADR-1901). The default requires it for every
    /// non-loopback peer.
    pub encryption: crate::crypto::CryptoMode,
    /// The transport's rate controller, pacer and shared uplink (spec 120 § 4.2, format v4,
    /// ADR-8301).
    pub rate_control: crate::cc::RateSettings,
}

impl Default for SessionSettings {
    fn default() -> Self {
        let timeout = super::TimeoutPolicy::default();
        Self {
            timeout_s: timeout.timeout_ms as f64 / 1000.0,
            timeout_ms: timeout.timeout_ms,
            interrupt_ms: timeout.interrupt_ms,
            start_grace_ms: timeout.start_grace_ms,
            connect_timeout_s: 10.0,
            handshake_timeout_s: 10.0,
            loading_timeout_s: 120.0,
            joining_timeout_s: 30.0,
            auth_timeout_s: 10.0,
            max_handshaking: 32,
            reserved_slots: 0,
            reconnect_grace_s: 60.0,
            stream_share: 0.25,
            stream_idle_timeout_s: 30.0,
            max_stream_buffer_bytes: 512 * 1024 * 1024,
            dev_allow_content_mismatch: false,
            encryption: crate::crypto::CryptoMode::default(),
            rate_control: crate::cc::RateSettings::default(),
        }
    }
}

impl SessionSettings {
    /// Resolves millisecond settings and the legacy seconds override once for all layers, with
    /// every value in range ([`super::TimeoutPolicy::sanitized`]).
    pub fn timeout_policy(&self) -> super::TimeoutPolicy {
        super::TimeoutPolicy {
            timeout_ms: if self.timeout_ms == super::TimeoutPolicy::default().timeout_ms {
                ms(self.timeout_s)
            } else {
                self.timeout_ms
            },
            interrupt_ms: self.interrupt_ms,
            start_grace_ms: self.start_grace_ms,
        }
        .sanitized()
    }

    /// After `config/net.ron` was read: a `timeout_ms` the file names wins over legacy
    /// `timeout_s`, and the seconds count only when the file has no `timeout_ms`. Leaves both
    /// fields in agreement, so [`Self::timeout_policy`] gives the file's value either way.
    pub(crate) fn resolve_loaded_timeout(&mut self, file_names_timeout_ms: bool) {
        if file_names_timeout_ms {
            self.timeout_s = self.timeout_ms as f64 / 1000.0;
        } else if self.timeout_ms == super::TimeoutPolicy::default().timeout_ms {
            self.timeout_ms = ms(self.timeout_s);
        }
    }
}

fn ms(seconds: f64) -> u64 {
    (seconds.max(0.0) * 1000.0).round() as u64
}

/// A transport link (one connection as the transport knows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LinkId(pub u32);

/// A peer address as the authenticator and the ban check see it.
pub type PeerAddr = SocketAddr;

/// The client's lifecycle state (spec 77 § 3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientSessionState {
    /// Not connected; the reason of the last end, if any.
    Disconnected(Option<DisconnectReason>),
    /// Transport connecting.
    Connecting,
    /// `Hello` sent; waiting for `Welcome` or a refusal.
    Handshaking,
    /// Loading the level named by `Welcome` or `TravelBegin`.
    Loading,
    /// Level loaded; receiving the join snapshot.
    Joining,
    /// Playing.
    InGame,
    /// Sending goodbyes before `Disconnected`.
    Disconnecting,
}

impl ClientSessionState {
    /// The lowercase state name.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disconnected(_) => "disconnected",
            Self::Connecting => "connecting",
            Self::Handshaking => "handshaking",
            Self::Loading => "loading",
            Self::Joining => "joining",
            Self::InGame => "in_game",
            Self::Disconnecting => "disconnecting",
        }
    }
}

/// A peer's lifecycle state on the server (spec 77 § 3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeerState {
    /// Transport up, no valid `Hello` yet.
    Handshaking,
    /// `Welcome` sent; the client is loading.
    Loading,
    /// `LevelReady` received; join snapshot sent.
    Joining,
    /// Playing.
    InGame,
    /// Dropped unexpectedly; slot reserved for a reconnect.
    Grace,
    /// Left, kicked, timed out or grace expired.
    Gone,
}

impl PeerState {
    /// The lowercase state name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Handshaking => "handshaking",
            Self::Loading => "loading",
            Self::Joining => "joining",
            Self::InGame => "in_game",
            Self::Grace => "grace",
            Self::Gone => "gone",
        }
    }
}

/// The id of one authenticator request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthRequestId(pub u64);

/// An authenticator's answer (spec 77 § 5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthResult {
    /// Accepted.
    Accept {
        /// The player's platform id (used by platform-id bans).
        platform_id: Option<String>,
        /// May use reserved slots.
        reserved: bool,
    },
    /// Refused with a text (sent as `auth_failed`).
    Reject(&'static str),
    /// Not decided yet: the session polls once per tick.
    Pending,
}

/// A pluggable authenticator (one per session; store platforms plug in here).
pub trait SessionAuthenticator: Send + Sync + 'static {
    /// First look at a `Hello`.
    fn check(&mut self, id: AuthRequestId, hello: &Hello, peer_addr: &PeerAddr) -> AuthResult;
    /// Polled once per tick while a request is pending; `None` = still pending.
    fn poll(&mut self, id: AuthRequestId) -> Option<AuthResult> {
        let _ = id;
        None
    }
}

/// A game's verdict on a joining player, after the engine's checks (spec 77 § 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinDecision {
    /// Let it in.
    Accept,
    /// Refuse with a text (sent as `kicked`).
    Refuse(String),
}

/// What the server sends a joining peer (spec 77 § 6.1). Supplied by the replication layer
/// through [`SessionServer::set_join_source`]; the session layer only orders and frames it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JoinSnapshot {
    /// Item 1: `EntityGuid`s of level entities removed since the level loaded.
    pub removed: Vec<u128>,
    /// Item 2: spawn records of relevant-everywhere entities and the peer's own (opaque).
    pub records: Vec<Vec<u8>>,
    /// The server tick at which items 1 and 2 were captured.
    pub tick: u64,
}

/// The game's join hook (`SessionRules::on_player_joining`).
pub type JoinRule = Box<dyn FnMut(&Hello) -> JoinDecision + Send + Sync>;

/// The join snapshot provider.
pub type JoinSource = Box<dyn FnMut(PeerId) -> JoinSnapshot + Send + Sync>;

/// Something that happened on the server (spec 77 § 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerSessionEvent {
    /// A peer is interrupted but retains its slot and lifecycle phase.
    PeerInterrupted {
        /// The peer.
        peer: PeerId,
    },
    /// Traffic returned to an interrupted peer.
    PeerRestored {
        /// The peer.
        peer: PeerId,
    },
    /// A peer's state changed.
    PeerStateChanged {
        /// The peer.
        peer: PeerId,
        /// Old state.
        from: PeerState,
        /// New state.
        to: PeerState,
    },
    /// A peer was welcomed (its player controllers exist from now on).
    PeerWelcomed {
        /// The peer.
        peer: PeerId,
        /// Its players.
        players: Vec<NetPlayerId>,
        /// Their names.
        names: Vec<String>,
    },
    /// A remote peer reached `InGame`.
    PlayerJoined {
        /// The peer.
        peer: PeerId,
        /// Its players.
        players: Vec<NetPlayerId>,
    },
    /// A player left (`Gone`, or one player kicked from a split-screen peer).
    PlayerLeft {
        /// The player.
        player: NetPlayerId,
        /// Why.
        reason: DisconnectReason,
    },
    /// A peer dropped unexpectedly and entered the grace period.
    PlayerDropped {
        /// The peer.
        peer: PeerId,
        /// Why it dropped.
        reason: DisconnectReason,
    },
    /// A peer resumed its slot with its reconnect token: from the grace period, or by taking
    /// over its own live slot from a new link (spec 120 § 10.3; the old link is closed without
    /// a goodbye).
    PlayerResumed {
        /// The peer.
        peer: PeerId,
    },
    /// A `Hello` carried a reconnect token that matches no held slot (expired, already used, or
    /// forged). The join goes on as a new player; the host may log it or refuse the link.
    TokenRefused {
        /// The link that presented it.
        link: LinkId,
    },
    /// A connection was refused at a handshake check.
    Refused {
        /// The link.
        link: LinkId,
        /// Why.
        reason: DisconnectReason,
    },
    /// Travel began.
    TravelStarted {
        /// The new level.
        level: String,
        /// The travel id.
        travel_id: u32,
    },
    /// The server finished loading the travel's level.
    TravelFinished {
        /// The level.
        level: String,
    },
    /// A stream event on a peer's connection.
    Stream {
        /// The peer.
        peer: PeerId,
        /// The event.
        event: StreamEvent,
    },
    /// A game session message.
    Game {
        /// From.
        peer: PeerId,
        /// Payload.
        bytes: Vec<u8>,
    },
}

/// One outgoing transport message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The link.
    pub link: LinkId,
    /// Reliable or unreliable.
    pub channel: Channel,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// Server-side session options (`server.toml` and `session.host`).
#[derive(Debug, Clone, PartialEq)]
pub struct HostOptions {
    /// Server name.
    pub name: String,
    /// Message of the day.
    pub motd: String,
    /// Player capacity (players, not peers).
    pub max_players: u32,
    /// Empty = no password.
    pub password: String,
    /// Simulation rate.
    pub tick_hz: u32,
    /// Send cadence in ticks.
    pub send_every_ticks: u32,
    /// Starting level.
    pub level: String,
    /// The server's per-client downstream rate, bytes per second (spec 72 § 7).
    pub client_rate_bytes: u32,
    /// Entropy coding of replication state frames (spec 72 § 18, ADR-3315): `None` reads the
    /// project's `config/net.ron`; `Some` overrides it (a dedicated server's `server.toml`).
    /// Negotiated per connection at the first `ACK`.
    pub entropy: Option<crate::entropy::EntropyMode>,
    /// Remote admin token (`None` = remote admin off).
    pub admin_token: Option<String>,
    /// Dev override: a login over a plain link is allowed (spec 76 § 7 refuses it).
    pub allow_plain_admin: bool,
}

impl Default for HostOptions {
    fn default() -> Self {
        Self {
            name: "Regia server".into(),
            motd: String::new(),
            max_players: 16,
            password: String::new(),
            tick_hz: 60,
            send_every_ticks: 2,
            level: String::new(),
            client_rate_bytes: 49_152,
            entropy: None,
            admin_token: None,
            allow_plain_admin: false,
        }
    }
}

#[derive(Debug)]
struct PendingLink {
    addr: SocketAddr,
    since_ms: u64,
    auth: Option<(AuthRequestId, Box<Hello>, u64)>,
    plain_wire: bool,
}

/// One session peer on the server.
#[derive(Debug)]
struct ServerPeer {
    link: Option<LinkId>,
    /// The link this peer had last, once it has none: the transport may hold that link's ticket
    /// for a peer in grace ([`SessionServer::grace_holds_link`]).
    last_link: Option<LinkId>,
    addr: SocketAddr,
    names: Vec<String>,
    locals: Vec<u8>,
    platform_id: Option<String>,
    reserved: bool,
    state: PeerState,
    since_ms: u64,
    last_recv_ms: u64,
    interrupted: bool,
    last_sent_ms: u64,
    token: [u8; RECONNECT_TOKEN_BYTES],
    /// Every older token of this peer the client may still hold, until it proves it holds the
    /// newest welcome (a message only a welcomed client sends, ADR-8535). A welcome can be lost
    /// with its link, or heard while a hello of an earlier attempt still carries an older token;
    /// a retry with any of them is the same resume again rather than `TokenRefused` and a fresh
    /// join. Bounded in number and time (ADR-8623).
    retry_tokens: UnconfirmedTokens,
    /// A resume put this player back in `Loading` and it has not reached `InGame` since: a link
    /// lost meanwhile is a lost link of a player in the game, so it keeps the grace slot
    /// (ADR-8623).
    resuming: bool,
    rate_bytes: u32,
    /// `Hello.client_rate_bytes` (0 = the server's default).
    declared_rate_bytes: u32,
    streams: StreamLink,
    admin: bool,
    admin_failures: u8,
    welcomed_travel: u32,
    plain_wire: bool,
    /// The driver reported the link's reliable window as backed up: streams pause and
    /// [`SessionServer::try_send_game`] refuses until it drains (spec 120 § 10.3).
    backpressured: bool,
}

/// A public row about one session peer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SessionPeerRow {
    /// Whether silence passed the interruption interval.
    pub interrupted: bool,
    /// Peer id.
    pub peer: u32,
    /// Player ids written `peer.local`.
    pub players: Vec<String>,
    /// Player names.
    pub names: Vec<String>,
    /// State name.
    pub state: &'static str,
    /// Address.
    pub addr: String,
    /// May use reserved slots.
    pub reserved: bool,
}

/// The server's session core.
pub struct SessionServer {
    settings: SessionSettings,
    opts: HostOptions,
    manifest: ContentManifest,
    session_id: u64,
    secret: [u8; 32],
    pending: BTreeMap<LinkId, PendingLink>,
    links: BTreeMap<LinkId, PeerId>,
    /// Peers whose transport connection was replaced under the same link id (a transport resume
    /// that took a live connection over, ADR-10088), by that link: detached, in their old state,
    /// until the link's hello says who came back. The link is pending while its entry is here.
    replaced: BTreeMap<LinkId, PeerId>,
    peers: BTreeMap<PeerId, ServerPeer>,
    next_peer: u32,
    level: String,
    travel_id: u32,
    tick: u64,
    now_ms: u64,
    /// Unix-epoch seconds on the injected clock, for ban checks (ADR-4772).
    now_unix_s: u64,
    draining: bool,
    dev_build: bool,
    bans: BanStore,
    auth: Option<Box<dyn SessionAuthenticator>>,
    /// The store's join ticket hook (KI-NT-047, ADR-10069): `Hello::auth` is the ticket.
    ticket_check: Option<std::sync::Arc<dyn JoinTicketCheck>>,
    /// Tickets the hook took a verdict on, for links not admitted yet.
    link_tickets: BTreeMap<LinkId, Vec<u8>>,
    /// The ticket of each admitted peer; ended when the peer is gone, matched against late
    /// refusals.
    peer_tickets: BTreeMap<PeerId, Vec<u8>>,
    rules_join: Option<JoinRule>,
    next_auth: u64,
    join_source: Option<JoinSource>,
    /// Dev builds: this process's registered protocol types, named in `protocol_mismatch`.
    protocol_types: Vec<(String, u64)>,
    /// The raw `replication.android_listen_max_clients` when this is an Android listen server
    /// (`None` on dedicated servers and other platforms; 0 reads as the default 8, spec 72 § 14).
    /// Survives `set_options`: a hot-reloaded `max_players` cannot raise capacity above the cap.
    android_listen_max_clients: Option<u32>,
    out: Vec<Outgoing>,
    /// `TravelBegin`s queued in `out` whose keep list is written when the driver takes them
    /// (ADR-7259): `(index into out, level, travel id)`.
    travel_slots: Vec<(usize, String, u32)>,
    /// The copies clients keep at the queued `TravelBegin`s, set by the driver from the world.
    travel_keep: Option<crate::session::proto::TravelKeep>,
    closing: Vec<(LinkId, u8)>,
    /// Links replaced by a token takeover: the driver drops them without a goodbye.
    quiet_closes: Vec<LinkId>,
    events: VecDeque<ServerSessionEvent>,
    admin_queue: Vec<(PeerId, String)>,
    /// Wrong passwords and admin tokens per address block.
    auth_strikes: BTreeMap<AddrBlock, AuthStrikes>,
}

impl std::fmt::Debug for SessionServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionServer")
            .field("session_id", &self.session_id)
            .field("peers", &self.peers.len())
            .field("pending", &self.pending.len())
            .field("level", &self.level)
            .field("travel_id", &self.travel_id)
            .finish()
    }
}

fn token_from(secret: &[u8; 32], peer: PeerId, nonce: u64) -> [u8; RECONNECT_TOKEN_BYTES] {
    let mut h = blake3::Hasher::new_keyed(secret);
    h.update(b"reconnect");
    h.update(&peer.0.to_le_bytes());
    h.update(&nonce.to_le_bytes());
    let mut out = [0u8; RECONNECT_TOKEN_BYTES];
    out.copy_from_slice(&h.finalize().as_bytes()[..RECONNECT_TOKEN_BYTES]);
    out
}

impl SessionServer {
    /// A server core. `secret` seeds the session id and reconnect tokens (tests pass a fixed one;
    /// real hosts pass `handshake::fresh_secret()`).
    pub fn new(
        settings: SessionSettings,
        opts: HostOptions,
        manifest: ContentManifest,
        secret: [u8; 32],
    ) -> Self {
        let session_id = u64::from_le_bytes(
            blake3::keyed_hash(&secret, b"session id").as_bytes()[..8]
                .try_into()
                .unwrap_or([0; 8]),
        );
        let level = opts.level.clone();
        Self {
            settings,
            opts,
            manifest,
            session_id,
            secret,
            pending: BTreeMap::new(),
            links: BTreeMap::new(),
            replaced: BTreeMap::new(),
            peers: BTreeMap::new(),
            next_peer: 1,
            level,
            travel_id: 1,
            tick: 0,
            now_ms: 0,
            now_unix_s: 0,
            draining: false,
            dev_build: cfg!(debug_assertions),
            bans: BanStore::in_memory(),
            auth: None,
            ticket_check: None,
            link_tickets: BTreeMap::new(),
            peer_tickets: BTreeMap::new(),
            rules_join: None,
            next_auth: 1,
            join_source: None,
            protocol_types: Vec::new(),
            android_listen_max_clients: None,
            out: Vec::new(),
            travel_slots: Vec::new(),
            travel_keep: None,
            closing: Vec::new(),
            quiet_closes: Vec::new(),
            events: VecDeque::new(),
            admin_queue: Vec::new(),
            auth_strikes: BTreeMap::new(),
        }
    }

    /// Whether `addr`'s block is inside its backoff wait ([`auth_backoff_ms`]): its try is then
    /// refused unchecked.
    fn auth_locked(&mut self, addr: SocketAddr) -> bool {
        let block = AddrBlock::of(addr.ip());
        let now = self.now_ms;
        match self.auth_strikes.get(&block) {
            Some(s) if now.saturating_sub(s.last_ms) >= AUTH_STRIKE_WINDOW_MS => {
                self.auth_strikes.remove(&block);
                false
            }
            Some(s) => now < s.wait_until_ms,
            None => false,
        }
    }

    /// Counts one wrong password or admin token from `addr`'s block.
    fn auth_strike(&mut self, addr: SocketAddr) {
        let block = AddrBlock::of(addr.ip());
        let now = self.now_ms;
        if !self.auth_strikes.contains_key(&block) && self.auth_strikes.len() >= MAX_AUTH_STRIKE_BLOCKS {
            self.auth_strikes
                .retain(|_, s| now.saturating_sub(s.last_ms) < AUTH_STRIKE_WINDOW_MS);
            if self.auth_strikes.len() >= MAX_AUTH_STRIKE_BLOCKS
                && let Some(least) = self
                    .auth_strikes
                    .iter()
                    .min_by_key(|(_, s)| s.count)
                    .map(|(block, _)| *block)
            {
                self.auth_strikes.remove(&least);
            }
        }
        let entry = self.auth_strikes.entry(block).or_insert(AuthStrikes {
            count: 0,
            last_ms: now,
            wait_until_ms: now,
        });
        entry.count = entry.count.saturating_add(1);
        entry.last_ms = now;
        entry.wait_until_ms = now.saturating_add(auth_backoff_ms(entry.count));
    }

    /// Console lines from logged-in remote admins (spec 76 § 7), oldest first; answer each with
    /// [`Self::admin_reply`].
    pub fn take_admin_commands(&mut self) -> Vec<(PeerId, String)> {
        std::mem::take(&mut self.admin_queue)
    }

    /// The random 64-bit session id.
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// The settings.
    pub fn settings(&self) -> &SessionSettings {
        &self.settings
    }

    /// The live options.
    pub fn options(&self) -> &HostOptions {
        &self.opts
    }

    /// Replaces the hot-reloadable options (name, motd, capacity and password for future joins,
    /// rates). The level is not changed (use [`Self::travel`]). On an Android listen server the
    /// reported capacity is clamped to the listen cap, matching what the capacity check
    /// enforces (spec 72 § 14).
    pub fn set_options(&mut self, opts: HostOptions) {
        let level = self.level.clone();
        self.opts = opts;
        self.opts.level = level;
        if let Some(cap) = self.android_listen_max_clients {
            self.opts.max_players = self
                .opts
                .max_players
                .min(crate::settings::android_listen_cap_value(cap));
        }
    }

    /// Installs the Android listen-server cap (spec 72 § 14). The host path calls this with
    /// `Some(replication.android_listen_max_clients)` for a non-dedicated server on Android and
    /// leaves it `None` otherwise. The capacity check clamps to it after `max_players` and the
    /// reserved slots; a `0` setting reads as the default
    /// [`crate::settings::ANDROID_LISTEN_MAX_CLIENTS_DEFAULT`].
    pub fn set_android_listen_cap(&mut self, android_listen_max_clients: Option<u32>) {
        self.android_listen_max_clients = android_listen_max_clients;
    }

    /// This server's content manifest.
    pub fn manifest(&self) -> &ContentManifest {
        &self.manifest
    }

    /// Treats the build as a dev build (`dev_allow_content_mismatch` applies) or not.
    pub fn set_dev_build(&mut self, dev: bool) {
        self.dev_build = dev;
    }

    /// Installs the authenticator.
    pub fn set_authenticator(&mut self, auth: Box<dyn SessionAuthenticator>) {
        self.auth = Some(auth);
    }

    /// Installs (`Some`) or removes the store's join ticket hook (KI-NT-047, ADR-10069), as
    /// [`crate::NetServer::set_ticket_check`] does for a bare server. The ticket is the hello's
    /// `auth` bytes (`ConnectOptions::auth`; empty when the client sent none). It runs on every
    /// transport link's hello (in-process loopback and simulated clients, links from
    /// `LOOPBACK_LINK_BASE` up, are not checked) after the password and before the
    /// authenticator, a resume's too, so a forged
    /// reconnect token cannot skip it. [`TicketVerdict::Refuse`] refuses the join with
    /// `auth_failed` and the detail `ticket_refused`; `Admit` and `Pending` go on. A late refusal
    /// ([`JoinTicketCheck::take_refusals`], read every [`Self::update`]) kicks that peer with
    /// `kicked: ticket_refused`. [`JoinTicketCheck::end_ticket`] runs when the peer is gone (not
    /// while it waits in grace) and when a link that had a verdict is refused or drops before
    /// admission. A peer that resumes with other ticket bytes ends its old ticket.
    pub fn set_ticket_check(&mut self, check: Option<std::sync::Arc<dyn JoinTicketCheck>>) {
        self.ticket_check = check;
    }

    /// True while a store join ticket hook is installed ([`Self::set_ticket_check`]).
    pub fn has_ticket_check(&self) -> bool {
        self.ticket_check.is_some()
    }

    /// Ends `ticket` unless another admitted peer or pending link holds the same bytes (a resume
    /// that sent its ticket again).
    fn end_ticket(&self, ticket: &[u8]) {
        let Some(check) = &self.ticket_check else {
            return;
        };
        let held = self.peer_tickets.values().any(|t| t == ticket)
            || self.link_tickets.values().any(|t| t == ticket);
        if !held {
            check.end_ticket(ticket);
        }
    }

    /// A link with a verdict was refused or dropped before admission.
    fn drop_link_ticket(&mut self, link: LinkId) {
        if let Some(ticket) = self.link_tickets.remove(&link) {
            self.end_ticket(&ticket);
        }
    }

    /// A link with a verdict became `peer` (a new peer or a resume).
    fn admit_link_ticket(&mut self, link: LinkId, peer: PeerId) {
        let Some(ticket) = self.link_tickets.remove(&link) else {
            return;
        };
        if let Some(old) = self.peer_tickets.insert(peer, ticket)
            && Some(&old) != self.peer_tickets.get(&peer)
        {
            self.end_ticket(&old);
        }
    }

    /// Kicks every peer whose ticket the store refused late.
    fn apply_ticket_refusals(&mut self) {
        let Some(check) = &self.ticket_check else {
            return;
        };
        let refused = check.take_refusals();
        if refused.is_empty() {
            return;
        }
        let peers: Vec<PeerId> = self
            .peer_tickets
            .iter()
            .filter(|(_, t)| refused.contains(t))
            .map(|(p, _)| *p)
            .collect();
        for peer in peers {
            tracing::info!(%peer, "net session: store refused the join ticket late");
            self.kick_peer(peer, TicketRefused::CODE);
        }
    }

    /// Installs the game's join hook (`SessionRules::on_player_joining`).
    pub fn set_join_rule(&mut self, rule: JoinRule) {
        self.rules_join = Some(rule);
    }

    /// Installs the join snapshot provider.
    pub fn set_join_source(&mut self, source: JoinSource) {
        self.join_source = Some(source);
    }

    /// Sets this process's registered protocol types (`runtime::protocol_types`). A dev build
    /// lists them in a `protocol_mismatch` refusal so the client can name the types that differ
    /// (spec 77 § 4 item 2); other builds never send them.
    pub fn set_protocol_types(&mut self, types: Vec<(String, u64)>) {
        self.protocol_types = types;
    }

    /// The ban list.
    pub fn bans(&self) -> &BanStore {
        &self.bans
    }

    /// The ban list, mutably.
    pub fn bans_mut(&mut self) -> &mut BanStore {
        &mut self.bans
    }

    /// Replaces the ban store (the host opens `bans.ron`).
    pub fn set_bans(&mut self, bans: BanStore) {
        self.bans = bans;
    }

    /// Unix-epoch seconds on the clock the host injected ([`Self::set_unix_now_s`]), the only
    /// clock the join-time ban check reads (ADR-4772). It stays 0 until the first injection,
    /// which keeps every timed ban active — the safe direction.
    pub fn now_unix_s(&self) -> u64 {
        self.now_unix_s
    }

    /// Injects the unix-epoch second for ban checks. The session driver calls this once per
    /// step; tests and replays supply the recorded value, so who is banned is a function of the
    /// session inputs, not of the host's system clock.
    pub fn set_unix_now_s(&mut self, now_unix_s: u64) {
        self.now_unix_s = now_unix_s;
    }

    /// The current level.
    pub fn level(&self) -> &str {
        &self.level
    }

    /// The current travel id.
    pub fn travel_id(&self) -> u32 {
        self.travel_id
    }

    /// The server tick (advanced by [`Self::update`]).
    pub fn tick(&self) -> u64 {
        self.tick
    }

    /// True once [`Self::drain`] ran.
    pub fn is_draining(&self) -> bool {
        self.draining
    }

    /// Updates an orthogonal interruption flag without changing the peer's lifecycle phase.
    pub fn set_interrupted(&mut self, link: LinkId, interrupted: bool) {
        if let Some(peer) = self.peer_of(link)
            && let Some(p) = self.peers.get_mut(&peer)
            && p.interrupted != interrupted
        {
            p.interrupted = interrupted;
            self.events.push_back(if interrupted {
                ServerSessionEvent::PeerInterrupted { peer }
            } else {
                ServerSessionEvent::PeerRestored { peer }
            });
        }
    }

    /// Records validated transport activity, including keepalives that carry no session message.
    pub fn note_link_activity(&mut self, link: LinkId, at_ms: u64) {
        if let Some(peer) = self.peer_of(link)
            && let Some(p) = self.peers.get_mut(&peer)
            && at_ms > p.last_recv_ms
        {
            p.last_recv_ms = at_ms;
            self.set_interrupted(link, false);
        }
    }

    /// The transport moved a link to a new address (a NAT rebind or a network change the
    /// transport authenticated): address bans, `peer_addr` and the peer rows follow it. Unknown
    /// links and pending links are updated too; nothing else changes.
    pub fn set_link_addr(&mut self, link: LinkId, addr: SocketAddr) {
        if let Some(pending) = self.pending.get_mut(&link) {
            pending.addr = addr;
            return;
        }
        if let Some(peer) = self.peer_of(link)
            && let Some(p) = self.peers.get_mut(&peer)
            && p.addr != addr
        {
            tracing::info!(%peer, old = %p.addr, new = %addr, "net session: peer address moved");
            p.addr = addr;
        }
    }

    /// The driver's flow-control signal for a link: `true` while its reliable send window is
    /// backed up (the transport refused with backpressure, or most of the window waits for
    /// acknowledgement). Streams to that peer pause and [`Self::try_send_game`] refuses until
    /// it clears; nothing is dropped (spec 120 § 10.3).
    pub fn set_link_backpressure(&mut self, link: LinkId, backpressured: bool) {
        if let Some(peer) = self.peer_of(link)
            && let Some(p) = self.peers.get_mut(&peer)
        {
            p.backpressured = backpressured;
        }
    }

    /// True while a peer's reliable window is backed up ([`Self::set_link_backpressure`]) or
    /// the peer is interrupted: a game with optional reliable traffic should hold it back.
    pub fn is_backpressured(&self, peer: PeerId) -> bool {
        self.peers
            .get(&peer)
            .is_some_and(|p| p.backpressured || p.interrupted)
    }

    /// A peer's state (`None` for unknown or retired peers).
    pub fn peer_state(&self, peer: PeerId) -> Option<PeerState> {
        self.peers.get(&peer).map(|p| p.state)
    }

    /// Every known peer with its state and the rate its `Hello` declared (`None` = the server's
    /// default), for replication's `NetSessionPeers` (ADR-7213).
    pub fn peer_rates(&self) -> impl Iterator<Item = (PeerId, PeerState, Option<u32>)> + '_ {
        self.peers.iter().map(|(id, p)| {
            (
                *id,
                p.state,
                (p.declared_rate_bytes > 0).then_some(p.declared_rate_bytes),
            )
        })
    }

    /// The peer behind a link.
    pub fn peer_of(&self, link: LinkId) -> Option<PeerId> {
        self.links.get(&link).copied()
    }

    /// The peer behind a link or, for a link whose transport connection was replaced in place
    /// and that has had no hello yet ([`Self::link_up`]), the peer that waits for it.
    pub fn peer_behind(&self, link: LinkId) -> Option<PeerId> {
        self.links
            .get(&link)
            .or_else(|| self.replaced.get(&link))
            .copied()
    }

    /// True when a peer in grace had `link` last: the driver's transport keeps that link's
    /// ticket while this holds, so the player's reconnect resumes the same link in one round
    /// trip (ADR-10088), and drops it otherwise.
    pub fn grace_holds_link(&self, link: LinkId) -> bool {
        self.peers
            .values()
            .any(|p| p.state == PeerState::Grace && p.last_link == Some(link))
    }

    /// The link of a peer (none during grace).
    pub fn link_of(&self, peer: PeerId) -> Option<LinkId> {
        self.peers.get(&peer).and_then(|p| p.link)
    }

    /// Links still handshaking.
    pub fn handshaking(&self) -> usize {
        self.pending.len()
    }

    /// Players holding a slot (every state but `Gone`, grace included).
    pub fn players_in_use(&self) -> u32 {
        self.peers.values().map(|p| p.locals.len() as u32).sum()
    }

    /// Every live peer id, ascending.
    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    /// One row per session peer.
    pub fn peer_rows(&self) -> Vec<SessionPeerRow> {
        self.peers
            .iter()
            .map(|(id, p)| SessionPeerRow {
                peer: id.0,
                interrupted: p.interrupted,
                players: p.locals.iter().map(|l| format!("{}.{l}", id.0)).collect(),
                names: p.names.clone(),
                state: p.state.as_str(),
                addr: p.addr.to_string(),
                reserved: p.reserved,
            })
            .collect()
    }

    /// The player's name, if connected.
    pub fn player_name(&self, player: NetPlayerId) -> Option<&str> {
        let p = self.peers.get(&player.peer)?;
        let i = p.locals.iter().position(|l| *l == player.local)?;
        p.names.get(i).map(String::as_str)
    }

    /// The peer's address.
    pub fn peer_addr(&self, peer: PeerId) -> Option<SocketAddr> {
        self.peers.get(&peer).map(|p| p.addr)
    }

    /// The peer's platform id from the authenticator.
    pub fn peer_platform_id(&self, peer: PeerId) -> Option<&str> {
        self.peers.get(&peer).and_then(|p| p.platform_id.as_deref())
    }

    /// True when the peer is a logged-in remote admin.
    pub fn is_admin(&self, peer: PeerId) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.admin)
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<ServerSessionEvent> {
        self.events.pop_front()
    }

    /// Takes the outgoing messages. Queued `TravelBegin`s get their keep list now
    /// ([`Self::set_travel_keep`], ADR-7259).
    pub fn take_outgoing(&mut self) -> Vec<Outgoing> {
        if !self.travel_slots.is_empty() {
            let keep = self.travel_keep.take();
            for (at, level, travel_id) in std::mem::take(&mut self.travel_slots) {
                let mut msg = SessionMsg::TravelBegin {
                    level,
                    travel_id,
                    keep: keep.clone(),
                };
                let bytes = msg.encode().or_else(|_| {
                    // Validated on the way in, so only the list can be at fault: send none.
                    if let SessionMsg::TravelBegin { keep, .. } = &mut msg {
                        *keep = None;
                    }
                    msg.encode()
                });
                match (self.out.get_mut(at), bytes) {
                    (Some(o), Ok(bytes)) => o.bytes = bytes,
                    (_, Err(e)) => {
                        tracing::warn!(error = %e, "net session: travel message too large to send")
                    }
                    (None, Ok(_)) => {}
                }
            }
            // A slot whose encoding failed leaves an empty message: drop it.
            self.out.retain(|o| !o.bytes.is_empty());
        }
        std::mem::take(&mut self.out)
    }

    /// Whether a `TravelBegin` waits for its keep list (the driver then reads the world and calls
    /// [`Self::set_travel_keep`] before it takes the outgoing messages).
    pub fn has_pending_travel(&self) -> bool {
        !self.travel_slots.is_empty()
    }

    /// The copies clients keep at the queued `TravelBegin`s (spec 77 § 6.2 step 3, ADR-7259): the
    /// persistent `NetId`s and the first unassigned `NetId`, read from the world in the same step
    /// that despawns the old level. `None` (the default) sends no list: clients then wait for the
    /// despawn records.
    pub fn set_travel_keep(&mut self, keep: Option<crate::session::proto::TravelKeep>) {
        self.travel_keep = keep;
    }

    /// Links the driver must drop now without telling the far end (the old link of a peer that
    /// took its slot over from a new link, spec 120 § 10.3). Drivers that only call
    /// [`Self::take_closes`] get these there instead, closed with a goodbye.
    pub fn take_quiet_closes(&mut self) -> Vec<LinkId> {
        std::mem::take(&mut self.quiet_closes)
    }

    /// Links the driver must close now (with the quiet closes [`Self::take_quiet_closes`] did not
    /// take).
    pub fn take_closes(&mut self) -> Vec<LinkId> {
        let mut now = std::mem::take(&mut self.quiet_closes);
        self.closing.retain_mut(|(link, left)| {
            if *left == 0 {
                now.push(*link);
                false
            } else {
                true
            }
        });
        now
    }

    /// Queues `msg`; `false` when it does not encode (too large) and nothing was queued.
    fn send(&mut self, link: LinkId, channel: Channel, msg: &SessionMsg) -> bool {
        match msg.encode() {
            Ok(bytes) => {
                self.out.push(Outgoing { link, channel, bytes });
                if let Some(peer) = self.links.get(&link).and_then(|p| self.peers.get_mut(p)) {
                    peer.last_sent_ms = self.now_ms;
                }
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "net session: message too large to send");
                false
            }
        }
    }

    fn close_later(&mut self, link: LinkId) {
        if !self.closing.iter().any(|(l, _)| *l == link) {
            self.closing.push((link, CLOSE_AFTER_UPDATES));
        }
    }

    fn set_state(&mut self, peer: PeerId, to: PeerState) {
        if let Some(p) = self.peers.get_mut(&peer)
            && p.state != to
        {
            let from = p.state;
            p.state = to;
            p.since_ms = self.now_ms;
            self.events
                .push_back(ServerSessionEvent::PeerStateChanged { peer, from, to });
        }
    }

    /// A transport link came up. Beyond `max_handshaking` pending links the link is closed.
    ///
    /// A link id that still has a peer is a transport that resumed the connection in place (the
    /// client came back with its transport ticket before the old connection timed out; wire 6,
    /// ADR-10088): the old connection is gone, so the peer is detached, and the link waits for a
    /// hello like any new one. The peer keeps its state meanwhile: a hello with its reconnect
    /// token resumes it exactly as a takeover from a new link does (spec 120 § 10.3); any other
    /// outcome of the link (another player's hello, a refusal, a timeout, the link going down)
    /// is the loss of the old link, with the grace a timeout gets.
    pub fn link_up(&mut self, link: LinkId, addr: SocketAddr, now_ms: u64) {
        self.now_ms = now_ms;
        self.detach_replaced(link);
        // Nothing meant for the connection this id had before reaches the new one.
        self.closing.retain(|(l, _)| *l != link);
        self.out.retain(|o| o.link != link);
        if self.pending.len() >= self.settings.max_handshaking as usize && !self.pending.contains_key(&link) {
            tracing::info!(%addr, "net session: too many handshaking peers; closing the transport");
            self.closing.push((link, 0));
            self.settle_replaced(link, None);
            return;
        }
        self.pending.insert(
            link,
            PendingLink {
                addr,
                since_ms: now_ms,
                auth: None,
                plain_wire: false,
            },
        );
    }

    /// Marks a link as a network link without encryption. In-process links are never marked:
    /// they cross no wire.
    pub fn mark_link_plain(&mut self, link: LinkId) {
        if let Some(pending) = self.pending.get_mut(&link) {
            pending.plain_wire = true;
        }
    }

    /// The peer bound to `link` loses it because the transport replaced the connection behind
    /// the id ([`Self::link_up`]): its streams fail with `link_replaced`, nothing queued for the
    /// old connection reaches the new one, and the peer waits in [`Self::replaced`] for the
    /// link's hello.
    fn detach_replaced(&mut self, link: LinkId) {
        let Some(peer) = self.links.remove(&link) else {
            return;
        };
        let mut events = Vec::new();
        if let Some(p) = self.peers.get_mut(&peer) {
            p.link = None;
            p.last_link = Some(link);
            p.streams.fail_all("link_replaced", &mut events);
            p.admin = false;
            p.backpressured = false;
        }
        self.replaced.insert(link, peer);
        tracing::info!(%peer, link = link.0, "net session: the transport resumed a live link in place");
        for event in events {
            self.events.push_back(ServerSessionEvent::Stream { peer, event });
        }
    }

    /// The link of a replaced connection got its answer ([`Self::link_up`]): `resumed` came back
    /// on it, or nobody did. A detached peer that is not the one resumed, and that no other link
    /// picked up meanwhile, lost its link the way a timeout loses it.
    fn settle_replaced(&mut self, link: LinkId, resumed: Option<PeerId>) {
        let Some(old) = self.replaced.remove(&link) else {
            return;
        };
        if Some(old) == resumed {
            return;
        }
        let waiting = self.peers.get(&old).is_some_and(|p| {
            p.link.is_none()
                && matches!(
                    p.state,
                    PeerState::Loading | PeerState::Joining | PeerState::InGame
                )
        });
        if waiting {
            self.peer_lost(old, DisconnectReason::Timeout);
        }
    }

    /// A transport link went down.
    pub fn link_down(&mut self, link: LinkId, reason: DisconnectReason, now_ms: u64) {
        self.now_ms = now_ms;
        self.closing.retain(|(l, _)| *l != link);
        self.settle_replaced(link, None);
        if self.pending.remove(&link).is_some() {
            self.drop_link_ticket(link);
            return;
        }
        let Some(peer) = self.links.remove(&link) else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&peer) {
            p.link = None;
            p.last_link = Some(link);
        }
        self.peer_lost(peer, reason);
    }

    /// The peer's link is gone (drop, timeout, goodbye). Grace for unexpected drops from `InGame`,
    /// and from the `Loading` / `Joining` of a resume (ADR-8623).
    fn peer_lost(&mut self, peer: PeerId, reason: DisconnectReason) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        let link = p.link.take();
        if link.is_some() {
            p.last_link = link;
        }
        let mut stream_events = Vec::new();
        p.streams.fail_all(reason.code(), &mut stream_events);
        p.admin = false;
        let state = p.state;
        let interrupted = p.interrupted;
        let resuming = p.resuming;
        if let Some(link) = link {
            self.links.remove(&link);
            self.close_later(link);
        }
        for event in stream_events {
            self.events.push_back(ServerSessionEvent::Stream { peer, event });
        }
        let grace = ms(self.settings.reconnect_grace_s);
        // A reliable window that overflowed while the link was silent is the link's fault, not
        // the game's: the player keeps the grace slot (spec 120 § 10.3).
        let link_fault = reason == DisconnectReason::ReliableOverflow && interrupted;
        // A player a resume put back in `Loading` or `Joining` is still a player in the game: the
        // token it holds and the welcome it may have heard must find the slot (ADR-8623).
        let in_game = state == PeerState::InGame
            || (resuming && matches!(state, PeerState::Loading | PeerState::Joining));
        if in_game && (reason.enters_grace() || link_fault) && grace > 0 && !self.draining {
            self.set_state(peer, PeerState::Grace);
            self.events
                .push_back(ServerSessionEvent::PlayerDropped { peer, reason });
        } else {
            self.retire(peer, reason);
        }
    }

    fn retire(&mut self, peer: PeerId, reason: DisconnectReason) {
        let Some(p) = self.peers.get(&peer) else {
            return;
        };
        let locals = p.locals.clone();
        if let Some(link) = p.link {
            self.links.remove(&link);
            self.close_later(link);
        }
        self.set_state(peer, PeerState::Gone);
        tracing::info!(%peer, reason = %reason, "net session: peer left");
        for local in locals {
            self.events.push_back(ServerSessionEvent::PlayerLeft {
                player: NetPlayerId { peer, local },
                reason: reason.clone(),
            });
        }
        self.peers.remove(&peer);
        if let Some(ticket) = self.peer_tickets.remove(&peer) {
            self.end_ticket(&ticket);
        }
    }

    /// Bytes arrived on a link.
    pub fn receive(&mut self, link: LinkId, bytes: &[u8], now_ms: u64) {
        self.now_ms = now_ms;
        if self.pending.contains_key(&link) {
            self.receive_hello(link, bytes);
            return;
        }
        let Some(&peer) = self.links.get(&link) else {
            return;
        };
        if let Some(p) = self.peers.get_mut(&peer) {
            p.last_recv_ms = now_ms;
        }
        let msg = match SessionMsg::decode(bytes) {
            Ok(m) => m,
            Err(_) => {
                self.disconnect_peer(peer, DisconnectReason::MalformedPacket);
                return;
            }
        };
        self.set_interrupted(link, false);
        // Only a welcomed client sends these: it holds the newest token, so the one its resume
        // presented is spent for good (ADR-8535). Keepalives and hello copies prove nothing.
        if matches!(
            msg,
            SessionMsg::LevelReady { .. } | SessionMsg::InGame { .. } | SessionMsg::Game { .. }
        ) && let Some(p) = self.peers.get_mut(&peer)
        {
            p.retry_tokens.confirm();
        }
        match msg {
            SessionMsg::LevelReady { travel_id } => {
                let state = self.peers.get(&peer).map(|p| p.state);
                if state == Some(PeerState::Loading) && travel_id == self.travel_id {
                    self.begin_join(peer);
                }
                // A stale travel id is ignored (spec 77 § 6.2).
            }
            SessionMsg::InGame { .. } => {
                if self.peers.get(&peer).map(|p| p.state) == Some(PeerState::Joining) {
                    if let Some(p) = self.peers.get_mut(&peer) {
                        p.resuming = false;
                    }
                    self.set_state(peer, PeerState::InGame);
                    let players = self.players_of(peer);
                    self.events
                        .push_back(ServerSessionEvent::PlayerJoined { peer, players });
                }
            }
            SessionMsg::Goodbye(reason) => {
                let reason = if reason == DisconnectReason::Suspended {
                    DisconnectReason::Suspended
                } else {
                    DisconnectReason::ClientQuit
                };
                self.peer_lost(peer, reason);
            }
            SessionMsg::Keepalive => {}
            m @ (SessionMsg::StreamBegin { .. } | SessionMsg::StreamChunk { .. }) => {
                let mut events = Vec::new();
                let ok = self
                    .peers
                    .get_mut(&peer)
                    .is_some_and(|p| p.streams.receive(&m, &mut events).is_ok());
                let held: u64 = self
                    .peers
                    .values()
                    .map(|p| p.streams.incoming_bytes() as u64)
                    .sum();
                let cap = self.settings.max_stream_buffer_bytes;
                if cap > 0
                    && held > cap
                    && let Some(p) = self.peers.get_mut(&peer)
                {
                    tracing::warn!(%peer, held, "net session: stream buffers over the server limit");
                    p.streams.fail_incoming("stream_buffer_full", &mut events);
                }
                for event in events {
                    self.events.push_back(ServerSessionEvent::Stream { peer, event });
                }
                if !ok {
                    self.disconnect_peer(peer, DisconnectReason::MalformedPacket);
                }
            }
            SessionMsg::AdminLogin { token } => self.admin_login(peer, &token),
            SessionMsg::AdminCommand { line } => {
                if self.is_admin(peer) {
                    self.admin_queue.push((peer, line));
                } else {
                    self.admin_reply(peer, vec!["not an admin".into()]);
                }
            }
            SessionMsg::Game { bytes } => self.events.push_back(ServerSessionEvent::Game { peer, bytes }),
            SessionMsg::Hello(_) => {}
            // Server-to-client messages from a client are a protocol error.
            _ => self.disconnect_peer(peer, DisconnectReason::MalformedPacket),
        }
    }

    fn admin_login(&mut self, peer: PeerId, token: &str) {
        let Some(expected) = self.opts.admin_token.clone() else {
            self.admin_reply(peer, vec!["remote admin is off".into()]);
            return;
        };
        // Refused before the compare and without a strike: the token already crossed the wire
        // readable, and this peer may be honest and only misconfigured.
        let plain = self.peers.get(&peer).is_some_and(|p| p.plain_wire);
        if plain && !self.opts.allow_plain_admin {
            tracing::warn!(%peer, "net session: remote admin login over a plain link refused");
            self.admin_reply(peer, vec!["remote admin needs an encrypted link".into()]);
            return;
        }
        let Some(addr) = self.peers.get(&peer).map(|p| p.addr) else {
            return;
        };
        if self.auth_locked(addr) {
            tracing::warn!(%peer, "net session: admin login refused, too many wrong tokens from its address");
            self.disconnect_peer(peer, DisconnectReason::AuthFailed("admin token".into()));
            return;
        }
        let ok = !expected.is_empty() && constant_time_eq(expected.as_bytes(), token.as_bytes());
        if !ok {
            self.auth_strike(addr);
        }
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        if ok {
            p.admin = true;
            p.admin_failures = 0;
            tracing::info!(%peer, "net session: remote admin login");
            self.admin_reply(peer, vec!["admin: logged in".into()]);
        } else {
            p.admin_failures = p.admin_failures.saturating_add(1);
            if p.admin_failures >= 3 {
                tracing::warn!(%peer, "net session: three wrong admin tokens");
                self.disconnect_peer(peer, DisconnectReason::AuthFailed("admin token".into()));
            } else {
                self.admin_reply(peer, vec!["admin: wrong token".into()]);
            }
        }
    }

    /// Sends console output to a remote admin.
    pub fn admin_reply(&mut self, peer: PeerId, lines: Vec<String>) {
        if let Some(link) = self.link_of(peer) {
            self.send(link, Channel::GAME_RPC, &SessionMsg::AdminOutput { lines });
        }
    }

    fn players_of(&self, peer: PeerId) -> Vec<NetPlayerId> {
        self.peers
            .get(&peer)
            .map(|p| {
                p.locals
                    .iter()
                    .map(|&local| NetPlayerId { peer, local })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn refuse(&mut self, link: LinkId, reason: DisconnectReason) {
        tracing::info!(reason = %reason, "net session: join refused");
        self.send(link, Channel::GAME_RPC, &SessionMsg::Refused(reason.clone()));
        self.pending.remove(&link);
        self.settle_replaced(link, None);
        self.drop_link_ticket(link);
        self.close_later(link);
        self.events
            .push_back(ServerSessionEvent::Refused { link, reason });
    }

    fn receive_hello(&mut self, link: LinkId, bytes: &[u8]) {
        if self.pending.get(&link).is_some_and(|p| p.auth.is_some()) {
            return; // Waiting on the authenticator; duplicates are ignored.
        }
        // 1-2: the prefix alone answers a foreign version.
        let Some((version, hash)) = Hello::decode_prefix(bytes) else {
            self.refuse(link, DisconnectReason::MalformedPacket);
            return;
        };
        // Peers of different versions join when this build still speaks the client's oldest; the
        // state wire is then negotiated per connection (ADR-1950).
        // The refusals name which side is older (spec 120 § 12.2, ADR-7985): the client's oldest
        // version above the server's newest means the server is older; below this build's
        // oldest, the client is.
        if !(crate::settings::MIN_NET_PROTOCOL_VERSION..=self.manifest.protocol_version).contains(&version) {
            let side = if version > self.manifest.protocol_version {
                MismatchSide::ServerOlder
            } else {
                MismatchSide::ClientOlder
            };
            self.refuse(
                link,
                DisconnectReason::VersionMismatch(format!(
                    "client {version}, server {}: {} ({})",
                    self.manifest.protocol_version,
                    side.advice(),
                    side.as_str()
                )),
            );
            return;
        }
        if hash != self.manifest.protocol_hash {
            let side = Hello::decode(bytes).map_or(MismatchSide::Unordered, |h| {
                manifest_side(&h.manifest, &self.manifest)
            });
            let mut detail = format!(
                "client {hash:#018x}, server {:#018x}: {} ({})",
                self.manifest.protocol_hash,
                side.advice(),
                side.as_str()
            );
            if self.dev_build && !self.protocol_types.is_empty() {
                let room = crate::session::proto::MAX_DETAIL_BYTES.saturating_sub(detail.len());
                detail.push_str(&protocol_types_line(&self.protocol_types, room));
            }
            self.refuse(link, DisconnectReason::ProtocolMismatch(detail));
            return;
        }
        let hello = match Hello::decode(bytes) {
            Ok(h) => h,
            Err(_) => {
                self.refuse(link, DisconnectReason::MalformedPacket);
                return;
            }
        };
        if self.draining {
            self.refuse(link, DisconnectReason::ServerShuttingDown);
            return;
        }
        let Some(names) = hello.trimmed_names() else {
            self.refuse(
                link,
                DisconnectReason::InvalidMessage("player names must be 1..=32 characters".into()),
            );
            return;
        };
        let mut hello = hello;
        hello.player_names = names;
        // 3: content.
        let diff = hello.manifest.diff(&self.manifest);
        if !diff.is_empty() {
            let mut detail = diff
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n");
            // An older or newer release gets one more line naming the side (ADR-7985); the same
            // release keeps the item list alone (`content_mismatch` already says what to do).
            let side = manifest_side(&hello.manifest, &self.manifest);
            if side.server_is_older().is_some() {
                detail.push_str(&format!("\n{} ({})", side.advice(), side.as_str()));
            }
            if self.dev_build
                && self.settings.dev_allow_content_mismatch
                && diff[0].item != "manifest_version"
            {
                tracing::warn!(%detail, "net session: content mismatch allowed (dev_allow_content_mismatch)");
            } else {
                self.refuse(link, DisconnectReason::ContentMismatch(detail));
                return;
            }
        }
        // 4: password, then the authenticator.
        let addr = self.pending.get(&link).map(|p| p.addr);
        let Some(addr) = addr else {
            return;
        };
        if !self.opts.password.is_empty() {
            if self.auth_locked(addr) {
                self.refuse(link, DisconnectReason::BadPassword);
                return;
            }
            let given = hello.password.as_deref().unwrap_or("");
            if !constant_time_eq(given.as_bytes(), self.opts.password.as_bytes()) {
                self.auth_strike(addr);
                self.refuse(link, DisconnectReason::BadPassword);
                return;
            }
        }
        // In-process loopback and simulated clients (`LOOPBACK_LINK_BASE` and up) are this
        // process's own players: no store ticket, as a bare `NetServer` never sees them either.
        if link.0 < crate::session::hub::LOOPBACK_LINK_BASE
            && let Some(check) = &self.ticket_check
        {
            let ticket = hello.auth.clone().unwrap_or_default();
            if check.verdict(&ticket) == TicketVerdict::Refuse {
                self.refuse(link, DisconnectReason::AuthFailed(TicketRefused::CODE.into()));
                return;
            }
            // A verdict on a link already holding one (a repeated hello) ends the earlier one.
            if let Some(old) = self.link_tickets.insert(link, ticket)
                && Some(&old) != self.link_tickets.get(&link)
            {
                self.end_ticket(&old);
            }
        }
        if let Some(auth) = self.auth.as_mut() {
            let id = AuthRequestId(self.next_auth);
            self.next_auth += 1;
            match auth.check(id, &hello, &addr) {
                AuthResult::Pending => {
                    if let Some(p) = self.pending.get_mut(&link) {
                        p.auth = Some((id, Box::new(hello), self.now_ms));
                    }
                }
                AuthResult::Reject(text) => self.refuse(link, DisconnectReason::AuthFailed(text.into())),
                AuthResult::Accept {
                    platform_id,
                    reserved,
                } => self.after_auth(link, hello, platform_id, reserved),
            }
        } else {
            self.after_auth(link, hello, None, false);
        }
    }

    fn after_auth(&mut self, link: LinkId, hello: Hello, platform_id: Option<String>, reserved: bool) {
        let Some(addr) = self.pending.get(&link).map(|p| p.addr) else {
            return;
        };
        // 5: bans.
        let now_s = self.now_unix_s;
        let subject = BanSubject {
            platform_id: platform_id.as_deref(),
            addr: Some(addr.ip()),
            names: &hello.player_names,
        };
        if let Some(entry) = self.bans.list().find(&subject, now_s) {
            let detail = entry.detail();
            self.refuse(link, DisconnectReason::Banned(detail));
            return;
        }
        // 6: reconnect token. It resumes a grace slot, and it also takes over a slot whose old
        // link the server still thinks is live (`Loading`, `Joining`, `InGame`, interrupted or
        // not): a client that comes back before the server's own timeout fires (one-way loss, a
        // game that retries at once, a restarted process) keeps its `PeerId` and its slot instead
        // of joining twice or being refused `full` by its own ghost (spec 120 § 10.3).
        if let Some(token) = hello.reconnect_token {
            // Every peer and every row is compared, so the time does not say which one matched.
            let now = self.now_ms;
            let mut resumed = None;
            for (id, p) in &self.peers {
                let newest = constant_time_eq(&p.token, &token);
                let retry = p.retry_tokens.matches(&token, now);
                let held = matches!(
                    p.state,
                    PeerState::Grace | PeerState::Loading | PeerState::Joining | PeerState::InGame
                );
                if held & (newest | retry) {
                    resumed = Some(*id);
                }
            }
            if let Some(peer) = resumed {
                self.take_over(peer);
                self.resume(link, peer, &hello);
                return;
            }
            tracing::info!(link = link.0, "net session: reconnect token matches no held slot");
            self.events.push_back(ServerSessionEvent::TokenRefused { link });
        }
        // 7: capacity.
        if !hello.local_player_count_ok() {
            self.refuse(link, DisconnectReason::TooManyLocalPlayers);
            return;
        }
        let want = hello.player_names.len() as u32;
        let base = if reserved {
            self.opts.max_players
        } else {
            self.opts.max_players.saturating_sub(self.settings.reserved_slots)
        };
        // Android listen servers serve at most `android_listen_max_clients` players (spec 72 § 14),
        // whatever a larger `max_players` (also a hot-reloaded one) says.
        let cap = self.android_listen_max_clients.map_or(base, |setting| {
            base.min(crate::settings::android_listen_cap_value(setting))
        });
        if self.players_in_use() + want > cap {
            self.refuse(link, DisconnectReason::ServerFull);
            return;
        }
        // The game's own verdict runs after the engine's checks.
        if let Some(rule) = self.rules_join.as_mut()
            && let JoinDecision::Refuse(text) = rule(&hello)
        {
            self.refuse(link, DisconnectReason::Kicked(text));
            return;
        }
        let plain_wire = self.pending.remove(&link).is_some_and(|p| p.plain_wire);
        // A new player on a link whose old connection was replaced: the old peer lost its link.
        self.settle_replaced(link, None);
        let Some(peer) = self.alloc_peer() else {
            self.refuse(link, DisconnectReason::ServerFull);
            return;
        };
        let token = token_from(&self.secret, peer, self.next_auth ^ self.tick.rotate_left(17));
        self.next_auth += 1;
        let locals: Vec<u8> = (0..want as u8).collect();
        let rate = if hello.client_rate_bytes == 0 {
            self.opts.client_rate_bytes
        } else {
            hello.client_rate_bytes.min(self.opts.client_rate_bytes.max(1))
        };
        self.peers.insert(
            peer,
            ServerPeer {
                link: Some(link),
                last_link: None,
                addr,
                names: hello.player_names.clone(),
                locals,
                platform_id,
                reserved,
                state: PeerState::Handshaking,
                since_ms: self.now_ms,
                last_recv_ms: self.now_ms,
                interrupted: false,
                last_sent_ms: self.now_ms,
                token,
                retry_tokens: UnconfirmedTokens::default(),
                resuming: false,
                rate_bytes: rate,
                declared_rate_bytes: hello.client_rate_bytes,
                streams: StreamLink::default(),
                admin: false,
                admin_failures: 0,
                welcomed_travel: 0,
                plain_wire,
                backpressured: false,
            },
        );
        self.links.insert(link, peer);
        self.admit_link_ticket(link, peer);
        self.welcome(peer);
    }

    /// The next client id, from 1 upward. Never reuses an id in this session. `u32::MAX` is not
    /// issued (spec 70 § 4); `None` means the join must be refused with `ServerFull`.
    fn alloc_peer(&mut self) -> Option<PeerId> {
        loop {
            if self.next_peer == 0 || self.next_peer == u32::MAX {
                return None;
            }
            let id = PeerId(self.next_peer);
            self.next_peer = self.next_peer.saturating_add(1);
            if !self.peers.contains_key(&id) {
                return Some(id);
            }
        }
    }

    fn welcome(&mut self, peer: PeerId) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        p.welcomed_travel = self.travel_id;
        let link = p.link;
        let names = p.names.clone();
        let welcome = Welcome {
            peer_id: peer,
            session_id: self.session_id,
            server_tick: self.tick,
            tick_hz: self.opts.tick_hz,
            send_every_ticks: self.opts.send_every_ticks,
            level: self.level.clone(),
            travel_id: self.travel_id,
            motd: self.opts.motd.clone(),
            server_name: self.opts.name.clone(),
            reconnect_token: p.token,
            players: p
                .locals
                .iter()
                .map(|&local| NetPlayerId { peer, local })
                .collect(),
        };
        let players = welcome.players.clone();
        if let Some(link) = link {
            self.send(link, Channel::GAME_RPC, &SessionMsg::Welcome(Box::new(welcome)));
        }
        tracing::info!(%peer, ?names, "net session: welcome");
        self.events
            .push_back(ServerSessionEvent::PeerWelcomed { peer, players, names });
        // The state machine passes through Handshaking only for event consumers.
        self.set_state(peer, PeerState::Loading);
    }

    /// Detaches a live peer's old link before a token resume: the link is dropped without a
    /// goodbye (the client already left it), its streams fail with `link_replaced`, and nothing
    /// else about the peer changes. No-op for a peer in grace (it has no link).
    fn take_over(&mut self, peer: PeerId) {
        let Some(p) = self.peers.get_mut(&peer) else {
            return;
        };
        let Some(old) = p.link.take() else {
            return;
        };
        let mut events = Vec::new();
        p.streams.fail_all("link_replaced", &mut events);
        p.admin = false;
        p.backpressured = false;
        self.links.remove(&old);
        self.closing.retain(|(l, _)| *l != old);
        self.out.retain(|o| o.link != old);
        self.quiet_closes.push(old);
        tracing::info!(%peer, old = old.0, "net session: reconnect token took over a live slot");
        for event in events {
            self.events.push_back(ServerSessionEvent::Stream { peer, event });
        }
    }

    fn resume(&mut self, link: LinkId, peer: PeerId, hello: &Hello) {
        let Some((addr, plain_wire)) = self.pending.remove(&link).map(|p| (p.addr, p.plain_wire)) else {
            return;
        };
        self.settle_replaced(link, Some(peer));
        let now = self.now_ms;
        // A token works once: one sniffed `Welcome` must not be good for the whole session.
        let token = token_from(&self.secret, peer, self.next_auth ^ self.tick.rotate_left(17));
        self.next_auth += 1;
        let mut restored = false;
        // A superseded token lives as long as the slot it names can wait for the client: the load
        // and the grace after a lost link (ADR-8623).
        let lifetime = ms(self.settings.reconnect_grace_s).max(ms(self.settings.loading_timeout_s));
        if let Some(p) = self.peers.get_mut(&peer) {
            // The interruption belonged to the old link.
            restored = std::mem::take(&mut p.interrupted);
            p.backpressured = false;
            // Every older token stays good for a retry until the client shows it got the welcome
            // with the new one (ADR-8535): the one this hello presented, the newest one it
            // replaces, and the unconfirmed ones before them (ADR-8623).
            if let Some(presented) = hello.reconnect_token {
                p.retry_tokens =
                    std::mem::take(&mut p.retry_tokens).after_resume(p.token, presented, now, lifetime);
            }
            p.token = token;
            p.resuming = true;
            p.link = Some(link);
            p.addr = addr;
            // The new link decides: a slot resumed over a plain link is a plain peer.
            p.plain_wire = plain_wire;
            p.last_recv_ms = now;
            p.last_sent_ms = now;
            p.names.clone_from(&hello.player_names);
            p.names.truncate(p.locals.len());
        }
        self.links.insert(link, peer);
        self.admit_link_ticket(link, peer);
        if restored {
            self.events.push_back(ServerSessionEvent::PeerRestored { peer });
        }
        tracing::info!(%peer, "net session: resumed with the reconnect token");
        self.events.push_back(ServerSessionEvent::PlayerResumed { peer });
        self.welcome(peer);
    }

    fn begin_join(&mut self, peer: PeerId) {
        self.set_state(peer, PeerState::Joining);
        let snapshot = match self.join_source.as_mut() {
            Some(source) => source(peer),
            None => JoinSnapshot {
                tick: self.tick,
                ..JoinSnapshot::default()
            },
        };
        let Some(link) = self.link_of(peer) else {
            return;
        };
        // Every part of the snapshot or no `InGame`: a client let in without one record would play
        // on missing that state for good (rc1-rev-net-k 4). A game's join source past the session's
        // payload limit refuses the join instead.
        let mut sent = true;
        for chunk in snapshot.removed.chunks(crate::session::proto::MAX_JOIN_REMOVED) {
            sent &= self.send(
                link,
                Channel::GAME_RPC,
                &SessionMsg::JoinRemoved {
                    guids: chunk.to_vec(),
                },
            );
        }
        for record in snapshot.records {
            sent &= self.send(
                link,
                Channel::GAME_RPC,
                &SessionMsg::JoinRecords { bytes: record },
            );
        }
        if !sent {
            tracing::warn!(%peer, "net session: the join snapshot did not fit the session's messages");
            self.disconnect_peer(
                peer,
                DisconnectReason::LevelLoadFailed(
                    "the join snapshot has a record too large to send".to_owned(),
                ),
            );
            return;
        }
        self.send(
            link,
            Channel::GAME_RPC,
            &SessionMsg::InGame { tick: snapshot.tick },
        );
    }

    /// Disconnects a peer with a reason (sent as a goodbye), retiring it (no grace).
    pub fn disconnect_peer(&mut self, peer: PeerId, reason: DisconnectReason) {
        if let Some(link) = self.link_of(peer) {
            self.send(link, Channel::GAME_RPC, &SessionMsg::Goodbye(reason.clone()));
        }
        if let Some(p) = self.peers.get_mut(&peer) {
            let mut events = Vec::new();
            p.streams.fail_all(reason.code(), &mut events);
            for event in events {
                self.events.push_back(ServerSessionEvent::Stream { peer, event });
            }
        }
        self.retire(peer, reason);
    }

    /// Removes one player (spec 77 § 7.2). The peer is disconnected with `kicked` when it was its
    /// last player. `false` when the player is unknown.
    pub fn kick(&mut self, player: NetPlayerId, reason: &str) -> bool {
        let Some(p) = self.peers.get_mut(&player.peer) else {
            return false;
        };
        let Some(i) = p.locals.iter().position(|l| *l == player.local) else {
            return false;
        };
        tracing::info!(peer = %player.peer, local = player.local, %reason, "net session: kick");
        if p.locals.len() == 1 {
            self.disconnect_peer(player.peer, DisconnectReason::Kicked(reason.to_owned()));
            return true;
        }
        p.locals.remove(i);
        if i < p.names.len() {
            p.names.remove(i);
        }
        let link = p.link;
        let why = DisconnectReason::Kicked(reason.to_owned());
        if let Some(link) = link {
            self.send(
                link,
                Channel::GAME_RPC,
                &SessionMsg::PlayerRemoved {
                    local: player.local,
                    reason: why.clone(),
                },
            );
        }
        self.events
            .push_back(ServerSessionEvent::PlayerLeft { player, reason: why });
        true
    }

    /// Removes every player of a peer and disconnects it.
    pub fn kick_peer(&mut self, peer: PeerId, reason: &str) -> bool {
        if !self.peers.contains_key(&peer) {
            return false;
        }
        self.disconnect_peer(peer, DisconnectReason::Kicked(reason.to_owned()));
        true
    }

    /// Starts a travel to `level` (spec 77 § 6.2). The server's level counts as loaded at once (the
    /// driver loads it before the next update); every peer goes to `Loading`.
    pub fn travel(&mut self, level: &str) -> u32 {
        self.travel_id = self.travel_id.wrapping_add(1).max(1);
        self.level = level.to_owned();
        let travel_id = self.travel_id;
        tracing::info!(%level, travel_id, "net session: travel");
        self.events.push_back(ServerSessionEvent::TravelStarted {
            level: level.to_owned(),
            travel_id,
        });
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            let Some(p) = self.peers.get(&peer) else {
                continue;
            };
            match p.state {
                PeerState::Loading | PeerState::Joining | PeerState::InGame => {
                    if let Some(link) = p.link {
                        // The keep list is written when the driver takes the message.
                        self.travel_slots
                            .push((self.out.len(), level.to_owned(), travel_id));
                        self.out.push(Outgoing {
                            link,
                            channel: Channel::GAME_RPC,
                            bytes: Vec::new(),
                        });
                        if let Some(peer) = self.peers.get_mut(&peer) {
                            peer.last_sent_ms = self.now_ms;
                        }
                    }
                    if let Some(p) = self.peers.get_mut(&peer) {
                        p.welcomed_travel = travel_id;
                        // A fresh loading timeout even when the peer was already loading.
                        p.since_ms = self.now_ms;
                    }
                    self.set_state(peer, PeerState::Loading);
                }
                _ => {}
            }
        }
        self.events.push_back(ServerSessionEvent::TravelFinished {
            level: level.to_owned(),
        });
        travel_id
    }

    /// Enters draining: refuses new joins and disconnects every peer with `server_shutting_down`.
    pub fn drain(&mut self) {
        if self.draining {
            return;
        }
        self.draining = true;
        let links: Vec<LinkId> = self.pending.keys().copied().collect();
        for link in links {
            self.refuse(link, DisconnectReason::ServerShuttingDown);
        }
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            self.disconnect_peer(peer, DisconnectReason::ServerShuttingDown);
        }
    }

    /// Starts a stream to a peer.
    pub fn send_stream(
        &mut self,
        peer: PeerId,
        kind: &str,
        bytes: Vec<u8>,
    ) -> Result<StreamId, StreamRefusal> {
        let p = self.peers.get_mut(&peer).ok_or(StreamRefusal::TooMany)?;
        p.streams.send(kind, bytes)
    }

    /// Sends a game session message to a peer (reliable) unless its link is backed up
    /// ([`Self::is_backpressured`]): `Err` hands the bytes back to retry on a later tick. Use
    /// it for traffic the game can hold back; [`Self::send_game`] always queues.
    pub fn try_send_game(&mut self, peer: PeerId, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        if self.link_of(peer).is_none() || self.is_backpressured(peer) {
            return Err(bytes);
        }
        self.send_game(peer, bytes);
        Ok(())
    }

    /// Sends a game session message to a peer (reliable). The driver holds it while the link is
    /// backed up; only a backlog past its cap ends the connection.
    pub fn send_game(&mut self, peer: PeerId, bytes: Vec<u8>) {
        if let Some(link) = self.link_of(peer) {
            self.send(link, Channel::GAME_RPC, &SessionMsg::Game { bytes });
        }
    }

    /// Sends a game session message to every `InGame` peer.
    pub fn broadcast_game(&mut self, bytes: &[u8]) -> usize {
        let targets: Vec<LinkId> = self
            .peers
            .values()
            .filter(|p| p.state == PeerState::InGame)
            .filter_map(|p| p.link)
            .collect();
        for link in &targets {
            self.send(
                *link,
                Channel::GAME_RPC,
                &SessionMsg::Game {
                    bytes: bytes.to_vec(),
                },
            );
        }
        targets.len()
    }

    /// Stream payload bytes sent to a peer so far.
    pub fn stream_bytes_sent(&self, peer: PeerId) -> u64 {
        self.peers.get(&peer).map_or(0, |p| p.streams.sent_bytes())
    }

    /// The per-tick byte budget of a peer's connection.
    pub fn budget_bytes_per_tick(&self, peer: PeerId) -> f64 {
        self.peers.get(&peer).map_or(0.0, |p| {
            f64::from(p.rate_bytes) / f64::from(self.opts.tick_hz.max(1))
        })
    }

    /// One server tick: timeouts, the authenticator, keepalives, streams. Call once per fixed tick
    /// after delivering received messages.
    pub fn update(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("net_session").entered();
        self.now_ms = now_ms;
        self.tick += 1;
        let interrupt_ms = self.settings.timeout_policy().interrupt_ms;
        let silent: Vec<_> = self
            .peers
            .values()
            .filter(|p| {
                !p.interrupted
                    && p.state != PeerState::Grace
                    && now_ms.saturating_sub(p.last_recv_ms) >= interrupt_ms
            })
            .filter_map(|p| p.link)
            .collect();
        for link in silent {
            self.set_interrupted(link, true);
        }
        for (_, left) in &mut self.closing {
            *left = left.saturating_sub(1);
        }
        self.poll_auth();
        self.apply_ticket_refusals();
        // Handshake timeouts. A link that has not sent `Hello` is not admitted: it gets no start
        // grace (spec 120 § 10.1), so a half-open link holds a `max_handshaking` slot for
        // `handshake_timeout_s` only. Admitted loaders get the grace from the transport policy.
        let hs = ms(self.settings.handshake_timeout_s);
        let expired: Vec<LinkId> = self
            .pending
            .iter()
            .filter(|(_, p)| match &p.auth {
                Some((_, _, started)) => now_ms.saturating_sub(*started) >= ms(self.settings.auth_timeout_s),
                None => now_ms.saturating_sub(p.since_ms) >= hs,
            })
            .map(|(l, _)| *l)
            .collect();
        for link in expired {
            let authing = self.pending.get(&link).is_some_and(|p| p.auth.is_some());
            if authing {
                self.refuse(
                    link,
                    DisconnectReason::AuthFailed("authenticator timed out".into()),
                );
            } else {
                self.refuse(link, DisconnectReason::Timeout);
            }
        }
        // Peer timeouts.
        let loading = ms(self.settings.loading_timeout_s);
        let joining = ms(self.settings.joining_timeout_s);
        let silence = self.settings.timeout_policy().timeout_ms;
        let grace = ms(self.settings.reconnect_grace_s);
        let mut lost = Vec::new();
        for (id, p) in &self.peers {
            let in_state = now_ms.saturating_sub(p.since_ms);
            let quiet = now_ms.saturating_sub(p.last_recv_ms);
            match p.state {
                PeerState::Loading if in_state >= loading => {
                    lost.push((
                        *id,
                        DisconnectReason::LevelLoadFailed("loading took longer than the timeout".into()),
                    ));
                }
                PeerState::Joining if in_state >= joining => lost.push((*id, DisconnectReason::Timeout)),
                PeerState::InGame if quiet >= silence => lost.push((*id, DisconnectReason::Timeout)),
                PeerState::Grace if in_state >= grace => lost.push((*id, DisconnectReason::Timeout)),
                _ => {}
            }
        }
        for (peer, reason) in lost {
            match self.peer_state(peer) {
                Some(PeerState::Grace) => {
                    tracing::info!(%peer, "net session: reconnect grace expired");
                    self.retire(peer, reason);
                }
                Some(PeerState::InGame) => {
                    if let Some(link) = self.link_of(peer) {
                        self.send(link, Channel::Unreliable, &SessionMsg::Goodbye(reason.clone()));
                    }
                    self.peer_lost(peer, reason);
                }
                _ => self.disconnect_peer(peer, reason),
            }
        }
        // Streams, then keepalives.
        let share = self.settings.stream_share;
        let idle_ms = ms(self.settings.stream_idle_timeout_s);
        let ids: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in ids {
            let budget = self.budget_bytes_per_tick(peer);
            let Some(p) = self.peers.get_mut(&peer) else {
                continue;
            };
            let Some(link) = p.link else {
                continue;
            };
            let mut events = Vec::new();
            if idle_ms > 0 {
                p.streams.expire_idle(now_ms, idle_ms, &mut events);
            }
            // Flow control: a backed-up or silent link gets no new stream chunks, so an outage
            // cannot fill the reliable window and end the connection (spec 120 § 10.3).
            let msgs = if p.backpressured || p.interrupted {
                Vec::new()
            } else {
                p.streams.pump(budget, share, &mut events)
            };
            for m in msgs {
                self.send(link, Channel::GAME_RPC, &m);
            }
            for event in events {
                self.events.push_back(ServerSessionEvent::Stream { peer, event });
            }
            let quiet = self
                .peers
                .get(&peer)
                .is_some_and(|p| now_ms.saturating_sub(p.last_sent_ms) >= KEEPALIVE_MS);
            if quiet {
                self.send(link, Channel::Unreliable, &SessionMsg::Keepalive);
            }
        }
    }

    fn poll_auth(&mut self) {
        let Some(auth) = self.auth.as_mut() else {
            return;
        };
        let mut done = Vec::new();
        for (link, p) in &self.pending {
            if let Some((id, _, _)) = &p.auth
                && let Some(result) = auth.poll(*id)
                && result != AuthResult::Pending
            {
                done.push((*link, result));
            }
        }
        for (link, result) in done {
            let Some(hello) = self
                .pending
                .get_mut(&link)
                .and_then(|p| p.auth.take())
                .map(|(_, h, _)| *h)
            else {
                continue;
            };
            match result {
                AuthResult::Accept {
                    platform_id,
                    reserved,
                } => self.after_auth(link, hello, platform_id, reserved),
                AuthResult::Reject(text) => self.refuse(link, DisconnectReason::AuthFailed(text.into())),
                AuthResult::Pending => {}
            }
        }
    }
}

/// Something that happened on the client (spec 77 § 9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientSessionEvent {
    /// The server is silent but the connection remains alive.
    PeerInterrupted,
    /// Traffic returned after an interruption.
    PeerRestored,
    /// The session timed out and `NetSession` reconnects by itself with the reconnect token
    /// (spec 120 § 10.3, ADR-8352): attempt `attempt` (from 1) started. The outage ends with one
    /// `PeerRestored` (after the new `Welcomed`) or one [`ClientSessionEvent::ReconnectFailed`]; no
    /// `Disconnected` fires while it tries.
    Reconnecting {
        /// The attempt, from 1.
        attempt: u32,
    },
    /// The automatic reconnect gave up (the window ended, or the server refused for good);
    /// `Disconnected` follows with the same reason.
    ReconnectFailed {
        /// Attempts started.
        attempts: u32,
        /// Why (`timeout` when the window ended).
        reason: DisconnectReason,
    },
    /// The state changed.
    StateChanged {
        /// Old state.
        from: ClientSessionState,
        /// New state.
        to: ClientSessionState,
    },
    /// The session ended.
    Disconnected {
        /// Why.
        reason: DisconnectReason,
    },
    /// The server welcomed this client.
    Welcomed(Box<Welcome>),
    /// Load this level, then call [`SessionClient::level_loaded`].
    LoadLevel {
        /// Level path.
        level: String,
        /// The travel it belongs to.
        travel_id: u32,
    },
    /// Travel began (the client despawns its replicated copies except persistent ones).
    TravelStarted {
        /// The new level.
        level: String,
        /// The travel id.
        travel_id: u32,
        /// The copies to keep (ADR-7259); `NetSession` drops the others at once. `None`: the
        /// server sent no list, and the despawn records remove them.
        keep: Option<crate::session::proto::TravelKeep>,
    },
    /// The client entered `InGame` after a travel.
    TravelFinished {
        /// The level.
        level: String,
    },
    /// Join snapshot item 1: despawn these level entities before the first `InGame` frame.
    JoinRemoved(Vec<u128>),
    /// Join snapshot item 2: spawn records.
    JoinRecords(Vec<u8>),
    /// One of this client's local players was removed.
    PlayerRemoved {
        /// Its local index.
        local: u8,
        /// Why.
        reason: DisconnectReason,
    },
    /// A stream event.
    Stream(StreamEvent),
    /// Remote admin output.
    AdminOutput(Vec<String>),
    /// A game session message.
    Game(Vec<u8>),
}

/// The client's session core.
#[derive(Debug)]
pub struct SessionClient {
    settings: SessionSettings,
    state: ClientSessionState,
    since_ms: u64,
    now_ms: u64,
    last_recv_ms: u64,
    interrupted: bool,
    last_sent_ms: u64,
    hello: Option<Hello>,
    welcome: Option<Welcome>,
    level: String,
    travel_id: u32,
    traveled: bool,
    goodbye: Option<(DisconnectReason, u8)>,
    close: bool,
    rate_bytes: u32,
    streams: StreamLink,
    out: Vec<(Channel, Vec<u8>)>,
    events: VecDeque<ClientSessionEvent>,
    /// The driver's flow-control signal ([`SessionClient::set_backpressure`]).
    backpressured: bool,
    /// Dev builds: this process's registered protocol types, compared with a server's list.
    protocol_types: Vec<(String, u64)>,
}

/// Marks the server's type list in a dev `protocol_mismatch` detail (spec 77 § 4 item 2).
pub const PROTOCOL_TYPES_TAG: &str = "\ntypes: ";

/// The dev server's type list for a `protocol_mismatch` detail: [`PROTOCOL_TYPES_TAG`], then
/// `path=fingerprint` entries (32-bit hex) joined by `;`, cut to `room` bytes (a cut list ends in
/// `;+N`).
pub fn protocol_types_line(types: &[(String, u64)], room: usize) -> String {
    let mut line = String::from(PROTOCOL_TYPES_TAG);
    if room <= line.len() + 8 {
        return String::new();
    }
    for (i, (name, fp)) in types.iter().enumerate() {
        let entry = format!("{}{name}={:08x}", if i == 0 { "" } else { ";" }, *fp as u32);
        let rest = types.len() - i;
        // Keep room for the `;+N` tail of a cut list.
        if line.len() + entry.len() + 12 > room {
            line.push_str(&format!(";+{rest}"));
            return line;
        }
        line.push_str(&entry);
    }
    line
}

/// A dev client's reading of a `protocol_mismatch` detail that carries the server's type list:
/// the hashes line, then `differing types:` with each differing registered type and whether it is
/// "missing on client", "missing on server" or "different", sorted by name. A detail without the
/// list comes back unchanged.
pub fn explain_protocol_mismatch(detail: &str, own: &[(String, u64)]) -> String {
    let Some((head, list)) = detail.split_once(PROTOCOL_TYPES_TAG) else {
        return detail.to_owned();
    };
    let mut server: BTreeMap<&str, u32> = BTreeMap::new();
    let mut cut = false;
    for entry in list.split(';') {
        if entry.starts_with('+') {
            cut = true;
            continue;
        }
        if let Some((name, fp)) = entry.rsplit_once('=')
            && let Ok(fp) = u32::from_str_radix(fp, 16)
        {
            server.insert(name, fp);
        }
    }
    let mine: BTreeMap<&str, u32> = own.iter().map(|(n, fp)| (n.as_str(), *fp as u32)).collect();
    let mut rows: Vec<String> = Vec::new();
    for (name, fp) in &server {
        match mine.get(name) {
            None => rows.push(format!("{name} (missing on client)")),
            Some(m) if m != fp => rows.push(format!("{name} (different)")),
            Some(_) => {}
        }
    }
    if !cut {
        for name in mine.keys().filter(|n| !server.contains_key(*n)) {
            rows.push(format!("{name} (missing on server)"));
        }
    }
    rows.sort();
    let mut out = head.to_owned();
    if rows.is_empty() {
        out.push_str("\ndiffering types: none by name (the registration order differs)");
    } else {
        out.push_str("\ndiffering types: ");
        out.push_str(&rows.join(", "));
    }
    if cut {
        out.push_str(" (the server's list was cut)");
    }
    let max = crate::session::proto::MAX_DETAIL_BYTES;
    if out.len() > max {
        let mut end = max;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

impl SessionClient {
    /// A disconnected client core.
    pub fn new(settings: SessionSettings) -> Self {
        Self {
            settings,
            state: ClientSessionState::Disconnected(None),
            since_ms: 0,
            now_ms: 0,
            last_recv_ms: 0,
            interrupted: false,
            last_sent_ms: 0,
            hello: None,
            welcome: None,
            level: String::new(),
            travel_id: 0,
            traveled: false,
            goodbye: None,
            close: false,
            rate_bytes: 49_152,
            streams: StreamLink::default(),
            out: Vec::new(),
            events: VecDeque::new(),
            protocol_types: Vec::new(),
            backpressured: false,
        }
    }

    /// The driver's flow-control signal: `true` while the reliable send window to the server is
    /// backed up. Streams pause until it clears; nothing is dropped (spec 120 § 10.3).
    pub fn set_backpressure(&mut self, backpressured: bool) {
        self.backpressured = backpressured;
    }

    /// True while the link to the server is backed up or interrupted: hold back optional
    /// reliable traffic.
    pub fn is_backpressured(&self) -> bool {
        self.backpressured || self.interrupted
    }

    /// Whether the server is interrupted, independently of loading/joining/in-game phase.
    pub fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Changes interruption once per outage without resetting the lifecycle deadline.
    pub fn set_interrupted(&mut self, interrupted: bool) {
        if self.interrupted != interrupted {
            self.interrupted = interrupted;
            self.events.push_back(if interrupted {
                ClientSessionEvent::PeerInterrupted
            } else {
                ClientSessionEvent::PeerRestored
            });
        }
    }

    /// Records validated transport activity, including empty keepalives.
    pub fn note_link_activity(&mut self, at_ms: u64) {
        if at_ms > self.last_recv_ms {
            self.last_recv_ms = at_ms;
            self.set_interrupted(false);
        }
    }

    /// The state.
    pub fn state(&self) -> &ClientSessionState {
        &self.state
    }

    /// The welcome of the current (or last) session.
    pub fn welcome(&self) -> Option<&Welcome> {
        self.welcome.as_ref()
    }

    /// The session peer id, once welcomed.
    pub fn peer_id(&self) -> Option<PeerId> {
        self.welcome.as_ref().map(|w| w.peer_id)
    }

    /// The level the client is in (or loading).
    pub fn level(&self) -> &str {
        &self.level
    }

    /// The current travel id.
    pub fn travel_id(&self) -> u32 {
        self.travel_id
    }

    /// The token that resumes the last session's slot.
    pub fn reconnect_token(&self) -> Option<[u8; RECONNECT_TOKEN_BYTES]> {
        self.welcome.as_ref().map(|w| w.reconnect_token)
    }

    /// The next event.
    pub fn poll_event(&mut self) -> Option<ClientSessionEvent> {
        self.events.pop_front()
    }

    /// Takes the outgoing messages.
    pub fn take_outgoing(&mut self) -> Vec<(Channel, Vec<u8>)> {
        std::mem::take(&mut self.out)
    }

    /// True once: the driver must close the transport.
    pub fn take_close(&mut self) -> bool {
        std::mem::take(&mut self.close)
    }

    fn set_state(&mut self, to: ClientSessionState) {
        if self.state != to {
            let from = std::mem::replace(&mut self.state, to.clone());
            self.since_ms = self.now_ms;
            self.events
                .push_back(ClientSessionEvent::StateChanged { from, to });
        }
    }

    fn send(&mut self, channel: Channel, msg: &SessionMsg) {
        match msg.encode() {
            Ok(bytes) => {
                self.out.push((channel, bytes));
                self.last_sent_ms = self.now_ms;
            }
            Err(e) => tracing::warn!(error = %e, "net session: message too large to send"),
        }
    }

    fn end(&mut self, reason: DisconnectReason) {
        self.interrupted = false;
        let mut events = Vec::new();
        self.streams.fail_all(reason.code(), &mut events);
        for e in events {
            self.events.push_back(ClientSessionEvent::Stream(e));
        }
        self.goodbye = None;
        self.close = true;
        self.set_state(ClientSessionState::Disconnected(Some(reason.clone())));
        self.events.push_back(ClientSessionEvent::Disconnected { reason });
    }

    /// Starts connecting with this `Hello` (the driver opens the transport). Refused unless
    /// disconnected.
    pub fn connect(&mut self, hello: Hello, now_ms: u64) -> bool {
        if !matches!(self.state, ClientSessionState::Disconnected(_)) {
            return false;
        }
        self.now_ms = now_ms;
        // 0 means the server's default, on this side too: not one byte per second.
        if hello.client_rate_bytes > 0 {
            self.rate_bytes = hello.client_rate_bytes;
        }
        self.hello = Some(hello);
        self.close = false;
        self.traveled = false;
        self.set_state(ClientSessionState::Connecting);
        true
    }

    /// The transport is up: send `Hello`.
    pub fn link_up(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        if self.state != ClientSessionState::Connecting {
            return;
        }
        self.last_recv_ms = now_ms;
        if let Some(hello) = self.hello.clone() {
            self.send(Channel::GAME_RPC, &SessionMsg::Hello(Box::new(hello)));
        }
        self.set_state(ClientSessionState::Handshaking);
    }

    /// The transport went down.
    pub fn link_down(&mut self, reason: DisconnectReason, now_ms: u64) {
        self.now_ms = now_ms;
        match &self.state {
            ClientSessionState::Disconnected(_) => {}
            ClientSessionState::Disconnecting => {
                let r = self
                    .goodbye
                    .take()
                    .map_or(DisconnectReason::ClientQuit, |(r, _)| r);
                self.end(r);
            }
            _ => self.end(reason),
        }
    }

    /// Leaves: goodbyes (3 copies, one per update), then `Disconnected(reason)`.
    pub fn disconnect(&mut self, reason: DisconnectReason, now_ms: u64) {
        self.now_ms = now_ms;
        match self.state {
            ClientSessionState::Disconnected(_) | ClientSessionState::Disconnecting => {}
            ClientSessionState::Connecting => self.end(reason),
            _ => {
                self.goodbye = Some((reason, GOODBYE_COPIES));
                self.set_state(ClientSessionState::Disconnecting);
            }
        }
    }

    /// The level of `travel_id` finished loading: sends `LevelReady`. A stale travel id is ignored.
    pub fn level_loaded(&mut self, travel_id: u32, now_ms: u64) {
        self.now_ms = now_ms;
        if self.state == ClientSessionState::Loading && travel_id == self.travel_id {
            self.send(Channel::GAME_RPC, &SessionMsg::LevelReady { travel_id });
            self.set_state(ClientSessionState::Joining);
        }
    }

    /// The level failed to load.
    pub fn level_failed(&mut self, detail: &str, now_ms: u64) {
        self.now_ms = now_ms;
        if self.state == ClientSessionState::Loading {
            let reason = DisconnectReason::LevelLoadFailed(detail.to_owned());
            self.send(Channel::GAME_RPC, &SessionMsg::Goodbye(reason.clone()));
            self.end(reason);
        }
    }

    /// Starts a stream to the server.
    pub fn send_stream(&mut self, kind: &str, bytes: Vec<u8>) -> Result<StreamId, StreamRefusal> {
        self.streams.send(kind, bytes)
    }

    /// Sends a game session message (reliable).
    pub fn send_game(&mut self, bytes: Vec<u8>) {
        if self.state == ClientSessionState::InGame {
            self.send(Channel::GAME_RPC, &SessionMsg::Game { bytes });
        }
    }

    /// Sends a remote admin message.
    pub fn send_admin(&mut self, msg: SessionMsg) {
        if matches!(
            msg,
            SessionMsg::AdminLogin { .. } | SessionMsg::AdminCommand { .. }
        ) && self.state == ClientSessionState::InGame
        {
            self.send(Channel::GAME_RPC, &msg);
        }
    }

    /// Sets this process's registered protocol types (`runtime::protocol_types`). A dev build
    /// sets them; a `protocol_mismatch` refusal that carries the server's list then names the
    /// types that differ (spec 77 § 4 item 2).
    pub fn set_protocol_types(&mut self, types: Vec<(String, u64)>) {
        self.protocol_types = types;
    }

    /// Bytes arrived from the server.
    pub fn receive(&mut self, bytes: &[u8], now_ms: u64) {
        self.now_ms = now_ms;
        self.last_recv_ms = now_ms;
        if matches!(self.state, ClientSessionState::Disconnected(_)) {
            return;
        }
        let Ok(msg) = SessionMsg::decode(bytes) else {
            self.end(DisconnectReason::MalformedPacket);
            return;
        };
        self.set_interrupted(false);
        match msg {
            SessionMsg::Welcome(w) if self.state == ClientSessionState::Handshaking => {
                self.level.clone_from(&w.level);
                self.travel_id = w.travel_id;
                self.welcome = Some((*w).clone());
                self.events.push_back(ClientSessionEvent::Welcomed(w));
                self.enter_loading();
            }
            SessionMsg::Refused(DisconnectReason::ProtocolMismatch(detail))
                if !self.protocol_types.is_empty() =>
            {
                let named = explain_protocol_mismatch(&detail, &self.protocol_types);
                self.end(DisconnectReason::ProtocolMismatch(named));
            }
            SessionMsg::Refused(reason) => self.end(reason),
            SessionMsg::Goodbye(reason) => {
                if self.state != ClientSessionState::Disconnecting {
                    self.end(reason);
                }
            }
            SessionMsg::TravelBegin {
                level,
                travel_id,
                keep,
            } => {
                if matches!(
                    self.state,
                    ClientSessionState::Loading | ClientSessionState::Joining | ClientSessionState::InGame
                ) {
                    self.level = level.clone();
                    self.travel_id = travel_id;
                    self.traveled = true;
                    self.events.push_back(ClientSessionEvent::TravelStarted {
                        level,
                        travel_id,
                        keep,
                    });
                    // Restarts loading even when already loading (spec 77 § 6.2).
                    self.enter_loading();
                }
            }
            SessionMsg::JoinRemoved { guids } if self.state == ClientSessionState::Joining => {
                self.events.push_back(ClientSessionEvent::JoinRemoved(guids));
            }
            SessionMsg::JoinRecords { bytes } if self.state == ClientSessionState::Joining => {
                self.events.push_back(ClientSessionEvent::JoinRecords(bytes));
            }
            SessionMsg::InGame { tick } if self.state == ClientSessionState::Joining => {
                self.send(Channel::GAME_RPC, &SessionMsg::InGame { tick });
                self.set_state(ClientSessionState::InGame);
                if self.traveled {
                    self.events.push_back(ClientSessionEvent::TravelFinished {
                        level: self.level.clone(),
                    });
                }
            }
            SessionMsg::PlayerRemoved { local, reason } => {
                self.events
                    .push_back(ClientSessionEvent::PlayerRemoved { local, reason });
            }
            m @ (SessionMsg::StreamBegin { .. } | SessionMsg::StreamChunk { .. }) => {
                let mut events = Vec::new();
                let ok = self.streams.receive(&m, &mut events).is_ok();
                for e in events {
                    self.events.push_back(ClientSessionEvent::Stream(e));
                }
                if !ok {
                    self.end(DisconnectReason::MalformedPacket);
                }
            }
            SessionMsg::AdminOutput { lines } => {
                self.events.push_back(ClientSessionEvent::AdminOutput(lines))
            }
            SessionMsg::Game { bytes } => self.events.push_back(ClientSessionEvent::Game(bytes)),
            _ => {}
        }
    }

    fn enter_loading(&mut self) {
        self.set_state(ClientSessionState::Loading);
        // Re-entering Loading for a new travel resets the timeout too.
        self.since_ms = self.now_ms;
        self.events.push_back(ClientSessionEvent::LoadLevel {
            level: self.level.clone(),
            travel_id: self.travel_id,
        });
    }

    /// One client tick: goodbyes, timeouts, keepalives, streams.
    pub fn update(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
        if let Some((reason, left)) = self.goodbye.take() {
            if left > 0 {
                self.send(Channel::Unreliable, &SessionMsg::Goodbye(reason.clone()));
                self.goodbye = Some((reason, left - 1));
            } else {
                self.end(reason);
            }
            return;
        }
        let in_state = now_ms.saturating_sub(self.since_ms);
        let quiet = now_ms.saturating_sub(self.last_recv_ms);
        if matches!(
            self.state,
            ClientSessionState::Loading | ClientSessionState::Joining | ClientSessionState::InGame
        ) && quiet >= self.settings.timeout_policy().interrupt_ms
        {
            self.set_interrupted(true);
        }
        let timeout = match self.state {
            ClientSessionState::Connecting => in_state >= ms(self.settings.connect_timeout_s),
            // The server may hold a Hello for its authenticator up to `auth_timeout_s` before it
            // answers, so the client waits for both (ADR-1663). No start grace before `Welcome`:
            // the grace starts at admission (spec 120 § 10.1).
            ClientSessionState::Handshaking => {
                in_state >= ms(self.settings.handshake_timeout_s) + ms(self.settings.auth_timeout_s)
            }
            ClientSessionState::Loading => {
                if in_state >= ms(self.settings.loading_timeout_s) {
                    self.level_failed("loading took longer than the timeout", now_ms);
                    return;
                }
                false
            }
            ClientSessionState::Joining => in_state >= ms(self.settings.joining_timeout_s),
            ClientSessionState::InGame => quiet >= self.settings.timeout_policy().timeout_ms,
            _ => false,
        };
        if timeout {
            self.end(DisconnectReason::Timeout);
            return;
        }
        if matches!(
            self.state,
            ClientSessionState::Loading | ClientSessionState::Joining | ClientSessionState::InGame
        ) {
            let mut events = Vec::new();
            let idle_ms = ms(self.settings.stream_idle_timeout_s);
            if idle_ms > 0 {
                self.streams.expire_idle(now_ms, idle_ms, &mut events);
            }
            let budget = f64::from(self.rate_bytes)
                / f64::from(self.welcome.as_ref().map_or(60, |w| w.tick_hz.max(1)));
            // Flow control: no new stream chunks while the link is backed up or silent.
            if !self.is_backpressured() {
                for m in self.streams.pump(budget, self.settings.stream_share, &mut events) {
                    self.send(Channel::GAME_RPC, &m);
                }
            }
            for e in events {
                self.events.push_back(ClientSessionEvent::Stream(e));
            }
            if now_ms.saturating_sub(self.last_sent_ms) >= KEEPALIVE_MS {
                self.send(Channel::Unreliable, &SessionMsg::Keepalive);
            }
        }
    }
}

/// Which side of a refused join is older, from two manifests' game versions, then protocol
/// versions; same versions with other content is `content_differs` (ADR-7985).
fn manifest_side(client: &ContentManifest, server: &ContentManifest) -> MismatchSide {
    let stamp = |m: &ContentManifest| {
        VersionStamp::new(
            crate::payload::REGIA_NET_WIRE_VERSION,
            &m.game_version,
            m.protocol_version,
            m.content_hash,
        )
    };
    MismatchSide::between(Some(&stamp(client)), Some(&stamp(server)))
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use crate::session::bans::BanKind;
    use std::net::{IpAddr, Ipv4Addr};

    fn server_core() -> (SessionServer, ContentManifest) {
        let manifest = ContentManifest::build(
            "0.5.0",
            "1.0.0",
            crate::settings::NET_PROTOCOL_VERSION,
            0x1234,
            &[],
        );
        let server = SessionServer::new(
            SessionSettings::default(),
            HostOptions::default(),
            manifest.clone(),
            [7u8; 32],
        );
        (server, manifest)
    }

    fn hello_bytes(manifest: &ContentManifest) -> Vec<u8> {
        let hello = Hello {
            protocol_version: crate::settings::NET_PROTOCOL_VERSION,
            protocol_hash: manifest.protocol_hash,
            manifest: manifest.clone(),
            player_names: vec!["Tester".to_owned()],
            password: None,
            auth: None,
            client_rate_bytes: 0,
            reconnect_token: None,
        };
        SessionMsg::Hello(Box::new(hello))
            .encode()
            .expect("hello encodes")
    }

    /// Drives one join attempt from a fixed address at `now_unix_s` and returns the first
    /// session event it produced.
    fn join_at(server: &mut SessionServer, bytes: &[u8], now_unix_s: u64) -> Option<ServerSessionEvent> {
        server.set_unix_now_s(now_unix_s);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 4000);
        server.link_up(LinkId(1), addr, 0);
        server.receive(LinkId(1), bytes, 0);
        server.poll_event()
    }

    /// Audit fix-net3-determinism M2 (ADR-4772): the join-time ban check must read the clock the
    /// host injected (`SessionServer::set_unix_now_s`), not the system clock. With the old read
    /// of `unix_now_s()` (the real wall, ~1.76e9 in 2026) a ban expiring at 1 000 looks expired
    /// and the first join here is accepted, so this test fails before the fix.
    #[test]
    fn join_ban_follows_the_supplied_clock() {
        let (mut server, manifest) = server_core();
        server
            .bans_mut()
            .add(BanKind::Address, "203.0.113.7", Some(1_000), "griefing", 0)
            .expect("ban");
        let bytes = hello_bytes(&manifest);

        let refused = join_at(&mut server, &bytes, 999).expect("an event");
        assert!(
            matches!(
                refused,
                ServerSessionEvent::Refused {
                    reason: DisconnectReason::Banned(_),
                    ..
                }
            ),
            "a live ban refuses the join: {refused:?}"
        );

        // The same hello after the expiry is accepted — driven only by the injected clock.
        let welcomed = join_at(&mut server, &bytes, 1_001).expect("an event");
        assert!(
            matches!(welcomed, ServerSessionEvent::PeerWelcomed { .. }),
            "the expired ban accepts the join: {welcomed:?}"
        );
    }

    /// A join that would need `PeerId(u32::MAX)` is refused. That id is never issued.
    #[test]
    fn a_join_past_the_peer_id_space_is_refused() {
        let (mut server, manifest) = server_core();
        server.next_peer = u32::MAX;
        let event = join_at(&mut server, &hello_bytes(&manifest), 1_000).expect("an event");
        assert!(
            matches!(
                event,
                ServerSessionEvent::Refused {
                    reason: DisconnectReason::ServerFull,
                    ..
                }
            ),
            "{event:?}"
        );
    }

    fn refusal_detail(event: Option<ServerSessionEvent>) -> String {
        match event {
            Some(ServerSessionEvent::Refused {
                reason:
                    DisconnectReason::VersionMismatch(d)
                    | DisconnectReason::ProtocolMismatch(d)
                    | DisconnectReason::ContentMismatch(d),
                ..
            }) => d,
            other => panic!("not a version refusal: {other:?}"),
        }
    }

    fn hello_with_version(manifest: &ContentManifest, protocol_version: u32) -> Vec<u8> {
        let hello = Hello {
            protocol_version,
            protocol_hash: manifest.protocol_hash,
            manifest: manifest.clone(),
            player_names: vec!["Tester".to_owned()],
            password: None,
            auth: None,
            client_rate_bytes: 0,
            reconnect_token: None,
        };
        SessionMsg::Hello(Box::new(hello))
            .encode()
            .expect("hello encodes")
    }

    /// A client whose oldest protocol is newer than the server's newest: the server is older
    /// (spec 120 § 12.2, ADR-7985).
    #[test]
    fn a_session_refusal_says_the_server_is_older() {
        let (mut server, manifest) = server_core();
        let bytes = hello_with_version(&manifest, crate::settings::NET_PROTOCOL_VERSION + 1);
        let detail = refusal_detail(join_at(&mut server, &bytes, 1_000));
        assert!(
            detail.contains("the server is older than this build") && detail.ends_with("(server_older)"),
            "{detail}"
        );
        assert_eq!(MismatchSide::in_detail(&detail), Some(MismatchSide::ServerOlder));
    }

    /// An older release with other content: the client is told to update the game; the same
    /// release with other content is told to install the server's content.
    #[test]
    fn a_session_content_refusal_names_the_older_side() {
        let (mut server, manifest) = server_core();
        let mut older = manifest.clone();
        older.game_version = "0.9.0".into();
        older.content_hash ^= 1;
        let detail = refusal_detail(join_at(&mut server, &hello_bytes(&older), 1_000));
        assert!(
            detail.contains("update the game") && detail.ends_with("(client_older)"),
            "{detail}"
        );

        let (mut server, manifest) = server_core();
        let mut newer = manifest.clone();
        newer.game_version = "1.1.0".into();
        newer.content_hash ^= 1;
        let detail = refusal_detail(join_at(&mut server, &hello_bytes(&newer), 1_000));
        assert!(detail.ends_with("(server_older)"), "{detail}");

        // The same release keeps the plain item list.
        let (mut server, manifest) = server_core();
        let mut other = manifest.clone();
        other.content_hash ^= 1;
        let detail = refusal_detail(join_at(&mut server, &hello_bytes(&other), 1_000));
        assert_eq!(detail, "content_hash: different");
    }

    /// Another protocol hash (other registered types) from an older release: client older.
    #[test]
    fn a_session_protocol_refusal_names_the_older_side() {
        let (mut server, manifest) = server_core();
        let mut older = manifest.clone();
        older.game_version = "0.9.0".into();
        older.protocol_hash ^= 1;
        let detail = refusal_detail(join_at(&mut server, &hello_bytes(&older), 1_000));
        assert!(detail.ends_with("(client_older)"), "{detail}");
    }
}
