// SPDX-License-Identifier: MPL-2.0
//! `NetLink`: runs the replication runtime of an app over a real datagram transport (ADR-2039).
//!
//! [`crate::NetPlugin`] fills the inbox and outbox of [`NetNode`](crate::runtime::NetNode);
//! something must move those frames over a socket. [`NetLink`] is that something for a whole app:
//! one resource that owns a [`NetServer`] or a [`NetClient`] (raw UDP through [`UdpTransport`],
//! the in-memory network through [`crate::LoopbackSocket`], or Steam's relay through
//! `regia_steam::SteamSocket`; any [`DatagramSocket`]) plus the [`crate::bridge`] that cuts frames
//! into parts. [`NetLinkPlugin`] adds two systems:
//!
//! | System | Schedule | Work |
//! |---|---|---|
//! | `net_link_receive` | `PreUpdate`, before [`NetSet::Receive`] | the endpoint reads its socket; joins, leaves and frames reach the node |
//! | `net_link_flush` | `PostUpdate`, in [`NetSet::Flush`] | the node's frames and disconnects go to the endpoint, which sends; the report goes to [`NetAgentState`] |
//!
//! Encryption is the endpoints' own (ADR-1901): [`LinkOptions::crypto`] picks the mode, and
//! [`CryptoMode::Required`] seals loopback links too. A client that knows the server's key pins it
//! with [`LinkOptions::server_key`]; one that does not can trust it on first use with
//! [`LinkOptions::known_hosts`] (ADR-2416): the first connection to a host records its key in the
//! store, and a later connection that meets another key ends with `net_host_key_changed`.
//!
//! Time is a [`LinkClock`]: the wall clock for games, or a manual clock that advances a fixed step
//! per app update, so a test of several apps over real sockets has no sleeps and no timing budget.
//!
//! The link publishes its endpoint report under the label `"server"` or `"client"` (or
//! [`LinkOptions::label`]) every frame, so the agent methods `net.status`, `net.peers`,
//! `net.crypto` and `net.conditioner_set` see it without new methods.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use regia_app::{App, Plugin, PostUpdate, PreUpdate};
use regia_ecs::prelude::*;

use crate::bridge::{ClientBridge, NetFrame, ServerBridge};
use crate::capture::{CaptureError, CaptureTap, ReplayFeed, ReplayProgress, ReplaySocket};
use crate::client::{ClientConfig, ClientEvent, NetClient};
use crate::conditioner::ConditionerConfig;
use crate::crypto::{CryptoMode, ResumeCredential, ServerIdentity, ServerKey, secret_would_be_plain};
use crate::handshake::RESUME_TICKET_BYTES;
use crate::known_hosts::{FirstUsePin, KnownHostsError};
use crate::model::PeerId;
use crate::payload::NetGame;
use crate::plugin::NetSet;
use crate::predict::{ClientPrediction, expect_transport_rtt, set_transport_rtt};
use crate::reconnect::{ReconnectPolicy, ReconnectStep, Reconnector, seed_from};
use crate::runtime::ensure_resources;
use crate::server::{NetServer, ServerConfig, ServerEvent};
use crate::socket::{DatagramSocket, UdpTransport};
use crate::status::{NetAgentState, NetStatus, ReconnectStatus};

/// Most undrained [`LinkEvent`]s kept; older ones are dropped (a game that never reads them must
/// not grow without bound).
pub const MAX_LINK_EVENTS: usize = 1024;

/// A socket the link can own.
pub type LinkSocket = Box<dyn DatagramSocket + Send>;

/// Opens a fresh client socket of one transport kind (UDP, a Steam channel, a QUIC endpoint): a
/// reconnect after a timeout opens one per attempt (spec 120 § 10.3, ADR-8352). See
/// [`NetLink::connect_via`].
pub type LinkSocketOpener = std::sync::Arc<dyn Fn() -> std::io::Result<LinkSocket> + Send + Sync>;

/// Socket send errors, counted while the server is interrupted, after which a client with a
/// [`LinkSocketOpener`] stops waiting for the timeout and reconnects at once on a fresh socket:
/// its socket is dead (a phone's Wi-Fi socket after the switch to cellular), so the old
/// connection cannot come back by itself (ADR-8352).
pub const DEAD_SOCKET_SEND_ERRORS: u64 = 3;

/// Where the link's milliseconds come from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LinkClock {
    /// Milliseconds since the link started, from the monotonic clock.
    Wall,
    /// A counter that starts at `start_ms` and advances `step_us` microseconds after every flush:
    /// one app update is one step (16 667 us for 60 Hz). Tests and lockstep tools use it.
    Manual {
        /// The first reading, in milliseconds.
        start_ms: u64,
        /// Microseconds added per app update.
        step_us: u64,
    },
}

/// How a link is set up.
#[derive(Debug, Clone)]
pub struct LinkOptions {
    /// Silence policy passed to both transport endpoints, clamped into range
    /// ([`TimeoutPolicy::sanitized`](crate::session::TimeoutPolicy::sanitized)).
    pub timeout: crate::session::TimeoutPolicy,
    /// When the link is encrypted (ADR-1901). `Required` seals loopback links too.
    pub crypto: CryptoMode,
    /// Client: the server's public key to pin. Server: unused.
    pub server_key: Option<ServerKey>,
    /// Server: its static key; `None` derives a fresh one each run.
    pub identity: Option<ServerIdentity>,
    /// Outbound network conditioner (off by default). Seeded, so a manual clock replays it.
    pub conditioner: ConditionerConfig,
    /// Server: peer capacity.
    pub max_peers: usize,
    /// Server: authoritative tick rate announced to clients.
    pub tick_rate_hz: u32,
    /// Client: display name sent with the join.
    pub name: String,
    /// Server or client password (`None` for an open server).
    pub password: Option<String>,
    /// The label the endpoint report is published under; empty means `"server"` or `"client"`.
    pub label: String,
    /// The clock.
    pub clock: LinkClock,
    /// Client: the known-hosts store to trust the server's key on first use (ADR-2416), usually
    /// `user://known_hosts.ron` (`crate::known_hosts::KNOWN_HOSTS_FILE`). `None` turns trust on
    /// first use off. Ignored when [`LinkOptions::server_key`] pins a key.
    pub known_hosts: Option<PathBuf>,
    /// Client: the id the store keeps the key under; empty means the server's address
    /// (`203.0.113.7:7777`). A game whose servers move between addresses passes a stable name.
    pub host_id: String,
    /// Server: how long a player whose connection timed out stays dormant, in milliseconds (spec
    /// 120 § 10.3, ADR-8298): its `PeerId`, entities, ownership and interest state wait for it to
    /// come back with [`NetLink::resume`]. `0` turns resume off (a timeout is a leave at once).
    pub resume_grace_ms: u64,
    /// The game content both ends must share (a cooked data table, a rules file, the session
    /// manifest's `content_hash`), `0` for none. The link always adds the app's replicated schema
    /// ([`crate::runtime::protocol_hash`]: message types, replicated components, predictors) and
    /// the server requires the result, so a client with other components or other content is
    /// refused with `protocol_mismatch` and a `content_differs` detail instead of joining
    /// (ADR-7985).
    pub content_hash: u64,
    /// Path MTU discovery on this endpoint's packets (spec 120 § 4.3, ADR-8300; server.toml
    /// `[net] mtu_probing`): sets [`crate::connection::LinkConfig::mtu_probing`] on the server's
    /// or the client's links. Off by default.
    pub mtu_probing: bool,
    /// Client: reconnect by itself after a timeout (spec 120 § 10.3, ADR-8352): fresh handshakes
    /// with the resume ticket, 1 s doubling to 8 s apart with jitter, for `window_ms` (default the
    /// server's 60 s resume grace), over the transport kind the link connected with. The game sees
    /// [`LinkEvent::Reconnecting`] per attempt, then [`LinkEvent::PeerRestored`] or, at the
    /// window's end, [`LinkEvent::ReconnectFailed`] and [`LinkEvent::Ended`].
    /// [`ReconnectPolicy::off`] keeps the old behavior (a timeout ends the client).
    pub reconnect: ReconnectPolicy,
}

/// Default [`LinkOptions::resume_grace_ms`]: 60 s, the session's reconnect grace (spec 77 § 7.4).
pub const DEFAULT_RESUME_GRACE_MS: u64 = 60_000;

impl Default for LinkOptions {
    fn default() -> Self {
        Self {
            timeout: crate::session::TimeoutPolicy::default(),
            crypto: CryptoMode::Auto,
            server_key: None,
            identity: None,
            conditioner: ConditionerConfig::default(),
            max_peers: 16,
            tick_rate_hz: 60,
            name: "player".into(),
            password: None,
            label: String::new(),
            clock: LinkClock::Wall,
            known_hosts: None,
            host_id: String::new(),
            resume_grace_ms: DEFAULT_RESUME_GRACE_MS,
            content_hash: 0,
            mtu_probing: false,
            reconnect: ReconnectPolicy {
                window_ms: DEFAULT_RESUME_GRACE_MS,
                ..ReconnectPolicy::default()
            },
        }
    }
}

/// What happened on the link, for the game (a join spawns an avatar, a leave removes it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A peer is silent but retains its connection (`SERVER` on a client).
    PeerInterrupted {
        /// The affected peer.
        peer: PeerId,
    },
    /// Authentic traffic returned (`SERVER` on a client).
    PeerRestored {
        /// The restored peer.
        peer: PeerId,
    },
    /// Server: a peer finished its handshake; the node now replicates to it.
    Joined {
        /// The peer.
        peer: PeerId,
        /// Its display name.
        name: String,
    },
    /// Server: a player's connection timed out and it is dormant (spec 120 § 10.3): its `PeerId`,
    /// entities and ownership wait for a resume until the grace ends (then [`LinkEvent::Left`]).
    /// Its entities carry `NetInterrupted` meanwhile. A game keeps its avatar.
    Dormant {
        /// The peer.
        peer: PeerId,
    },
    /// Server: a peer came back with its resume ticket and keeps its `PeerId` and entities (spec
    /// 120 § 10.3). The first frame it gets is the full state.
    Resumed {
        /// The peer.
        peer: PeerId,
        /// Its display name.
        name: String,
    },
    /// Server: a peer left or was removed.
    Left {
        /// The peer.
        peer: PeerId,
        /// Lowercase reason code.
        reason: String,
    },
    /// Client: the server accepted us.
    Connected {
        /// Our peer id.
        peer: PeerId,
        /// Whether the link is sealed.
        sealed: bool,
    },
    /// Client: the connection timed out and the link reconnects by itself (spec 120 § 10.3,
    /// ADR-8352): attempt `attempt` (1, 2, ...) started a fresh handshake with the resume ticket.
    /// The first attempt follows [`LinkEvent::PeerInterrupted`] of the same outage; the outage
    /// ends with one [`LinkEvent::PeerRestored`] (the same `PeerId`) or one
    /// [`LinkEvent::ReconnectFailed`]. No [`LinkEvent::Ended`] fires while it tries.
    Reconnecting {
        /// The attempt, from 1.
        attempt: u32,
    },
    /// Client: the automatic reconnect gave up (its window ended, or the server refused the
    /// attempt for good); [`LinkEvent::Ended`] follows with the same reason.
    ReconnectFailed {
        /// Attempts started.
        attempts: u32,
        /// Lowercase reason code: `timeout` when the window ended, else the refusal's code.
        reason: String,
    },
    /// Client: the connection ended (denied, timed out, kicked, closed).
    Ended {
        /// Lowercase reason code (`connect_timeout`, `server_key_mismatch`,
        /// `net_host_key_changed`, ...).
        reason: String,
    },
    /// Client: the server refused the join with `protocol_mismatch`; this names which side is
    /// older (spec 120 § 12.2, ADR-7985). Follows the [`LinkEvent::Ended`] of the refusal.
    VersionMismatch {
        /// `client_older` (update the game), `server_older`, `content_differs` or `unordered`.
        side: crate::version_probe::MismatchSide,
        /// `Some(true)` when the server is older, `Some(false)` when this client is.
        server_is_older: Option<bool>,
        /// The line to show the player.
        message: String,
    },
    /// Client: the server's key was trusted on first use and recorded in the known-hosts store
    /// (ADR-2416). `saved` is false when the store could not be written; the pin then lasts for
    /// this connection only.
    HostKeyPinned {
        /// The host id it is stored under.
        host: String,
        /// The key.
        key: ServerKey,
        /// Whether the store was written.
        saved: bool,
    },
    /// A link's connection grade changed (`SERVER` on a client; spec 120 § 13.1, ADR-8182).
    QualityChanged {
        /// The peer.
        peer: PeerId,
        /// The grade shown before.
        from: crate::quality::LinkGrade,
        /// The grade shown now.
        to: crate::quality::LinkGrade,
    },
}

/// Why a link could not start.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The link already has an endpoint.
    #[error("the link is already running")]
    AlreadyRunning,
    /// The socket could not be bound.
    #[error("bind failed: {0}")]
    Bind(String),
    /// The join data or a token field does not fit the handshake.
    #[error("join does not fit the handshake: {0}")]
    Join(String),
    /// A password would cross a plain link to a non-loopback address (`CryptoMode::Off`).
    #[error("a password needs encryption on a non-loopback link")]
    PasswordNeedsEncryption,
    /// The known-hosts store could not be read, or the host id is not valid (ADR-2416). A damaged
    /// store refuses the connection rather than trusting a key it cannot check.
    #[error("known hosts: {0}")]
    KnownHosts(KnownHostsError),
    /// A client capture could not be replayed ([`NetLink::replay`], ADR-8890): its game or
    /// protocol is another, or it holds no session.
    #[error("{0}")]
    Capture(CaptureError),
}

impl LinkError {
    /// Stable lowercase code (agent protocol, logs).
    pub fn code(&self) -> &'static str {
        match self {
            Self::AlreadyRunning => "link_running",
            Self::Bind(_) => "link_bind_failed",
            Self::Join(_) => "link_join_too_large",
            Self::PasswordNeedsEncryption => "link_password_plain",
            Self::KnownHosts(e) => e.code(),
            Self::Capture(e) => e.code(),
        }
    }
}

/// One endpoint behind the link, type-erased over the game.
trait LinkEnd: Send {
    fn receive(&mut self, world: &mut World, now_ms: u64, events: &mut Vec<LinkEvent>);
    fn flush(&mut self, world: &mut World, now_ms: u64);
    fn publish(&mut self, label: &str, state: &mut NetAgentState);
    fn status(&self) -> NetStatus;
    fn is_server(&self) -> bool;
    fn local_addr(&self) -> Option<SocketAddr>;
    fn server_key(&self) -> Option<ServerKey>;
    fn sealed(&self) -> bool;
    fn connected(&self) -> bool;
    fn set_conditioner(&mut self, config: ConditionerConfig);
    /// Server: removes `peer` with `kicked` (ADR-2440). False when unknown or a client.
    fn kick(&mut self, peer: PeerId) -> bool;
    /// Says goodbye: a server tells every peer it closes, a client that it leaves (ADR-2440).
    fn goodbye(&mut self, now_ms: u64);
    /// Server: each peer and its source address (ADR-2665 (4)). A client: none.
    fn peer_addrs(&self) -> Vec<(PeerId, SocketAddr)> {
        Vec::new()
    }
    /// Whether this server endpoint currently owns the peer.
    fn has_peer(&self, _peer: PeerId) -> bool {
        false
    }
    /// Server: removes `peer` with `banned`. False when unknown or a client.
    fn remove_banned(&mut self, peer: PeerId) -> bool {
        let _ = peer;
        false
    }
    /// Server: refuses handshakes from `ip`'s block (its IPv4 address, or its whole IPv6 /64,
    /// ADR-3283) until `expires_unix_s` (0 = permanent), or lifts that refusal (`ban = false`).
    /// A client: nothing.
    fn refuse_ip(&mut self, ip: std::net::IpAddr, ban: bool, reason: &str, expires_unix_s: u64) {
        let _ = (ip, ban, reason, expires_unix_s);
    }

    /// Server: refuses handshakes from exactly `ip` (no other address of its IPv6 /64) until
    /// `expires_unix_s` (0 = permanent). A client: nothing.
    fn refuse_ip_exact(&mut self, ip: std::net::IpAddr, reason: &str, expires_unix_s: u64) {
        let _ = (ip, reason, expires_unix_s);
    }

    /// Client: the resume ticket of the server's last accept (spec 120 § 10.3).
    fn resume_ticket(&self) -> Option<[u8; RESUME_TICKET_BYTES]> {
        None
    }

    /// Client: the resumption credential of the server's last accept (wire 6, ADR-8858): with it
    /// the next connect is a one-round-trip resume.
    fn resumption(&self) -> Option<ResumeCredential> {
        None
    }

    /// Server: installs the join ticket hook (`NetServer::set_ticket_check`, KI-NT-047). A
    /// client: nothing.
    fn set_ticket_check(&mut self, check: std::sync::Arc<dyn crate::ticket::JoinTicketCheck>) -> bool {
        let _ = check;
        false
    }

    /// Server: marks a relay's IP (`NetServer::add_relay_ip`, ADR-8183). A client: nothing.
    fn add_relay_ip(&mut self, ip: std::net::IpAddr) -> bool {
        let _ = ip;
        false
    }

    /// Client: the socket back, for a reconnect on the same transport (ADR-8352).
    fn into_socket(self: Box<Self>) -> Option<LinkSocket> {
        None
    }

    /// Client: connected, and the server silent past the interruption interval.
    fn interrupted(&self) -> bool {
        false
    }

    /// Socket send and receive errors so far.
    fn socket_errors(&self) -> u64 {
        0
    }

    /// Client: the connection or the attempt has ended.
    fn ended(&self) -> bool {
        false
    }

    /// Client: attaches a capture (ADR-8890). False on a server, which keeps nothing.
    fn set_capture(&mut self, tap: Option<CaptureTap>) -> bool {
        let _ = tap;
        false
    }

    /// Client made by [`NetLink::replay`]: how far the captured session got.
    fn replay_progress(&self) -> Option<ReplayProgress> {
        None
    }
}

struct ServerEnd<G: NetGame<Message = NetFrame>> {
    server: NetServer<G, LinkSocket>,
    bridge: ServerBridge,
    /// [`LinkOptions::content_hash`], until the first update folds it in with the app's schema.
    content: Option<u64>,
}

/// The content hash a link endpoint declares: the app's replicated schema with the game's
/// [`LinkOptions::content_hash`] (ADR-7985).
fn link_content_hash(world: &World, game_content: u64) -> u64 {
    crate::payload::combined_content_hash(crate::runtime::protocol_hash(world), game_content)
}

impl<G: NetGame<Message = NetFrame>> LinkEnd for ServerEnd<G> {
    fn set_ticket_check(&mut self, check: std::sync::Arc<dyn crate::ticket::JoinTicketCheck>) -> bool {
        self.server.set_ticket_check(check);
        true
    }

    fn add_relay_ip(&mut self, ip: std::net::IpAddr) -> bool {
        self.server.add_relay_ip(ip);
        true
    }

    fn receive(&mut self, world: &mut World, now_ms: u64, events: &mut Vec<LinkEvent>) {
        if let Some(content) = self.content.take() {
            self.server.set_content_hash(link_content_hash(world, content));
        }
        // This link's clock is the host wall clock; ban decisions read it through the injected
        // value (ADR-4772), never the system clock inside the server.
        self.server.set_unix_now_s(crate::session::unix_now_s());
        self.server.receive(now_ms);
        for event in self.bridge.pump_in(world, &mut self.server) {
            match event {
                ServerEvent::PeerInterrupted { peer } => events.push(LinkEvent::PeerInterrupted { peer }),
                ServerEvent::PeerRestored { peer } => events.push(LinkEvent::PeerRestored { peer }),
                ServerEvent::PeerJoined { peer, name, .. } => events.push(LinkEvent::Joined { peer, name }),
                ServerEvent::PeerDormant { peer, .. } => events.push(LinkEvent::Dormant { peer }),
                ServerEvent::PeerResumed { peer, name, .. } => events.push(LinkEvent::Resumed { peer, name }),
                ServerEvent::PeerLeft { peer, reason } => events.push(LinkEvent::Left {
                    peer,
                    reason: reason.as_str().to_owned(),
                }),
                ServerEvent::QualityChanged { peer, from, to } => {
                    events.push(LinkEvent::QualityChanged { peer, from, to })
                }
                ServerEvent::Message { .. } | ServerEvent::Rejected { .. } => {}
            }
        }
    }

    fn flush(&mut self, world: &mut World, now_ms: u64) {
        if let Some(content) = self.content.take() {
            self.server.set_content_hash(link_content_hash(world, content));
        }
        self.server.set_unix_now_s(crate::session::unix_now_s());
        self.bridge.pump_out(world, &mut self.server);
        self.server.flush(now_ms);
    }

    fn publish(&mut self, label: &str, state: &mut NetAgentState) {
        self.server.publish(label, state);
    }

    fn status(&self) -> NetStatus {
        self.server.status()
    }

    fn is_server(&self) -> bool {
        true
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.server.local_addr()
    }

    fn server_key(&self) -> Option<ServerKey> {
        self.server.server_key()
    }

    fn sealed(&self) -> bool {
        self.server.status().crypto.sealed_peers > 0
    }

    fn connected(&self) -> bool {
        true
    }

    fn set_conditioner(&mut self, config: ConditionerConfig) {
        self.server.set_conditioner(config);
    }

    fn kick(&mut self, peer: PeerId) -> bool {
        self.server.kick(peer, crate::handshake::DisconnectReason::Kicked)
    }

    fn goodbye(&mut self, now_ms: u64) {
        self.server.shutdown();
        self.server.flush(now_ms);
    }

    fn peer_addrs(&self) -> Vec<(PeerId, SocketAddr)> {
        self.server.session().peers().map(|p| (p.id, p.addr)).collect()
    }

    fn has_peer(&self, peer: PeerId) -> bool {
        self.server.session().peer(peer).is_some()
    }

    fn remove_banned(&mut self, peer: PeerId) -> bool {
        self.server.kick(peer, crate::handshake::DisconnectReason::Banned)
    }

    fn refuse_ip(&mut self, ip: std::net::IpAddr, ban: bool, reason: &str, expires_unix_s: u64) {
        if !ban {
            self.server.unban_ip(ip);
            return;
        }
        // `ban_ip` re-adds the server's injected clock to the duration; before the first
        // receive/flush that clock is the derived wall base, which can sit seconds behind the
        // wall. Stamp the wall here so a refusal issued right after hosting lasts its full
        // duration (audit fixrev-15-k net3-determinism L4, ADR-5444).
        self.server.set_unix_now_s(crate::session::unix_now_s());
        let seconds = expiry_to_seconds(expires_unix_s);
        self.server.ban_ip(ip, reason, seconds);
    }

    fn refuse_ip_exact(&mut self, ip: std::net::IpAddr, reason: &str, expires_unix_s: u64) {
        // The same wall stamp as `refuse_ip` (ADR-5444).
        self.server.set_unix_now_s(crate::session::unix_now_s());
        self.server
            .ban_ip_exact(ip, reason, expiry_to_seconds(expires_unix_s));
    }
}

/// Whole seconds from now until `expires_unix_s` (0 stays 0 = permanent), at least one.
fn expiry_to_seconds(expires_unix_s: u64) -> u64 {
    if expires_unix_s == 0 {
        0
    } else {
        expires_unix_s.saturating_sub(crate::session::unix_now_s()).max(1)
    }
}

struct ClientEndpoint<G: NetGame<Message = NetFrame>> {
    client: NetClient<G, LinkSocket>,
    bridge: ClientBridge,
    tofu: Option<Tofu>,
    /// [`LinkOptions::content_hash`], until the first update folds it in with the app's schema
    /// (before the connect request goes out).
    content: Option<u64>,
}

/// Trust on first use for one client connection (ADR-2416).
#[derive(Debug)]
struct Tofu {
    /// The store state of this connection.
    pin: FirstUsePin,
    /// The store refused the key after the connection was made, and the link closed it.
    refused: bool,
}

impl Tofu {
    /// After the server proved `key`: records it when the host was unknown.
    fn on_connected(&mut self, key: ServerKey) -> Result<Option<LinkEvent>, KnownHostsError> {
        Ok(self.pin.on_connected(key)?.map(|saved| LinkEvent::HostKeyPinned {
            host: self.pin.host().to_owned(),
            key,
            saved,
        }))
    }
}

impl<G: NetGame<Message = NetFrame>> LinkEnd for ClientEndpoint<G> {
    fn receive(&mut self, world: &mut World, now_ms: u64, events: &mut Vec<LinkEvent>) {
        if let Some(content) = self.content.take() {
            self.client.set_content_hash(link_content_hash(world, content));
        }
        self.client.receive(now_ms);
        let sealed = self.client.is_sealed();
        for event in self.bridge.pump_in(world, &mut self.client) {
            match event {
                ClientEvent::PeerInterrupted => {
                    events.push(LinkEvent::PeerInterrupted { peer: PeerId::SERVER })
                }
                ClientEvent::PeerRestored => events.push(LinkEvent::PeerRestored { peer: PeerId::SERVER }),
                ClientEvent::Connected { peer, .. } => {
                    let key = self.client.server_key();
                    match (self.tofu.as_mut(), key) {
                        (Some(tofu), Some(key)) => match tofu.on_connected(key) {
                            Ok(pinned) => {
                                events.push(LinkEvent::Connected { peer, sealed });
                                events.extend(pinned);
                            }
                            Err(e) => {
                                // Another process pinned a different key between our load and
                                // this connection, or the store went bad: do not stay connected
                                // to a key the store disagrees with.
                                tracing::warn!(target: "regia_net", error = %e, "known hosts refused the server's key");
                                if let Some(tofu) = self.tofu.as_mut() {
                                    tofu.refused = true;
                                }
                                self.client.disconnect();
                                events.push(LinkEvent::Ended {
                                    reason: e.code().to_owned(),
                                });
                            }
                        },
                        _ => events.push(LinkEvent::Connected { peer, sealed }),
                    }
                }
                // The store already ended this connection and said why.
                ClientEvent::Ended(_) if self.tofu.as_ref().is_some_and(|t| t.refused) => {}
                ClientEvent::Ended(end) => {
                    let pinned_by_store = self.tofu.as_ref().is_some_and(|t| t.pin.pinned_by_store());
                    let reason = if pinned_by_store && end == crate::client::ClientEnd::ServerKeyMismatch {
                        // The key pinned on an earlier visit is not the one the server proved.
                        "net_host_key_changed"
                    } else {
                        end.as_str()
                    };
                    events.push(LinkEvent::Ended {
                        reason: reason.to_owned(),
                    });
                }
                ClientEvent::VersionMismatch {
                    server_is_older,
                    side,
                    message,
                    ..
                } => events.push(LinkEvent::VersionMismatch {
                    side,
                    server_is_older,
                    message,
                }),
                // The client has already raised its input lead (ADR-8077).
                ClientEvent::QualityChanged { from, to } => events.push(LinkEvent::QualityChanged {
                    peer: PeerId::SERVER,
                    from,
                    to,
                }),
                ClientEvent::Message { .. } | ClientEvent::InputsLate { .. } => {}
            }
        }
        // Spec 73's lead control reads the transport's RTT when a game predicts. `rtt_ms` is 0
        // until the transport's first sample: that is "unknown", not a 0 ms round trip, so the
        // lead keeps its own estimate until then (ADR-4915).
        if self.client.is_connected() && world.contains_resource::<ClientPrediction>() {
            expect_transport_rtt(world);
        }
        if self.client.is_connected()
            && world.contains_resource::<ClientPrediction>()
            && let Some(link) = self.client.status().link
            && link.rtt_ms > 0.0
        {
            set_transport_rtt(world, link.rtt_ms, link.jitter_ms);
        }
    }

    fn flush(&mut self, world: &mut World, now_ms: u64) {
        if let Some(content) = self.content.take() {
            self.client.set_content_hash(link_content_hash(world, content));
        }
        self.bridge.pump_out(world, &mut self.client);
        self.client.flush(now_ms);
    }

    fn publish(&mut self, label: &str, state: &mut NetAgentState) {
        self.client.publish(label, state);
    }

    fn status(&self) -> NetStatus {
        self.client.status()
    }

    fn is_server(&self) -> bool {
        false
    }

    fn local_addr(&self) -> Option<SocketAddr> {
        self.client.status().local_addr.and_then(|a| a.parse().ok())
    }

    fn server_key(&self) -> Option<ServerKey> {
        self.client.server_key()
    }

    fn sealed(&self) -> bool {
        self.client.is_sealed()
    }

    fn connected(&self) -> bool {
        self.client.is_connected()
    }

    fn set_conditioner(&mut self, config: ConditionerConfig) {
        self.client.set_conditioner(config);
    }

    fn kick(&mut self, _peer: PeerId) -> bool {
        false
    }

    fn goodbye(&mut self, now_ms: u64) {
        self.client.disconnect();
        self.client.flush(now_ms);
    }

    fn resume_ticket(&self) -> Option<[u8; RESUME_TICKET_BYTES]> {
        self.client.resume_ticket()
    }

    fn resumption(&self) -> Option<ResumeCredential> {
        self.client.resumption()
    }

    fn into_socket(self: Box<Self>) -> Option<LinkSocket> {
        Some(self.client.into_socket())
    }

    fn interrupted(&self) -> bool {
        self.client.is_interrupted()
    }

    fn socket_errors(&self) -> u64 {
        self.client.totals().socket_errors
    }

    fn ended(&self) -> bool {
        matches!(self.client.state(), crate::client::ClientState::Ended(_))
    }

    fn set_capture(&mut self, tap: Option<CaptureTap>) -> bool {
        self.client.set_capture(tap);
        true
    }

    fn replay_progress(&self) -> Option<ReplayProgress> {
        self.client.replay_feed().map(ReplayFeed::progress)
    }
}

/// Opens a client endpoint for game `G` with its join data (the type-erased part of a resume).
type ClientOpener = std::sync::Arc<
    dyn Fn(
            LinkSocket,
            SocketAddr,
            &LinkOptions,
            Option<Rejoin>,
            Option<u64>,
        ) -> Result<Box<dyn LinkEnd>, LinkError>
        + Send
        + Sync,
>;

/// What a reconnect presents (spec 120 § 10.3): the resume ticket of the last accept and, when
/// that link was sealed and the server keeps tickets, its resumption credential. With the
/// credential the connect opens with a one-round-trip `ResumeInit` (wire 6, ADR-8858, ADR-8902);
/// the ticket alone, or a resume the server refuses, is the full handshake of three round trips
/// that presents the ticket (the fallback spec 120 § 10.3 names).
#[derive(Clone)]
struct Rejoin {
    ticket: [u8; RESUME_TICKET_BYTES],
    credential: Option<ResumeCredential>,
}

/// What a client link needs to come back as the same peer (spec 120 § 10.3).
#[derive(Clone)]
struct ResumePlan {
    server: SocketAddr,
    opts: LinkOptions,
    open: ClientOpener,
    /// The newest ticket the server gave this client.
    ticket: Option<[u8; RESUME_TICKET_BYTES]>,
    /// The resumption credential of the accept that carried `ticket` (wire 6, ADR-10088): every
    /// reconnect path presents it, so a server that keeps tickets resumes this client in one
    /// round trip. `None` on a plain link or when the server issued none. Clones share its
    /// flight count, so each attempt's flight is newer than the last (ADR-8858 A3).
    credential: Option<ResumeCredential>,
    /// Opens a fresh socket of the transport kind the link connected with; `None` reuses the
    /// endpoint's own socket (ADR-8352).
    opener: Option<LinkSocketOpener>,
}

impl ResumePlan {
    /// What the next connect presents; `None` before the first accept (a fresh join).
    fn rejoin(&self) -> Option<Rejoin> {
        self.ticket.map(|ticket| Rejoin {
            ticket,
            credential: self.credential.clone(),
        })
    }
}

/// One outage's automatic reconnect (ADR-8352).
#[derive(Debug, Clone)]
struct Reconnecting {
    timeline: Reconnector,
}

fn open_client<G: NetGame<Message = NetFrame>>(
    socket: LinkSocket,
    server: SocketAddr,
    opts: &LinkOptions,
    rejoin: Option<Rejoin>,
    connect_timeout_ms: Option<u64>,
    join: &G::JoinData,
) -> Result<Box<dyn LinkEnd>, LinkError> {
    let (server_key, tofu) = match (&opts.server_key, &opts.known_hosts) {
        (None, Some(path)) => {
            let id = if opts.host_id.trim().is_empty() {
                server.to_string()
            } else {
                opts.host_id.clone()
            };
            let (pin, pinned) = FirstUsePin::start(path, &id).map_err(LinkError::KnownHosts)?;
            (pinned, Some(Tofu { pin, refused: false }))
        }
        (key, _) => (*key, None),
    };
    let config = client_config(opts, server_key, rejoin, connect_timeout_ms);
    let client = NetClient::<G, LinkSocket>::connect(socket, server, config, join)
        .map_err(|e| LinkError::Join(e.to_string()))?;
    Ok(Box::new(ClientEndpoint {
        client,
        bridge: ClientBridge::new(),
        tofu,
        content: Some(opts.content_hash),
    }))
}

/// The client configuration a link's [`LinkOptions`] give (a connect, a resume and a replay share
/// it, so a replay cuts and times its packets as the captured client did).
fn client_config(
    opts: &LinkOptions,
    server_key: Option<ServerKey>,
    rejoin: Option<Rejoin>,
    connect_timeout_ms: Option<u64>,
) -> ClientConfig {
    let defaults = ClientConfig::default();
    ClientConfig {
        name: opts.name.clone(),
        password: opts.password.clone(),
        conditioner: opts.conditioner.clone(),
        crypto: opts.crypto,
        server_key,
        resume: rejoin.as_ref().map(|r| r.ticket),
        resumption: rejoin.and_then(|r| r.credential),
        link: crate::connection::LinkConfig {
            mtu_probing: opts.mtu_probing,
            ..crate::connection::LinkConfig::default()
        },
        connect_timeout_ms: connect_timeout_ms.unwrap_or(defaults.connect_timeout_ms),
        ..defaults
    }
    .with_timeouts(opts.timeout.sanitized())
}

/// A UDP socket for a client of `server`: an ephemeral port of its family (loopback for a loopback
/// server).
fn client_udp_socket(server: SocketAddr) -> Result<LinkSocket, LinkError> {
    client_udp_io(server).map_err(|e| LinkError::Bind(e.to_string()))
}

/// [`client_udp_socket`] as a [`LinkSocketOpener`] body.
fn client_udp_io(server: SocketAddr) -> std::io::Result<LinkSocket> {
    let bind: SocketAddr = if server.ip().is_loopback() {
        SocketAddr::new(server.ip(), 0)
    } else if server.is_ipv4() {
        SocketAddr::from(([0, 0, 0, 0], 0))
    } else {
        SocketAddr::from(([0u16; 8], 0))
    };
    Ok(Box::new(UdpTransport::bind(bind)?))
}

/// The app's transport: idle, a server, or a client. Insert it with [`NetLinkPlugin`]; start it
/// with [`NetLink::host`], [`NetLink::host_udp`], [`NetLink::connect`] or [`NetLink::connect_udp`].
#[derive(Resource)]
pub struct NetLink {
    end: Option<Mutex<Box<dyn LinkEnd>>>,
    label: String,
    clock: LinkClock,
    started: Instant,
    manual_us: u64,
    events: VecDeque<LinkEvent>,
    dropped_events: u64,
    frames: u64,
    /// The label of a stopped endpoint whose report the next flush removes.
    retired: Option<String>,
    /// Client: how to come back as the same peer (spec 120 § 10.3).
    resume: Option<ResumePlan>,
    /// Client: the automatic reconnect of the current outage (ADR-8352).
    reconnecting: Option<Reconnecting>,
    /// Client: the `PeerId` of the last accept.
    last_peer: Option<PeerId>,
    /// Client: socket errors when the current interruption began (dead-socket check).
    interrupted_errors: Option<u64>,
    /// Client: the reconnect gave up while its last attempt still ran; that attempt's own end is
    /// not reported again.
    reconnect_gave_up: bool,
    /// Client: the capture every client endpoint of this link writes to, across reconnects and
    /// resumes (ADR-8890).
    capture: Option<CaptureTap>,
    /// Client: the grade of the link to the server last reported in
    /// [`LinkEvent::QualityChanged`]; a reconnect's fresh connection reports the change back
    /// from it (ADR-8889).
    client_grade: crate::quality::LinkGrade,
}

impl Default for NetLink {
    fn default() -> Self {
        Self {
            end: None,
            label: String::new(),
            clock: LinkClock::Wall,
            started: Instant::now(),
            manual_us: 0,
            events: VecDeque::new(),
            dropped_events: 0,
            frames: 0,
            retired: None,
            resume: None,
            reconnecting: None,
            last_peer: None,
            interrupted_errors: None,
            reconnect_gave_up: false,
            capture: None,
            client_grade: crate::quality::LinkGrade::Good,
        }
    }
}

impl std::fmt::Debug for NetLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetLink")
            .field("role", &self.role())
            .field("label", &self.label)
            .field("clock", &self.clock)
            .field("frames", &self.frames)
            .finish_non_exhaustive()
    }
}

impl NetLink {
    fn start(&mut self, end: Box<dyn LinkEnd>, opts: &LinkOptions) {
        self.label = if opts.label.is_empty() {
            if end.is_server() { "server" } else { "client" }.to_owned()
        } else {
            opts.label.clone()
        };
        self.clock = opts.clock;
        self.started = Instant::now();
        self.manual_us = match opts.clock {
            LinkClock::Manual { start_ms, .. } => start_ms.saturating_mul(1000),
            LinkClock::Wall => 0,
        };
        self.frames = 0;
        self.install(end);
        self.reconnecting = None;
        self.last_peer = None;
        self.interrupted_errors = None;
        self.reconnect_gave_up = false;
        self.client_grade = crate::quality::LinkGrade::Good;
    }

    /// Serves game `G` on `socket`.
    ///
    /// # Errors
    ///
    /// The link already runs.
    pub fn host<G: NetGame<Message = NetFrame>>(
        &mut self,
        socket: LinkSocket,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        if self.end.is_some() {
            return Err(LinkError::AlreadyRunning);
        }
        if has_password(opts)
            && let Ok(bind) = socket.local_addr()
            && secret_would_be_plain(opts.crypto, bind)
        {
            return Err(LinkError::PasswordNeedsEncryption);
        }
        let config = ServerConfig {
            health: crate::session::HealthPolicy::default().with_timeouts(opts.timeout.sanitized()),
            max_peers: opts.max_peers,
            tick_rate_hz: opts.tick_rate_hz,
            password: opts.password.clone(),
            conditioner: opts.conditioner.clone(),
            crypto: opts.crypto,
            identity: opts.identity.clone(),
            resume_grace_ms: opts.resume_grace_ms,
            link: crate::connection::LinkConfig {
                mtu_probing: opts.mtu_probing,
                ..crate::connection::LinkConfig::default()
            },
            ..ServerConfig::default()
        };
        let server = NetServer::<G, LinkSocket>::new(socket, config);
        self.resume = None;
        self.start(
            Box::new(ServerEnd {
                server,
                bridge: ServerBridge::new(),
                content: Some(opts.content_hash),
            }),
            opts,
        );
        Ok(())
    }

    /// Serves game `G` on UDP `bind` (port 0 picks a free port; read it back with
    /// [`NetLink::local_addr`]).
    ///
    /// # Errors
    ///
    /// The link already runs, or the port cannot be bound.
    pub fn host_udp<G: NetGame<Message = NetFrame>>(
        &mut self,
        bind: SocketAddr,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        if self.end.is_some() {
            return Err(LinkError::AlreadyRunning);
        }
        let socket = UdpTransport::bind(bind).map_err(|e| LinkError::Bind(e.to_string()))?;
        self.host::<G>(Box::new(socket), opts)
    }

    /// Connects to a server of game `G` at `server` through `socket`.
    ///
    /// # Errors
    ///
    /// The link already runs, or the name does not fit the handshake.
    pub fn connect<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        socket: LinkSocket,
        server: SocketAddr,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        self.connect_with_join::<G>(socket, server, opts, ())
    }

    /// [`Self::connect`] for a game whose join request carries data: `join` goes with the join
    /// and again with every resume and reconnect of this link. A store ticket rides here: the
    /// server's hook ([`Self::set_ticket_check`]) reads it through `G::join_ticket` (KI-NT-047,
    /// ADR-10069). At most [`crate::handshake::MAX_JOIN_DATA_BYTES`] encoded bytes.
    pub fn connect_with_join<G: NetGame<Message = NetFrame>>(
        &mut self,
        socket: LinkSocket,
        server: SocketAddr,
        opts: &LinkOptions,
        join: G::JoinData,
    ) -> Result<(), LinkError> {
        if self.end.is_some() {
            return Err(LinkError::AlreadyRunning);
        }
        if has_password(opts) && secret_would_be_plain(opts.crypto, server) {
            return Err(LinkError::PasswordNeedsEncryption);
        }
        let end = open_client::<G>(socket, server, opts, None, None, &join)?;
        self.start(end, opts);
        // `JoinData` need not be `Sync`; the lock makes the opener shareable.
        let join = std::sync::Mutex::new(join);
        let open: ClientOpener = std::sync::Arc::new(move |socket, server, opts, ticket, timeout| {
            let join = join
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            open_client::<G>(socket, server, opts, ticket, timeout, &join)
        });
        self.resume = Some(ResumePlan {
            server,
            opts: opts.clone(),
            open,
            ticket: None,
            credential: None,
            opener: None,
        });
        Ok(())
    }

    /// Connects through a socket `opener` opens, and keeps `opener`: every automatic reconnect
    /// attempt (ADR-8352) opens a fresh socket of the same transport kind with it (a Steam
    /// channel, a QUIC endpoint, a UDP port on the new network after a phone's switch), and a
    /// client whose socket fails while the server is interrupted reconnects at once instead of
    /// waiting for the timeout ([`DEAD_SOCKET_SEND_ERRORS`]). [`Self::connect`] without an opener
    /// reconnects on its own socket.
    ///
    /// # Errors
    ///
    /// The link already runs, the opener fails, or the name does not fit the handshake.
    pub fn connect_via<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        opener: LinkSocketOpener,
        server: SocketAddr,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        if self.end.is_some() {
            return Err(LinkError::AlreadyRunning);
        }
        let socket = opener().map_err(|e| LinkError::Bind(e.to_string()))?;
        self.connect::<G>(socket, server, opts)?;
        self.set_socket_opener(Some(opener));
        Ok(())
    }

    /// Client: names how a reconnect opens its socket (`None`: reuse the endpoint's own socket).
    /// Nothing on a link that never connected as a client.
    pub fn set_socket_opener(&mut self, opener: Option<LinkSocketOpener>) {
        if let Some(plan) = self.resume.as_mut() {
            plan.opener = opener;
        }
    }

    /// Client: true while the link reconnects by itself after a timeout (ADR-8352).
    pub fn is_reconnecting(&self) -> bool {
        self.reconnecting.is_some()
    }

    /// Client: the automatic reconnect's progress, while it runs (ADR-8352); `net.status` shows
    /// the same under `reconnect`.
    pub fn reconnect_status(&self) -> Option<ReconnectStatus> {
        let r = self.reconnecting.as_ref()?;
        let now = self.now_ms();
        Some(ReconnectStatus {
            attempt: r.timeline.attempts(),
            since_ms: now.saturating_sub(r.timeline.since_ms()),
            next_in_ms: r.timeline.next_at_ms().saturating_sub(now),
            window_left_ms: r.timeline.deadline_ms().saturating_sub(now),
        })
    }

    /// Connects over UDP: binds an ephemeral port of the server address's family.
    ///
    /// # Errors
    ///
    /// The link already runs, no port can be bound, or the name does not fit the handshake.
    pub fn connect_udp<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        server: SocketAddr,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        if self.end.is_some() {
            return Err(LinkError::AlreadyRunning);
        }
        let opener: LinkSocketOpener = std::sync::Arc::new(move || client_udp_io(server));
        self.connect_via::<G>(opener, server, opts)
    }

    /// Client: the resume ticket of the server's last accept (spec 120 § 10.3), kept across the
    /// connection's end. `None` before the first accept or when the server keeps no dormant
    /// peers.
    pub fn resume_ticket(&self) -> Option<[u8; RESUME_TICKET_BYTES]> {
        self.resume.as_ref().and_then(|r| r.ticket)
    }

    /// Client: comes back to the last server as the same peer (spec 120 § 10.3), over a fresh UDP
    /// socket (a phone's new address after it lost signal). The current endpoint, if any, is
    /// dropped without a goodbye (a goodbye would end the server's hold), the clock keeps running,
    /// and the join presents the last resume ticket; the server keeps the `PeerId`, entities and
    /// ownership, and sends the full state first. `Ok(false)` when this link never connected as a
    /// client. Without a ticket the join is a fresh one.
    ///
    /// # Errors
    ///
    /// No port can be bound, or the last connection's options no longer fit the handshake.
    pub fn resume(&mut self) -> Result<bool, LinkError> {
        let Some(server) = self.resume.as_ref().map(|r| r.server) else {
            return Ok(false);
        };
        let socket = client_udp_socket(server)?;
        self.resume_on(socket)
    }

    /// [`Self::resume`] on a socket the caller opened (a Steam socket, a loopback test socket).
    ///
    /// # Errors
    ///
    /// The last connection's options no longer fit the handshake, or the known-hosts store fails.
    pub fn resume_on(&mut self, socket: LinkSocket) -> Result<bool, LinkError> {
        self.refresh_ticket();
        let Some(plan) = self.resume.clone() else {
            return Ok(false);
        };
        let end = (plan.open)(socket, plan.server, &plan.opts, plan.rejoin(), None)?;
        // The clock and the frame count carry on (a manual clock must not run back), and the new
        // endpoint publishes under the old label.
        self.install(end);
        self.interrupted_errors = None;
        Ok(true)
    }

    /// Reconnect bookkeeping after the endpoint's receive (ADR-8352): a timeout with a ticket
    /// starts the timeline instead of ending the client, attempts' own ends are swallowed, the
    /// first accept ends the outage with `PeerRestored`, and the window's end gives up once.
    fn drive_reconnect(&mut self, now: u64, events: &mut Vec<LinkEvent>) {
        let Some(plan) = self.resume.as_ref() else {
            return;
        };
        let policy = plan.opts.reconnect;
        let ticket = plan.ticket;
        let has_opener = plan.opener.is_some();
        let mut out = Vec::with_capacity(events.len());
        for event in events.drain(..) {
            match event {
                LinkEvent::Connected { peer, sealed } => {
                    let back = self.reconnecting.take();
                    if back.is_none() || self.last_peer != Some(peer) {
                        // A first connection, or a server that no longer held the slot: a new peer.
                        out.push(LinkEvent::Connected { peer, sealed });
                    }
                    if let Some(r) = back {
                        tracing::info!(target: "regia_net", attempts = r.timeline.attempts(), peer = peer.0, "client reconnected");
                        out.push(LinkEvent::PeerRestored { peer: PeerId::SERVER });
                        // The fresh connection grades afresh (`Good`); the grade the game last
                        // heard was the old one's (`Interrupted` after the timeout, ADR-8889).
                        let to = crate::quality::LinkGrade::Good;
                        if self.client_grade != to {
                            out.push(LinkEvent::QualityChanged {
                                peer: PeerId::SERVER,
                                from: self.client_grade,
                                to,
                            });
                            self.client_grade = to;
                        }
                    }
                    self.last_peer = Some(peer);
                    self.interrupted_errors = None;
                }
                // The last attempt's own end after the window gave up: the game already got its
                // final event and `Ended`.
                LinkEvent::Ended { .. } if self.reconnect_gave_up => self.reconnect_gave_up = false,
                LinkEvent::Ended { reason } if self.reconnecting.is_some() => {
                    // An attempt that timed out (or found the server full) leaves room for the
                    // next; a refusal for good ends the outage.
                    if !matches!(reason.as_str(), "connect_timeout" | "full" | "timeout") {
                        let attempts = self.reconnecting.take().map_or(0, |r| r.timeline.attempts());
                        out.push(LinkEvent::ReconnectFailed {
                            attempts,
                            reason: reason.clone(),
                        });
                        out.push(LinkEvent::Ended { reason });
                    }
                }
                LinkEvent::Ended { reason } if policy.enabled && reason == "timeout" && ticket.is_some() => {
                    tracing::info!(target: "regia_net", window_ms = policy.window_ms, "client timed out; reconnecting");
                    self.begin_reconnect(policy, now, ticket);
                }
                LinkEvent::PeerInterrupted { peer } => {
                    self.interrupted_errors = self.end.as_ref().map(|end| lock(end).socket_errors());
                    out.push(LinkEvent::PeerInterrupted { peer });
                }
                LinkEvent::PeerRestored { peer } => {
                    self.interrupted_errors = None;
                    out.push(LinkEvent::PeerRestored { peer });
                }
                LinkEvent::QualityChanged { peer, from, to } => {
                    self.client_grade = to;
                    out.push(LinkEvent::QualityChanged { peer, from, to });
                }
                other => out.push(other),
            }
        }
        *events = out;
        // A dead socket cannot bring the connection back: with a way to open a fresh one, go now.
        if self.reconnecting.is_none()
            && policy.enabled
            && has_opener
            && ticket.is_some()
            && let (Some(base), Some(end)) = (self.interrupted_errors, self.end.as_ref())
        {
            let end = lock(end);
            if end.interrupted() && end.socket_errors().saturating_sub(base) >= DEAD_SOCKET_SEND_ERRORS {
                drop(end);
                tracing::info!(target: "regia_net", "client socket failed while interrupted; reconnecting");
                self.begin_reconnect(policy, now, ticket);
                // The first attempt goes at once: nothing is coming back on this socket.
                if let Some(r) = self.reconnecting.as_mut() {
                    r.timeline.hurry(now);
                }
            }
        }
        let step = match self.reconnecting.as_mut() {
            Some(r) => r.timeline.poll(now),
            None => return,
        };
        match step {
            ReconnectStep::Wait => {}
            ReconnectStep::Attempt(attempt) => {
                events.push(LinkEvent::Reconnecting { attempt });
                if let Err(e) = self.start_attempt(now) {
                    // No socket this time (no network yet): the next attempt tries again.
                    tracing::warn!(target: "regia_net", error = %e, attempt, "reconnect attempt could not start");
                }
            }
            ReconnectStep::GiveUp => {
                let attempts = self.reconnecting.take().map_or(0, |r| r.timeline.attempts());
                tracing::warn!(target: "regia_net", attempts, "client reconnect window ended");
                self.reconnect_gave_up = self.end.as_ref().is_some_and(|end| !lock(end).ended());
                events.push(LinkEvent::ReconnectFailed {
                    attempts,
                    reason: "timeout".into(),
                });
                events.push(LinkEvent::Ended {
                    reason: "timeout".into(),
                });
            }
        }
    }

    fn begin_reconnect(
        &mut self,
        policy: ReconnectPolicy,
        now: u64,
        ticket: Option<[u8; RESUME_TICKET_BYTES]>,
    ) {
        self.interrupted_errors = None;
        self.reconnecting = Some(Reconnecting {
            timeline: Reconnector::start(policy, now, ticket.map_or(0, |t| seed_from(&t))),
        });
    }

    /// Replaces the endpoint with a fresh handshake that presents the resume ticket, on a socket
    /// of the transport kind the link connected with.
    fn start_attempt(&mut self, now: u64) -> Result<(), LinkError> {
        let Some(plan) = self.resume.clone() else {
            return Ok(());
        };
        let timeout = self
            .reconnecting
            .as_ref()
            .map(|r| r.timeline.attempt_timeout_ms(now));
        // The old endpoint goes first: a Steam channel holds one socket.
        let old = self.end.take().map(|end| {
            end.into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let socket = match (&plan.opener, old) {
            (Some(open), old) => {
                drop(old);
                open().map_err(|e| LinkError::Bind(e.to_string()))?
            }
            (None, Some(old)) => match old.into_socket() {
                Some(socket) => socket,
                None => return Ok(()),
            },
            (None, None) => return Err(LinkError::Bind("no socket to reconnect on".into())),
        };
        let end = (plan.open)(socket, plan.server, &plan.opts, plan.rejoin(), timeout)?;
        self.install(end);
        Ok(())
    }

    /// Copies the live client endpoint's newest ticket, and the resumption credential of the
    /// same accept, into the resume plan. An endpoint that is still connecting has neither yet,
    /// and the plan keeps what the last accept gave: every attempt of one outage presents the
    /// same credential, each with a newer flight.
    fn refresh_ticket(&mut self) {
        let (ticket, credential) = match self.end.as_ref().map(lock) {
            Some(end) => (end.resume_ticket(), end.resumption()),
            None => (None, None),
        };
        if let (Some(t), Some(plan)) = (ticket, self.resume.as_mut()) {
            if plan.ticket != Some(t) {
                // A new accept: its credential, or none (a plain link, a server without tickets),
                // replaces the old one; a credential never outlives the ticket it came with.
                plan.credential = credential;
            } else if plan.credential.is_none() {
                plan.credential = credential;
            }
            plan.ticket = Some(t);
        }
    }

    /// Makes `end` the endpoint; a client endpoint gets the link's capture.
    fn install(&mut self, mut end: Box<dyn LinkEnd>) {
        if let Some(tap) = &self.capture
            && !end.is_server()
        {
            end.set_capture(Some(tap.clone()));
        }
        self.end = Some(Mutex::new(end));
    }

    /// Client: captures every packet the client receives (after decryption), its receive and
    /// flush times, its input stamps and its end into `tap` (spec 120 § 15.5, ADR-8221, ADR-8890),
    /// or stops capturing with `None`. The tap stays with the link: a reconnect or a resume writes
    /// its new connection into the same ring as a new session, and [`NetLink::stop`] keeps it, so
    /// it can be saved after the session ended. Attach it before [`NetLink::connect`] (or before
    /// the first update after it) to capture the whole connection. Call
    /// [`NetLink::capture_frame`] once a frame with a digest of what the game showed, so a
    /// replay can be compared frame by frame. `false` on a server, which keeps nothing.
    pub fn set_capture(&mut self, tap: Option<CaptureTap>) -> bool {
        if self.role() == "server" {
            return false;
        }
        if let Some(end) = self.end.as_mut() {
            lock_mut(end).set_capture(tap.clone());
        }
        self.capture = tap;
        true
    }

    /// The capture attached with [`NetLink::set_capture`].
    pub fn capture(&self) -> Option<&CaptureTap> {
        self.capture.as_ref()
    }

    /// Writes the game's frame marker into the capture: the link's time and `digest`, a digest
    /// of what the game showed and predicted this frame (chain it over the frames). Nothing
    /// without a capture.
    pub fn capture_frame(&self, digest: u64) {
        if let Some(tap) = &self.capture {
            tap.frame(self.now_ms(), digest);
        }
    }

    /// Replays a captured client session (ADR-8890) instead of connecting: the link's client
    /// reads the captured packets at the captured milliseconds, runs the app's bridge,
    /// replication, prediction and interpolation on them, and sends nothing. Use the captured
    /// app's [`LinkOptions`] (its clock above all: a [`LinkClock::Manual`] replay runs at the
    /// captured times when the app updates as often as the captured one did) and attach a fresh
    /// capture with [`NetLink::set_capture`] to compare the two with
    /// [`crate::capture::compare_sessions`]. The client's socket is a [`ReplaySocket`] of the
    /// default datagram size; a capture made over a transport with another size replays with
    /// [`NetLink::replay_on`].
    ///
    /// # Errors
    ///
    /// The link already runs, or `capture_protocol_mismatch` when another game or protocol made
    /// the capture.
    pub fn replay<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        feed: ReplayFeed,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        self.replay_on::<G>(Box::new(ReplaySocket::new()), feed, opts)
    }

    /// [`NetLink::replay`] through `socket`, whose `max_datagram_size` must be the captured
    /// transport's (a [`ReplaySocket::with_max_datagram`]); it is never read, and nothing is sent
    /// to it.
    ///
    /// # Errors
    ///
    /// As [`NetLink::replay`].
    pub fn replay_on<G: NetGame<Message = NetFrame, JoinData = ()>>(
        &mut self,
        socket: LinkSocket,
        feed: ReplayFeed,
        opts: &LinkOptions,
    ) -> Result<(), LinkError> {
        if self.end.is_some() {
            return Err(LinkError::AlreadyRunning);
        }
        let config = client_config(opts, opts.server_key, None, None);
        let client = NetClient::<G, LinkSocket>::replay(socket, config, feed).map_err(LinkError::Capture)?;
        self.resume = None;
        self.start(
            Box::new(ClientEndpoint {
                client,
                bridge: ClientBridge::new(),
                tofu: None,
                content: Some(opts.content_hash),
            }),
            opts,
        );
        Ok(())
    }

    /// Client made by [`NetLink::replay`]: how far the captured session got (receives at another
    /// time than the captured one, receives past its end, records not reached). `None` on any
    /// other link.
    pub fn replay_progress(&self) -> Option<ReplayProgress> {
        self.end.as_ref().and_then(|end| lock(end).replay_progress())
    }

    /// Drops the endpoint (no goodbye is sent; peers time out). The next flush removes its report
    /// from [`NetAgentState`].
    pub fn stop(&mut self) {
        self.reconnecting = None;
        self.interrupted_errors = None;
        if self.end.take().is_some() {
            self.retired = Some(self.label.clone());
        }
    }

    /// Says goodbye, then drops the endpoint (ADR-2440): a server tells every peer it closes
    /// (`server_shutdown`) and refuses new joins, a client tells the server it leaves, so peers do
    /// not wait for a timeout. Idle: nothing.
    pub fn close(&mut self) {
        let now = self.now_ms();
        if let Some(end) = self.end.as_mut() {
            lock_mut(end).goodbye(now);
        }
        self.stop();
    }

    /// Server: marks `ip` as a relay's address (spec 120 § 10.6, ADR-8183): every client that
    /// relay forwards shares the IP, so the per-IP caps count them by address. False while idle
    /// or on a client.
    pub fn add_relay_ip(&mut self, ip: std::net::IpAddr) -> bool {
        self.end
            .as_mut()
            .is_some_and(|end| lock_mut(end).add_relay_ip(ip))
    }

    /// Server: installs the store's join ticket hook on this link's server
    /// ([`crate::NetServer::set_ticket_check`]: the verdict on `G::join_ticket`, `end_ticket` when
    /// the peer leaves, a late refusal kicks; KI-NT-047, ADR-10069). It replaces an earlier hook.
    /// False while idle or on a client (nothing is installed).
    pub fn set_ticket_check(&mut self, check: std::sync::Arc<dyn crate::ticket::JoinTicketCheck>) -> bool {
        self.end
            .as_mut()
            .is_some_and(|end| lock_mut(end).set_ticket_check(check))
    }

    /// Server: removes `peer` and tells it `kicked` (ADR-2440). False when the peer is unknown or
    /// this link is not a server.
    pub fn kick(&mut self, peer: PeerId) -> bool {
        self.end.as_mut().is_some_and(|end| lock_mut(end).kick(peer))
    }

    /// Server: each peer and its source address (ADR-2665 (4)); empty while idle or on a client.
    pub fn peer_addrs(&self) -> Vec<(PeerId, SocketAddr)> {
        self.end
            .as_ref()
            .map(|end| lock(end).peer_addrs())
            .unwrap_or_default()
    }

    /// Whether a live server endpoint admitted this peer (an idle or client link cannot).
    pub(crate) fn has_peer(&self, peer: PeerId) -> bool {
        self.end.as_ref().is_some_and(|end| lock(end).has_peer(peer))
    }

    /// Server: removes `peer` and tells it `banned` (ADR-2665 (4)). False when the peer is unknown
    /// or this link is not a server.
    pub fn remove_banned(&mut self, peer: PeerId) -> bool {
        self.end
            .as_mut()
            .is_some_and(|end| lock_mut(end).remove_banned(peer))
    }

    /// Server: refuses handshakes from `ip`'s block (its IPv4 address, or its whole IPv6 /64,
    /// ADR-3283) until `expires_unix_s` (0 = permanent), and removes the peers in it (ADR-2665
    /// (4)). The host mirrors the session's block-shaped address bans here; an explicit `/128`
    /// goes through [`Self::refuse_ip_exact`] instead, and other kinds are enforced by the host
    /// each tick.
    pub fn refuse_ip(&mut self, ip: std::net::IpAddr, reason: &str, expires_unix_s: u64) {
        if let Some(end) = self.end.as_mut() {
            lock_mut(end).refuse_ip(ip, true, reason, expires_unix_s);
        }
    }

    /// Server: refuses handshakes from exactly `ip` — no other address of its IPv6 /64 — until
    /// `expires_unix_s` (0 = permanent), and removes the peer at `ip` (ADR-4936). Mirrors a
    /// `bans.ron` address entry spelled `addr/128`.
    pub fn refuse_ip_exact(&mut self, ip: std::net::IpAddr, reason: &str, expires_unix_s: u64) {
        if let Some(end) = self.end.as_mut() {
            lock_mut(end).refuse_ip_exact(ip, reason, expires_unix_s);
        }
    }

    /// Server: lifts a [`Self::refuse_ip`].
    pub fn allow_ip(&mut self, ip: std::net::IpAddr) {
        if let Some(end) = self.end.as_mut() {
            lock_mut(end).refuse_ip(ip, false, "", 0);
        }
    }

    /// `"idle"`, `"server"` or `"client"`.
    pub fn role(&self) -> &'static str {
        match self.end.as_ref().map(lock) {
            None => "idle",
            Some(end) if end.is_server() => "server",
            Some(_) => "client",
        }
    }

    /// True while a server runs or a client is connected.
    pub fn is_connected(&self) -> bool {
        self.end.as_ref().is_some_and(|end| lock(end).connected())
    }

    /// True when the link carries sealed traffic (client: its link; server: at least one peer).
    pub fn is_sealed(&self) -> bool {
        self.end.as_ref().is_some_and(|end| lock(end).sealed())
    }

    /// The bound address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.end.as_ref().and_then(|end| lock(end).local_addr())
    }

    /// Server: its public key. Client: the key the server proved, once it did.
    pub fn server_key(&self) -> Option<ServerKey> {
        self.end.as_ref().and_then(|end| lock(end).server_key())
    }

    /// The endpoint's status (`None` while idle).
    pub fn status(&self) -> Option<NetStatus> {
        self.end.as_ref().map(|end| lock(end).status())
    }

    /// Replaces the outbound conditioner.
    pub fn set_conditioner(&mut self, config: ConditionerConfig) {
        if let Some(end) = self.end.as_mut() {
            lock_mut(end).set_conditioner(config);
        }
    }

    /// The link's current time in milliseconds.
    pub fn now_ms(&self) -> u64 {
        match self.clock {
            LinkClock::Wall => self.started.elapsed().as_millis() as u64,
            LinkClock::Manual { .. } => self.manual_us / 1000,
        }
    }

    /// App updates the link has flushed since it started.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// The label the endpoint report is published under.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Takes the events since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<LinkEvent> {
        self.events.drain(..).collect()
    }

    /// Events dropped because nobody took them.
    pub fn dropped_events(&self) -> u64 {
        self.dropped_events
    }

    fn push_events(&mut self, events: Vec<LinkEvent>) {
        for e in events {
            if self.events.len() >= MAX_LINK_EVENTS {
                self.events.pop_front();
                self.dropped_events += 1;
            }
            self.events.push_back(e);
        }
    }
}

fn has_password(opts: &LinkOptions) -> bool {
    opts.password.as_deref().is_some_and(|p| !p.is_empty())
}

fn lock(end: &Mutex<Box<dyn LinkEnd>>) -> std::sync::MutexGuard<'_, Box<dyn LinkEnd>> {
    end.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn lock_mut(end: &mut Mutex<Box<dyn LinkEnd>>) -> &mut Box<dyn LinkEnd> {
    end.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// `PreUpdate`, before [`NetSet::Receive`]: the endpoint reads its socket and the bridge fills the
/// node's inbox.
pub fn net_link_receive(world: &mut World) {
    let _span = tracing::trace_span!("net_link_receive").entered();
    if !world.contains_resource::<NetLink>() {
        return;
    }
    ensure_resources(world);
    world.resource_scope::<NetLink, _>(|world, mut link| {
        let now = link.now_ms();
        let mut events = Vec::new();
        if let Some(end) = link.end.as_mut() {
            lock_mut(end).receive(world, now, &mut events);
        }
        link.refresh_ticket();
        link.drive_reconnect(now, &mut events);
        link.push_events(events);
    });
}

/// `PostUpdate`, in [`NetSet::Flush`]: the node's frames and disconnects go to the endpoint, which
/// sends; the endpoint report goes to [`NetAgentState`]; a manual clock steps.
pub fn net_link_flush(world: &mut World) {
    let _span = tracing::trace_span!("net_link_flush").entered();
    if !world.contains_resource::<NetLink>() {
        return;
    }
    world.init_resource::<NetAgentState>();
    world.resource_scope::<NetLink, _>(|world, mut link| {
        if let Some(label) = link.retired.take() {
            world.resource_mut::<NetAgentState>().remove(&label);
        }
        let now = link.now_ms();
        let label = link.label.clone();
        if let Some(end) = link.end.as_mut() {
            let end = lock_mut(end);
            end.flush(world, now);
            end.publish(&label, &mut world.resource_mut::<NetAgentState>());
        }
        if let Some(reconnect) = link.reconnect_status() {
            // `net.status` shows the outage, not the attempt's own handshake state.
            let mut state = world.resource_mut::<NetAgentState>();
            if let Some(mut report) = state.reports().get(&label).cloned() {
                report.status.state = "reconnecting".into();
                report.status.reconnect = Some(reconnect);
                state.publish(&label, report);
            }
        }
        link.frames += 1;
        if let LinkClock::Manual { step_us, .. } = link.clock {
            link.manual_us = link.manual_us.saturating_add(step_us);
        }
    });
}

/// Inserts an idle [`NetLink`] and its two systems. Add it next to [`crate::NetPlugin`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NetLinkPlugin;

impl Plugin for NetLinkPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetLink>();
        app.init_resource::<NetAgentState>();
        app.add_systems(PreUpdate, net_link_receive.before(NetSet::Receive));
        app.add_systems(PostUpdate, net_link_flush.in_set(NetSet::Flush));
    }
}

/// `PreUpdate`, after the link read its socket and before [`NetSet::Receive`]: stamps the arrival
/// time of this frame's server states on the link's clock.
fn link_stamp_arrival(world: &mut World) {
    let Some(now) = world.get_resource::<NetLink>().map(|l| l.now_ms() as f64) else {
        return;
    };
    if let Some(mut ni) = world.get_resource_mut::<crate::interp::NetInterpolation>() {
        ni.now_ms = now;
    }
}

/// `PostUpdate`, in [`NetSet::Interpolate`]: shows the proxies at the link's clock.
fn link_interpolate(world: &mut World) {
    let _span = tracing::trace_span!("net_link_interpolate").entered();
    let Some(now) = world.get_resource::<NetLink>().map(|l| l.now_ms() as f64) else {
        return;
    };
    if world.contains_resource::<crate::interp::NetInterpolation>() {
        crate::interp::net_interpolate(world, now);
    }
}

/// Proxy interpolation (spec 74) on the [`NetLink`]'s clock instead of the app's real time: with a
/// [`LinkClock::Manual`] link, a test of several apps shows the same proxies on every run. Use it
/// in place of [`crate::NetInterpolationPlugin`], after calling
/// [`crate::interp::install_interpolation`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LinkInterpolationPlugin;

impl Plugin for LinkInterpolationPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            PreUpdate,
            link_stamp_arrival.after(net_link_receive).before(NetSet::Receive),
        );
        app.add_systems(PostUpdate, link_interpolate.in_set(NetSet::Interpolate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::known_hosts::KnownHosts;
    use crate::model::NetMode;
    use crate::plugin::NetPlugin;
    use crate::runtime::NetNode;
    use crate::socket::LoopbackNet;

    struct Game;
    impl NetGame for Game {
        type Message = NetFrame;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "test.link";
        const PROTOCOL_VERSION: u32 = 1;
    }

    fn app(mode: NetMode) -> App {
        let mut app = App::new();
        regia_app::enable_fixed_step(&mut app, 60.0);
        app.add_plugins((NetPlugin { mode }, NetLinkPlugin));
        app
    }

    fn manual() -> LinkOptions {
        LinkOptions {
            clock: LinkClock::Manual {
                start_ms: 0,
                step_us: 16_000,
            },
            ..LinkOptions::default()
        }
    }

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[test]
    fn a_client_app_joins_a_server_app_through_the_link() {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        let opts = manual();
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7100)).expect("bind")), &opts)
            .expect("host");
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(7100), &opts)
            .expect("connect");
        assert_eq!(server.world().resource::<NetLink>().role(), "server");
        assert_eq!(client.world().resource::<NetLink>().role(), "client");
        for _ in 0..30 {
            server.update();
            client.update();
        }
        let link = client.world().resource::<NetLink>();
        assert!(link.is_connected());
        assert_eq!(link.now_ms(), 30 * 16, "the manual clock steps once per update");
        let peers = server.world().resource::<NetNode>().peers();
        assert_eq!(peers.len(), 1);
        let events = server.world_mut().resource_mut::<NetLink>().take_events();
        assert!(
            matches!(events.as_slice(), [LinkEvent::Joined { .. }]),
            "{events:?}"
        );
        let events = client.world_mut().resource_mut::<NetLink>().take_events();
        assert!(
            matches!(events.as_slice(), [LinkEvent::Connected { .. }]),
            "{events:?}"
        );
        let state = server.world().resource::<NetAgentState>();
        assert!(
            state.reports().contains_key("server"),
            "the report reaches net.status"
        );
    }

    /// `LinkStats::rtt_ms` is 0 until the transport's first round-trip sample. Prediction must not
    /// take that 0 as a measured round trip: a lead started from it is a whole RTT short, which
    /// cost every two-machine run a startup resync (profile B: lead 2 at about 130 ms RTT).
    #[test]
    fn an_unsampled_transport_rtt_does_not_reach_prediction() {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        crate::predict::install_prediction(client.world_mut());
        let opts = manual();
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7101)).expect("bind")), &opts)
            .expect("host");
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(7101), &opts)
            .expect("connect");
        let (mut unsampled, mut sampled) = (0u32, 0u32);
        for _ in 0..60 {
            server.update();
            client.update();
            let Some(rtt) = client
                .world()
                .resource::<NetLink>()
                .status()
                .filter(|s| s.state == "connected")
                .and_then(|s| s.link)
                .map(|l| l.rtt_ms)
            else {
                continue;
            };
            let fed = client.world().resource::<ClientPrediction>().transport_rtt;
            if rtt > 0.0 {
                sampled += 1;
                assert_eq!(fed.map(|f| f.0), Some(rtt), "a sampled RTT is fed");
            } else {
                unsampled += 1;
                assert_eq!(fed, None, "an unsampled RTT (0) is not a measurement");
            }
        }
        assert!(sampled > 0, "the link sampled its round trip");
        assert!(unsampled > 0, "the test saw the connected-but-unsampled window");
    }

    #[test]
    fn a_second_start_is_refused_with_a_code() {
        let net = LoopbackNet::new();
        let mut link = NetLink::default();
        let opts = manual();
        link.host::<Game>(Box::new(net.bind(addr(7101)).expect("bind")), &opts)
            .expect("host");
        let err = link
            .host::<Game>(Box::new(net.bind(addr(7102)).expect("bind")), &opts)
            .expect_err("second");
        assert_eq!(err.code(), "link_running");
        link.stop();
        assert_eq!(link.role(), "idle");
    }

    /// Audit fixrev-15-k net3-determinism L4 (ADR-5444): a refusal issued before the first
    /// receive/flush converted the absolute expiry to a duration and re-added the server's
    /// clock, which was then still 0 — the ban was stored with its duration as a 1970 expiry and
    /// expired before it began, admitting the very next handshake. It must hold for its
    /// remaining duration. Since H1 the server's clock before the first receive is its
    /// construction wall second, not 0, so this test now guards the end-to-end behavior only:
    /// it also passes without the wall stamp in `ServerEnd::refuse_ip`, which covers just the
    /// seconds the derived clock can lag the wall (audit rev-kimi-17 fixrev-15-k finding 4).
    #[test]
    fn a_refusal_before_the_first_receive_holds_for_its_duration() {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        let opts = manual();
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7104)).expect("bind")), &opts)
            .expect("host");
        // Immediately — no update ran the link's receive/flush yet, so the server clock was
        // never injected.
        let localhost = std::net::IpAddr::from([127, 0, 0, 1]);
        server.world_mut().resource_mut::<NetLink>().refuse_ip(
            localhost,
            "test",
            crate::session::unix_now_s() + 30,
        );
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(7104), &opts)
            .expect("connect");
        for _ in 0..20 {
            server.update();
            client.update();
        }
        assert!(
            server.world().resource::<NetNode>().peers().is_empty(),
            "the refused address must not join while the ban runs: {:?}",
            server.world().resource::<NetNode>().peers()
        );
    }

    #[test]
    fn required_encryption_seals_a_loopback_link() {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        let opts = LinkOptions {
            crypto: CryptoMode::Required,
            ..manual()
        };
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7103)).expect("bind")), &opts)
            .expect("host");
        let key = server.world().resource::<NetLink>().server_key();
        assert!(key.is_some(), "a server has a key");
        let client_opts = LinkOptions {
            server_key: key,
            ..opts
        };
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(
                Box::new(net.bind(addr(0)).expect("bind")),
                addr(7103),
                &client_opts,
            )
            .expect("connect");
        for _ in 0..40 {
            server.update();
            client.update();
        }
        assert!(client.world().resource::<NetLink>().is_sealed());
        assert!(server.world().resource::<NetLink>().is_sealed());
        let peers = server.world().resource::<NetNode>().peers();
        assert_eq!(peers.len(), 1);
        assert!(server.world().resource::<NetLink>().has_peer(peers[0]));
        assert!(!server.world().resource::<NetLink>().has_peer(PeerId(u32::MAX)));
        assert!(!client.world().resource::<NetLink>().has_peer(peers[0]));
    }

    /// A client that pins a key the server does not hold never connects and reports why.
    #[test]
    fn a_wrong_pinned_key_ends_the_client_with_its_code() {
        let net = LoopbackNet::new();
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        let opts = LinkOptions {
            crypto: CryptoMode::Required,
            ..manual()
        };
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7104)).expect("bind")), &opts)
            .expect("host");
        let other = ServerIdentity::generate().expect("key").public();
        let client_opts = LinkOptions {
            server_key: Some(other),
            ..opts
        };
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(
                Box::new(net.bind(addr(0)).expect("bind")),
                addr(7104),
                &client_opts,
            )
            .expect("connect");
        for _ in 0..60 {
            server.update();
            client.update();
        }
        assert!(!client.world().resource::<NetLink>().is_connected());
        let events = client.world_mut().resource_mut::<NetLink>().take_events();
        assert_eq!(
            events,
            vec![LinkEvent::Ended {
                reason: "server_key_mismatch".into()
            }]
        );
        assert!(server.world().resource::<NetNode>().peers().is_empty());
    }

    /// A client that goes silent is reported as left once the server's silence timeout passes,
    /// and the node stops replicating to it.
    #[test]
    fn a_silent_client_is_reported_left_and_dropped_from_the_node() {
        // Without a resume grace the timeout is the leave; with the default 60 s grace (spec 120
        // § 10.3, ADR-8298) the peer is dormant at the timeout and leaves at the grace's end.
        for (port, grace) in [(7105u16, 0u64), (7106, DEFAULT_RESUME_GRACE_MS)] {
            let net = LoopbackNet::new();
            let mut server = app(NetMode::DedicatedServer);
            let mut client = app(NetMode::Client);
            let opts = LinkOptions {
                timeout: crate::session::TimeoutPolicy {
                    timeout_ms: 10_000,
                    start_grace_ms: 0,
                    ..Default::default()
                },
                resume_grace_ms: grace,
                ..manual()
            };
            server
                .world_mut()
                .resource_mut::<NetLink>()
                .host::<Game>(Box::new(net.bind(addr(port)).expect("bind")), &opts)
                .expect("host");
            client
                .world_mut()
                .resource_mut::<NetLink>()
                .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(port), &opts)
                .expect("connect");
            for _ in 0..30 {
                server.update();
                client.update();
            }
            assert_eq!(server.world().resource::<NetNode>().peers().len(), 1);
            let _ = server.world_mut().resource_mut::<NetLink>().take_events();
            client.world_mut().resource_mut::<NetLink>().stop();
            // The explicit silence timeout is 10 s: 600 steps of 16 ms, and some margin.
            for _ in 0..700 {
                server.update();
            }
            let events = server.world_mut().resource_mut::<NetLink>().take_events();
            if grace == 0 {
                assert!(server.world().resource::<NetNode>().peers().is_empty());
                assert!(
                    matches!(
                        events.as_slice(),
                        [
                            LinkEvent::PeerInterrupted { .. },
                            // ADR-8182: the grade shows the interruption once.
                            LinkEvent::QualityChanged { to: crate::quality::LinkGrade::Interrupted, .. },
                            LinkEvent::Left { reason, .. },
                        ] if reason == "timeout"
                    ),
                    "{events:?}"
                );
                continue;
            }
            assert!(
                matches!(
                    events.as_slice(),
                    [
                        LinkEvent::PeerInterrupted { .. },
                        LinkEvent::QualityChanged {
                            to: crate::quality::LinkGrade::Interrupted,
                            ..
                        },
                        LinkEvent::Dormant { .. },
                    ]
                ),
                "{events:?}"
            );
            assert_eq!(
                server.world().resource::<NetNode>().peers().len(),
                1,
                "held dormant"
            );
            // The 60 s grace: 3,750 steps of 16 ms, and some margin.
            for _ in 0..3_800 {
                server.update();
            }
            assert!(server.world().resource::<NetNode>().peers().is_empty());
            let events = server.world_mut().resource_mut::<NetLink>().take_events();
            assert!(
                matches!(events.as_slice(), [LinkEvent::Left { reason, .. }] if reason == "timeout"),
                "{events:?}"
            );
        }
    }

    /// A password never goes out on a plain link to another machine; a stopped link's report is
    /// removed from the agent state.
    #[test]
    fn plain_passwords_are_refused_and_a_stopped_report_is_removed() {
        let net = LoopbackNet::new();
        let plain = LinkOptions {
            crypto: CryptoMode::Off,
            password: Some("hunter2".into()),
            ..manual()
        };
        let remote = SocketAddr::from(([192, 0, 2, 1], 7777));
        let mut link = NetLink::default();
        let err = link
            .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), remote, &plain)
            .expect_err("plain password");
        assert_eq!(err.code(), "link_password_plain");
        let err = link
            .host::<Game>(
                Box::new(net.bind(SocketAddr::from(([192, 0, 2, 2], 7777))).expect("bind")),
                &plain,
            )
            .expect_err("plain password");
        assert_eq!(err.code(), "link_password_plain");
        // On loopback the secret stays on this machine.
        link.host::<Game>(Box::new(net.bind(addr(7106)).expect("bind")), &plain)
            .expect("loopback host");

        let mut server = app(NetMode::DedicatedServer);
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(7107)).expect("bind")), &manual())
            .expect("host");
        server.update();
        assert!(
            server
                .world()
                .resource::<NetAgentState>()
                .reports()
                .contains_key("server")
        );
        server.world_mut().resource_mut::<NetLink>().stop();
        server.update();
        assert!(server.world().resource::<NetAgentState>().reports().is_empty());
    }

    // ---- Trust on first use (ADR-2416) ------------------------------------------------------

    fn temp_store(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("regia-net-link-tofu-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join(crate::known_hosts::KNOWN_HOSTS_FILE)
    }

    /// One server app with `identity` on `port` and one client app trusting through `store`; runs
    /// both for `frames` updates and returns the client's events.
    fn tofu_round(
        net: &LoopbackNet,
        port: u16,
        identity: &ServerIdentity,
        store: &std::path::Path,
        frames: usize,
    ) -> (Vec<LinkEvent>, bool) {
        let mut server = app(NetMode::DedicatedServer);
        let mut client = app(NetMode::Client);
        let opts = LinkOptions {
            crypto: CryptoMode::Required,
            identity: Some(identity.clone()),
            ..manual()
        };
        server
            .world_mut()
            .resource_mut::<NetLink>()
            .host::<Game>(Box::new(net.bind(addr(port)).expect("bind")), &opts)
            .expect("host");
        let client_opts = LinkOptions {
            known_hosts: Some(store.to_path_buf()),
            host_id: "Test.Host".into(),
            ..opts
        };
        client
            .world_mut()
            .resource_mut::<NetLink>()
            .connect::<Game>(
                Box::new(net.bind(addr(0)).expect("bind")),
                addr(port),
                &client_opts,
            )
            .expect("connect");
        for _ in 0..frames {
            server.update();
            client.update();
        }
        let connected = client.world().resource::<NetLink>().is_connected();
        let events = client.world_mut().resource_mut::<NetLink>().take_events();
        (events, connected)
    }

    /// The first connection pins the server's key in the store; the next one to the same host id
    /// with the same key connects quietly; a server with another key is refused with
    /// `net_host_key_changed` before anything secret is sent, and the store keeps the old key.
    #[test]
    fn the_key_is_pinned_on_first_use_and_a_mismatch_is_refused() {
        let net = LoopbackNet::new();
        let store = temp_store("pin");
        let identity = ServerIdentity::generate().expect("key");
        let (events, connected) = tofu_round(&net, 7110, &identity, &store, 40);
        assert!(connected);
        assert!(
            matches!(events.as_slice(), [LinkEvent::Connected { sealed: true, .. }, LinkEvent::HostKeyPinned { host, key, saved: true }]
                if host == "test.host" && *key == identity.public()),
            "{events:?}"
        );
        let pinned = KnownHosts::load(&store).expect("load");
        assert_eq!(pinned.get("test.host").map(|h| h.key), Some(identity.public()));

        let (events, connected) = tofu_round(&net, 7111, &identity, &store, 40);
        assert!(connected, "the same key on a second visit");
        assert!(
            matches!(events.as_slice(), [LinkEvent::Connected { .. }]),
            "{events:?}"
        );

        let impostor = ServerIdentity::generate().expect("key");
        let (events, connected) = tofu_round(&net, 7112, &impostor, &store, 60);
        assert!(!connected);
        assert_eq!(
            events,
            vec![LinkEvent::Ended {
                reason: "net_host_key_changed".into()
            }]
        );
        assert_eq!(
            KnownHosts::load(&store).expect("load"),
            pinned,
            "the store is unchanged"
        );
        if let Some(dir) = store.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    /// A damaged store refuses the connection with its code instead of trusting a key it cannot
    /// check, and is not overwritten; an explicit pin ignores the store.
    #[test]
    fn a_damaged_store_refuses_to_connect() {
        let net = LoopbackNet::new();
        let store = temp_store("damaged");
        if let Some(dir) = store.parent() {
            std::fs::create_dir_all(dir).expect("dir");
        }
        std::fs::write(&store, "(format_version: 1, hosts: {").expect("write");
        let mut link = NetLink::default();
        let opts = LinkOptions {
            known_hosts: Some(store.clone()),
            ..manual()
        };
        let err = link
            .connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(7113), &opts)
            .expect_err("damaged store");
        assert_eq!(err.code(), "net_known_hosts_unreadable");
        assert_eq!(
            std::fs::read_to_string(&store).expect("read"),
            "(format_version: 1, hosts: {"
        );
        let pinned = LinkOptions {
            server_key: Some(ServerIdentity::generate().expect("key").public()),
            ..opts
        };
        link.connect::<Game>(Box::new(net.bind(addr(0)).expect("bind")), addr(7113), &pinned)
            .expect("a pinned key needs no store");
        if let Some(dir) = store.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}
