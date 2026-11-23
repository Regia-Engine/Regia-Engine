// SPDX-License-Identifier: MPL-2.0
//! The authoritative server endpoint.
//!
//! [`NetServer`] owns a socket, the [`Session`], one [`Connection`] per peer, the
//! [`TickDriver`] that collects player input and the outbound [`NetConditioner`]. A game loop calls
//! [`NetServer::receive`], drains [`NetServer::poll_event`], advances the simulation with
//! [`NetServer::advance_tick`], sends its state with [`NetServer::send`] and finally
//! [`NetServer::flush`]. All time is passed in as milliseconds on one monotonic clock.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::marker::PhantomData;
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::addr_block::same_block;
use crate::conditioner::{ConditionerConfig, NetConditioner};
use crate::connection::{Channel, Connection, LinkConfig, LinkFailure, ReassemblyBudget, SendError};
use crate::crypto::{
    self, CRYPTO_VERSION, CryptoMode, DEFAULT_HANDSHAKES_PER_S, HandshakeGovernor, KeyInit, KeyReply,
    NOISE_PATTERN, Negotiated, PLAIN_VERSION, PendingHandshake, PendingTable, RekeyPolicy, ResumeEpochs,
    ResumeInit, ResumeRefusal, ResumeReply, ResumeResponder, ResumeRetry, SEALED_RESERVED_HEADER_BYTES,
    SealedHeader, SealedKind, SecureLink, ServerIdentity, ServerKey, TicketClaims, TicketKeys,
    accept_handshake,
};
use crate::feedback::{InputArrivals, LinkFeedback};
use crate::handshake::{
    Accept, CONNECTED_HEADER_BYTES, Challenge, ChallengeResponse, ConnectRequest, CookieMinter, Deny,
    DenyReason, DisconnectReason, JoinToken, PacketKind, PasswordChallenge, PeerRole, RESUME_TICKET_BYTES,
    RESUMPTION_TICKET_BYTES, connected_header, fresh_secret, split_datagram,
};
use crate::inbound::{InboundRateConfig, InboundRates, PacketVerdict, PeerInbound, PeerInboundStatus};
use crate::password::PasswordGate;
use crate::path::{CandidateLimiter, PathState};
use crate::payload::{NetCodec, NetGame, protocol_id};
use crate::quality::{GradeChange, InputTotals, LinkGrade, PeerQuality, QualityTracker};
use crate::rate_limit::{ReliableSendGovernor, SourceLimiter};
use crate::resume_register::{Flight, PendingResume, ResumeRegister, ResumeTarget, ticket_id};
use crate::retry_tokens::UnconfirmedTokens;
use crate::session::{HealthPolicy, PeerId, Session};
use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};
use crate::status::{
    CryptoStatus, DiagnosticsOverlay, EndpointReport, EndpointTotals, NetAgentState, NetStatus, PeerRow,
};
use crate::tick::{InputBundle, TickDriver, TickInputs, decode_reliable_input};
use crate::ticket::{JoinTicketCheck, TicketRefused, TicketVerdict};
use crate::version_probe::{ProbeResponder, VersionMismatchReport, VersionStamp, is_probe_request};
use crate::wire::{WireError, WireReader, WireWriter, constant_time_eq};

/// Lane byte in front of every message inside a connection.
pub(crate) const LANE_GAME: u8 = 0;
/// Client input bundle.
pub(crate) const LANE_INPUT: u8 = 1;
/// Server tick sync.
pub(crate) const LANE_TICK_SYNC: u8 = 2;
/// One client input too large for a datagram, on the reliable channel: `tick u64 | input`
/// (ADR-7896, wire version 3).
pub(crate) const LANE_INPUT_RELIABLE: u8 = 3;
/// The channel the reliable input lane rides: the unordered inputs stream (spec 120 § 5.1), so a
/// lost input never waits behind game messages and a late one never holds the next back.
pub const RELIABLE_INPUT_CHANNEL: Channel = Channel::Stream(crate::connection::StreamId::INPUTS);
/// Server input feedback, [`crate::feedback::LinkFeedback`] (spec 120 § 6.4, ADR-8077, wire
/// version 4).
pub(crate) const LANE_LINK_FEEDBACK: u8 = 4;
/// Client clock stamp, `sent_ms u32` (wrapping milliseconds of the client's clock), every
/// [`crate::quality::QUALITY_SAMPLE_MS`] on the essential unreliable class: the server's
/// interarrival jitter of the upstream path (spec 120 § 13.1, ADR-8182). The server's own stamp
/// rides at the end of each tick sync, and a client sends this lane only to a server whose tick
/// syncs carry one: older peers ignore the trailing stamp and never see the lane (ADR-8889; it
/// ships in v0.7's wire 6).
pub(crate) const LANE_CLOCK: u8 = 5;

/// Datagrams one `receive` call does real work for at most outside connected peers (handshakes,
/// rebinds, unknown sources), and the share connected peers split per receive (ADR-8181).
const MAX_DATAGRAMS_PER_RECEIVE: usize = 4096;
/// The message encode buffer the server keeps between sends is dropped past this size, so one
/// huge message does not pin its storage for the server's life (ADR-10081).
const ENCODE_SCRATCH_MAX_BYTES: usize = 64 * 1024;
/// Datagrams one `receive` call reads at most, so a flood cannot stall a frame. Above the work
/// budget: a datagram over its peer's rate or slice is dropped after a header parse and one lookup,
/// so reading past a flood is cheap and the other peers' datagrams behind it are reached (ADR-8181).
pub const MAX_DATAGRAMS_READ_PER_RECEIVE: usize = 4 * MAX_DATAGRAMS_PER_RECEIVE;
/// The least per-receive slice of a connected peer: [`MAX_DATAGRAMS_READ_PER_RECEIVE`]'s work
/// budget is split evenly between peers, but never below this many datagrams each (four full polls
/// of the default link).
pub const MIN_PEER_SLICE: u32 = 256;
/// Messages of one channel a peer may deliver per `receive`. Past it unreliable messages are
/// dropped (and the peer is over rate, ADR-8181), and a reliable one disconnects the peer with
/// `Flood`. The channels count apart: only the
/// reliable window, which keeps a correct peer far below this, makes the reliable count safe to act on.
pub const MAX_PEER_MESSAGES_PER_RECEIVE: u32 = 4096;
/// A connected peer whose malformed score reaches this many points is removed (`MalformedPacket`).
/// Each refused message adds its kind's weight ([`RejectKind::malformed_weight`]) and the score
/// leaks `inbound::MALFORMED_LEAK_PER_S` points a second; good or empty packets earn nothing
/// (ADR-8181), so garbage mixed with good traffic still adds up. Only traffic that authenticated
/// under the peer's session keys (or, on a plain link, came from its validated address) is scored
/// (ADR-8606). The replication runtime scores its own refusals (voice, ADR-7984) against the same
/// limit.
pub const MALFORMED_SCORE_LIMIT: u32 = 1024;
/// Malformed points of one refusal that only a broken or hostile sender produces (ADR-8606): an
/// authenticated message or packet that failed to decode, an unknown lane, an out-of-range field.
/// At the soak's 63 refusals a second the score passes [`MALFORMED_SCORE_LIMIT`] in about 1 s; a
/// drip of 5 a second in about 13.5 s; a peer below one such refusal every 4 s never climbs.
pub const MALFORMED_WEIGHT: u32 = 16;
/// Malformed points of a refusal that a correct client can still cause without a broken link or
/// codec (ADR-8606): a spectator's input, which is well formed, refused undecoded, and comes from a
/// host game that kept its input loop running for a spectator.
pub const MALFORMED_WEIGHT_MISUSE: u32 = 1;
/// Source IPs the handshake limiter tracks.
const MAX_HANDSHAKE_SOURCES: usize = 65_536;
/// Disconnect notices are sent this many times (they are not acknowledged).
const DISCONNECT_REPEATS: usize = 3;
/// A new join may replace the sealed peer at its address once that peer has been silent this
/// long, in milliseconds. Well above the keepalive, and below the client's connect timeout.
pub const SEALED_PEER_QUIET_MS: u64 = 2_000;

/// Server settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Peer capacity (players and spectators).
    pub max_peers: usize,
    /// Authoritative tick rate.
    pub tick_rate_hz: u32,
    /// Required client build id; empty accepts any build.
    pub build: String,
    /// Host password; `None` for an open server.
    pub password: Option<String>,
    /// Flag thresholds and the silence timeout.
    pub health: HealthPolicy,
    /// Per-connection channel tuning.
    pub link: LinkConfig,
    /// Outbound network conditioner (off by default).
    pub conditioner: ConditionerConfig,
    /// Reliable messages per peer per second the server will send (`0` = unlimited).
    pub reliable_msgs_per_s: u32,
    /// Future ticks of input buffered per peer.
    pub input_buffer_ticks: usize,
    /// How often each client gets a tick sync, in milliseconds.
    pub tick_sync_interval_ms: u64,
    /// Peers one address block may hold at once (`0` = no limit): one IPv4 address, or one IPv6
    /// /64 (ADR-3283). Keeps one machine or subscriber from taking every slot; raise it for
    /// players behind one carrier-grade NAT. With `resume_grace_ms` above 0, a player that leaves
    /// holding its resume ticket (a goodbye, or a timeout into dormancy) keeps its slot of the
    /// block for the grace, and its return with the ticket is never counted as a new peer; a fresh
    /// join counts the held slots too and may borrow at most one, so a block holds at most this
    /// many plus one through joins (ADR-8733).
    pub max_peers_per_ip: usize,
    /// When links are encrypted. The default requires it for every non-loopback peer (ADR-1901).
    pub crypto: CryptoMode,
    /// The server's static key; `None` derives one from the server's secret, so it changes every
    /// run unless the caller keeps it. Clients pin the public half.
    #[serde(skip)]
    pub identity: Option<ServerIdentity>,
    /// Key exchanges one second may run (`0` = no limit). Each costs a few curve operations.
    pub max_handshakes_per_s: u32,
    /// Proved handshake packets one address block (an IPv4 address or an IPv6 /64) may send per
    /// second (`0` = no limit), with a burst of twice that. Only packets that brought a valid
    /// cookie back (`ChallengeResponse`, `KeyInit`) are counted, after their keyed cookie check
    /// (one short BLAKE3), so a forged source address cannot spend a real client's allowance
    /// (ADR-3283); no join validation or curve work runs for a packet over it. A `ConnectRequest`
    /// is never counted: its source is unproved, and its answer is no larger than it.
    pub handshakes_per_ip_per_s: u32,
    /// Handshake answers (a `Challenge` or `Deny`) one second may send in reply to pre-proof
    /// `ConnectRequest`s, unusable-cookie refreshes and version probes, server-wide (`0` = no
    /// limit; default 4,096, spec 120 § 11.4). A request over the cap is dropped without an
    /// answer, so a spoofed-address flood cannot turn the server into a 1:1 packet reflector
    /// (ADR-4936). Within the cap each source prefix (IPv4 /24, IPv6 /56) gets a fair share of the
    /// rate among the prefixes asking, and an answer whose cookie comes back valid is given back
    /// to its prefix (ADR-8718): a forged flood from many prefixes spends only their shares, and a
    /// forger can spend a real client's share only by forging that client's own prefix (finding
    /// 23, ADR-3283). Banned sources are still refused, after their cookie proves the address, in
    /// `on_key_init` and the join admission.
    pub pre_proof_answers_per_s: u32,
    /// When the server moves each peer's sending key on (ADR-2416). Each client's records are
    /// followed whatever this says.
    pub rekey: RekeyPolicy,
    /// How long a player that timed out stays dormant, in milliseconds (spec 120 § 10.3, ADR-8298):
    /// its `PeerId` and slot are held and a join that presents its resume ticket comes back as the
    /// same peer ([`ServerEvent::PeerDormant`], [`ServerEvent::PeerResumed`]). `0` (the default)
    /// keeps no dormant peers: a timeout is a [`ServerEvent::PeerLeft`] at once. `NetLink` turns
    /// it on (`link::LinkOptions::resume_grace_ms`).
    pub resume_grace_ms: u64,
    /// One uplink shared by every peer (spec 120 § 4.2 item 8, ADR-8301): an aggregate rate
    /// controller fed by the peers' delivery rates and the host's own queueing signal, split with
    /// weighted max-min fairness (spectators a quarter). [`ServerConfig::listen_server`] turns it
    /// on; a dedicated server may set it with [`Self::uplink_bytes_per_second`].
    #[serde(default)]
    pub shared_uplink: bool,
    /// The shared uplink's fixed capacity, bytes per second (`server_rate_bytes`); 0 measures it.
    #[serde(default)]
    pub uplink_bytes_per_second: u32,
    /// Bytes of out-of-order reliable fragments all peers together may make the server store
    /// (spec 120 § 5.2, ADR-7981); `0` (the default) means
    /// `min(64 MiB, 32 KiB x max_peers)` ([`ServerConfig::reassembly_budget_bytes`]). A packet that
    /// would store past it is refused as if lost; each peer's own cap is
    /// `LinkConfig::connection_buffer_bytes`.
    pub server_buffer_bytes: usize,
    /// Per-peer inbound rate policy (spec 120 § 11.3, ADR-8181): packet, byte and message buckets
    /// charged before the AEAD open, and removal with `DisconnectReason::Flood` after two seconds
    /// over them. The default derives the rates from `tick_rate_hz` and `link`.
    pub inbound: InboundRateConfig,
}

/// Most of [`ServerConfig::reassembly_budget_bytes`] when it is derived: 64 MiB.
pub const MAX_SERVER_BUFFER_BYTES: usize = 64 * 1024 * 1024;
/// Derived [`ServerConfig::reassembly_budget_bytes`] per peer slot: 32 KiB.
pub const SERVER_BUFFER_BYTES_PER_PEER: usize = 32 * 1024;

impl ServerConfig {
    /// The defaults of a listen server: the host's home uplink is shared by every client, so the
    /// shared uplink is on (spec 120 § 4.2 item 8).
    pub fn listen_server() -> Self {
        Self {
            shared_uplink: true,
            ..Self::default()
        }
    }

    /// The server-wide reassembly budget in force: `server_buffer_bytes`, or
    /// `min(64 MiB, 32 KiB x max_peers)` when that is `0`.
    pub fn reassembly_budget_bytes(&self) -> usize {
        if self.server_buffer_bytes > 0 {
            self.server_buffer_bytes
        } else {
            MAX_SERVER_BUFFER_BYTES.min(SERVER_BUFFER_BYTES_PER_PEER.saturating_mul(self.max_peers.max(1)))
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_peers: 16,
            tick_rate_hz: 30,
            build: String::new(),
            password: None,
            health: HealthPolicy::default(),
            link: LinkConfig::default(),
            conditioner: ConditionerConfig::default(),
            reliable_msgs_per_s: 0,
            input_buffer_ticks: 64,
            tick_sync_interval_ms: 250,
            max_peers_per_ip: 8,
            crypto: CryptoMode::default(),
            identity: None,
            max_handshakes_per_s: DEFAULT_HANDSHAKES_PER_S,
            handshakes_per_ip_per_s: 64,
            pre_proof_answers_per_s: 4_096,
            rekey: RekeyPolicy::default(),
            resume_grace_ms: 0,
            shared_uplink: false,
            uplink_bytes_per_second: 0,
            server_buffer_bytes: 0,
            inbound: InboundRateConfig::default(),
        }
    }
}

/// Why [`NetServer::adopt_dormant`] refused (spec 120 § 10.5, ADR-8901). [`Self::code`] is the
/// agent-protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdoptError {
    /// The server keeps no dormant peers ([`ServerConfig::resume_grace_ms`] is 0).
    ResumeOff,
    /// The ticket is all zeros (the wire's "no ticket").
    ZeroTicket,
    /// The id is the server's own (`PeerId::SERVER`).
    ServerId,
    /// The id is connected, dormant or kept already.
    IdInUse,
    /// Another peer of this server holds the same ticket.
    TicketInUse,
    /// The server was shut down.
    Closed,
}

impl AdoptError {
    /// Lowercase code (agent protocol, logs).
    pub fn code(self) -> &'static str {
        match self {
            Self::ResumeOff => "adopt_resume_off",
            Self::ZeroTicket => "adopt_zero_ticket",
            Self::ServerId => "adopt_server_id",
            Self::IdInUse => "adopt_id_in_use",
            Self::TicketInUse => "adopt_ticket_in_use",
            Self::Closed => "adopt_closed",
        }
    }
}

impl std::fmt::Display for AdoptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for AdoptError {}

/// Something that happened on the server.
#[derive(Debug, Clone)]
pub enum ServerEvent<G: NetGame> {
    /// A peer became silent; its seat and connection are retained.
    PeerInterrupted {
        /// The silent peer.
        peer: PeerId,
    },
    /// Authentic traffic returned after an interruption.
    PeerRestored {
        /// The restored peer.
        peer: PeerId,
    },
    /// A peer joined.
    PeerJoined {
        /// Its id.
        peer: PeerId,
        /// Its display name.
        name: String,
        /// Player or spectator.
        role: PeerRole,
        /// The game's join data.
        join: G::JoinData,
    },
    /// A player timed out and is dormant (spec 120 § 10.3; only with
    /// [`ServerConfig::resume_grace_ms`] above 0): its connection is gone, but its `PeerId` and
    /// slot are held for the grace window. [`ServerEvent::PeerResumed`] follows when it comes back
    /// with its ticket, or [`ServerEvent::PeerLeft`] (`timeout`) when the grace ends.
    PeerDormant {
        /// The dormant peer.
        peer: PeerId,
        /// Why its connection ended (`timeout`).
        reason: DisconnectReason,
    },
    /// A peer came back with its resume ticket on a new connection and keeps its `PeerId`
    /// (spec 120 § 10.3): from dormancy, or taking over a connection the server still held (the
    /// old one is dropped without an event).
    PeerResumed {
        /// Its id, the same as before.
        peer: PeerId,
        /// Its display name.
        name: String,
        /// Player or spectator.
        role: PeerRole,
        /// The game's join data.
        join: G::JoinData,
    },
    /// A peer left or was removed.
    PeerLeft {
        /// Its id.
        peer: PeerId,
        /// Why.
        reason: DisconnectReason,
    },
    /// A game message arrived.
    Message {
        /// From whom.
        peer: PeerId,
        /// On which channel.
        channel: Channel,
        /// The message.
        message: G::Message,
    },
    /// The server refused one message a connected peer sent (spec 70 § 6.7, ADR-7892). Nothing in
    /// it reached the game or the tick driver. When `kind` is fatal
    /// ([`RejectKind::is_fatal`]) the server has already removed the peer, and a
    /// [`ServerEvent::PeerLeft`] with [`DisconnectReason::ProtocolError`] follows
    /// ([`DisconnectReason::Flood`] for [`RejectKind::Flood`]); otherwise the
    /// message scored malformed points, and a peer whose points pass the limit leaves with
    /// [`DisconnectReason::MalformedPacket`]. A host may act on it itself, for example
    /// [`NetServer::kick`] a peer on its first rejection.
    Rejected {
        /// The sender.
        peer: PeerId,
        /// What was refused.
        kind: RejectKind,
        /// The decode error.
        error: WireError,
    },
    /// A peer's connection grade changed (spec 120 § 13.1, ADR-8182): with hysteresis, at most
    /// one change per [`crate::quality::DEGRADE_HOLD_MS`], and `Interrupted` once per outage.
    /// [`NetServer::peer_quality`] has the signals behind it.
    QualityChanged {
        /// The peer.
        peer: PeerId,
        /// The grade shown before.
        from: LinkGrade,
        /// The grade shown now.
        to: LinkGrade,
    },
}

/// What [`ServerEvent::Rejected`] (or the replication runtime's `NetRejection`) refused
/// (ADR-7892, ADR-7984; spec 120 § 11.2). New kinds are added as the hostile-input policy grows, so
/// a match on it needs a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejectKind {
    /// An input bundle whose decode reported a field over its cap ([`WireError::TooLong`]): a
    /// bundle of more than [`crate::tick::MAX_BUNDLE_INPUTS`] inputs, or a field of the game's input
    /// codec (a command tail, say) longer than the codec allows. A correct client never sends it,
    /// so it is a protocol violation: the sender is disconnected at once with
    /// [`DisconnectReason::ProtocolError`], the way the connection layer treats an over-long
    /// field. Other peers are not affected.
    InputTooLong,
    /// An input bundle that failed to decode otherwise: truncated, a value the game's codec
    /// refuses, or bytes left over after the bundle. Scores [`MALFORMED_WEIGHT`] malformed points.
    InputMalformed,
    /// A game message that failed to decode, or left bytes over. Scores [`MALFORMED_WEIGHT`]
    /// malformed points.
    MessageMalformed,
    /// A message on a lane the server does not know, or with no lane byte. Scores
    /// [`MALFORMED_WEIGHT`] malformed points.
    UnknownLane,
    /// A reliable message reassembled past `LinkConfig::max_reliable_message_bytes`. Fatal.
    ReliableTooLong,
    /// More reliable messages waited than the connection holds. Fatal.
    InboxFull,
    /// A reliable fragment longer than one packet of the link's configuration carries. Fatal
    /// (spec 120 § 5.2, ADR-7981).
    FragmentTooLong,
    /// The peer made the server store more out-of-order fragments than its connection's cap.
    /// Fatal (spec 120 § 5.2, ADR-7981).
    ReassemblyOverCap,
    /// The peer left a reliable message unfinished, or a gap before stored fragments unfilled,
    /// for `max(4 x RTO, 2 s)` while it kept sending. Fatal (spec 120 § 5.2, ADR-7981).
    ReassemblyStalled,
    /// The peer stayed over its inbound packet, byte or message rate for
    /// `InboundRateConfig::flood_disconnect_ms`, or sent more reliable messages in one receive than
    /// [`MAX_PEER_MESSAGES_PER_RECEIVE`]. Fatal; the [`ServerEvent::PeerLeft`] that follows has
    /// [`DisconnectReason::Flood`] (spec 120 § 11.3, ADR-8181).
    Flood,
    /// An input (bundle or reliable input) from a spectator, who has no seat to drive. Refused
    /// undecoded; scores [`MALFORMED_WEIGHT_MISUSE`] malformed point (ADR-7984, ADR-8606).
    SpectatorInput,
    /// A voice packet that did not parse (it spent a token of the sender's voice rate bucket
    /// first), or a voice control message with an unknown kind or an over-long block account
    /// (ADR-8438). Raised by the replication runtime for the voice relay; scores
    /// [`MALFORMED_WEIGHT`] malformed points (ADR-7984, ADR-8606).
    VoiceMalformed,
    /// A message type only the server may send (`ToOwner`, `Broadcast`) arrived from a client.
    /// Raised by the replication runtime. A correct client never sends one, so it is fatal: the
    /// sender is disconnected with [`DisconnectReason::ProtocolError`] (ADR-7984).
    ServerOnlyMessage,
    /// The peer stored more out-of-order fragments on one stream than that stream's window, past
    /// the flow-control credit it was given. Fatal (spec 120 § 5.2, ADR-8302).
    StreamOverCap,
    /// The peer sent on a reliable stream the table does not declare. Fatal (ADR-8302).
    UnknownStream,
}

impl From<LinkFailure> for RejectKind {
    fn from(failure: LinkFailure) -> Self {
        match failure {
            LinkFailure::MessageTooLong => Self::ReliableTooLong,
            LinkFailure::InboxFull => Self::InboxFull,
            LinkFailure::FragmentTooLong => Self::FragmentTooLong,
            LinkFailure::ReassemblyOverCap => Self::ReassemblyOverCap,
            LinkFailure::ReassemblyStalled => Self::ReassemblyStalled,
            LinkFailure::StreamOverCap => Self::StreamOverCap,
            LinkFailure::UnknownStream => Self::UnknownStream,
        }
    }
}

impl RejectKind {
    /// Stable lowercase name (`input_too_long`, `input_malformed`, `message_malformed`,
    /// `unknown_lane`, `reliable_too_long`, `inbox_full`, `fragment_too_long`,
    /// `reassembly_over_cap`, `reassembly_stalled`, `flood`, `spectator_input`, `voice_malformed`,
    /// `server_only_message`, `stream_over_cap`, `unknown_stream`), for logs and the agent
    /// protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InputTooLong => "input_too_long",
            Self::InputMalformed => "input_malformed",
            Self::MessageMalformed => "message_malformed",
            Self::UnknownLane => "unknown_lane",
            Self::ReliableTooLong => "reliable_too_long",
            Self::InboxFull => "inbox_full",
            Self::FragmentTooLong => "fragment_too_long",
            Self::ReassemblyOverCap => "reassembly_over_cap",
            Self::ReassemblyStalled => "reassembly_stalled",
            Self::Flood => "flood",
            Self::SpectatorInput => "spectator_input",
            Self::VoiceMalformed => "voice_malformed",
            Self::ServerOnlyMessage => "server_only_message",
            Self::StreamOverCap => "stream_over_cap",
            Self::UnknownStream => "unknown_stream",
        }
    }

    /// Every kind, in declaration order (for docs, tests and agent tables).
    pub const ALL: [RejectKind; 7] = [
        Self::InputTooLong,
        Self::InputMalformed,
        Self::MessageMalformed,
        Self::UnknownLane,
        Self::SpectatorInput,
        Self::VoiceMalformed,
        Self::ServerOnlyMessage,
    ];

    /// Malformed points one rejection of this kind scores (ADR-8606): [`MALFORMED_WEIGHT`] for an
    /// authenticated message that failed to decode or named an unknown lane
    /// (`InputMalformed`, `MessageMalformed`, `UnknownLane`, `VoiceMalformed`: no lossy, reordering
    /// or migrating link produces one, because a damaged datagram fails its tag and a late or
    /// duplicated one is dropped before decode), [`MALFORMED_WEIGHT_MISUSE`] for `SpectatorInput`,
    /// and 0 for the fatal kinds, which remove the peer instead of scoring. This is the weight of
    /// the server's transport score; the replication runtime's own voice score (ADR-7984) still
    /// charges one point per refusal against its pass credit.
    pub fn malformed_weight(self) -> u32 {
        match self {
            Self::InputMalformed | Self::MessageMalformed | Self::UnknownLane | Self::VoiceMalformed => {
                MALFORMED_WEIGHT
            }
            Self::SpectatorInput => MALFORMED_WEIGHT_MISUSE,
            _ => 0,
        }
    }

    /// True when the rejection disconnects the sender at once.
    pub fn is_fatal(self) -> bool {
        !matches!(
            self,
            Self::InputMalformed
                | Self::MessageMalformed
                | Self::UnknownLane
                | Self::SpectatorInput
                | Self::VoiceMalformed
        )
    }
}

/// What opening a sealed record under a connected peer's keys gave (ADR-8666).
#[derive(Debug)]
enum Opened {
    /// It authenticated and names a kind.
    Record(SealedKind, Vec<u8>),
    /// It authenticated, but its kind byte names no kind: already counted and scored.
    UnknownKind,
    /// It failed its open (forged, replayed, damaged): counted, charged to no one.
    Failed,
}

/// How a join reached the admission checks.
#[derive(Debug)]
enum JoinPath {
    /// A plain link's `ChallengeResponse`.
    Plain,
    /// The sealed `Join` of the pending handshake at the join's address.
    Sealed,
    /// The key confirmation of a pending one-round-trip resume (ADR-8858 A1, ADR-8902). Its
    /// accept is with the client already, so nothing is minted and no accept is sent, and a
    /// refusal is a sealed `Disconnect` on the pending link.
    Resume(Box<PendingResume>),
}

#[derive(Debug)]
struct PeerConn {
    conn: Connection,
    token: u32,
    client_nonce: u64,
    last_sync_ms: Option<u64>,
    pending_sync: Option<u64>,
    inputs_too_far_ahead: u64,
    /// This player's input arrivals for its [`LinkFeedback`] (ADR-8077).
    arrivals: InputArrivals,
    /// The feedback queued and not yet packed.
    pending_feedback: Option<u64>,
    /// The last feedback this peer was sent, for `net.peers`.
    last_feedback: Option<LinkFeedback>,
    /// This peer's connection quality (ADR-8182).
    quality: QualityTracker,
    link: Option<Box<SecureLink>>,
    /// Unreliable messages delivered since the current `receive` began.
    unreliable: u32,
    /// Reliable messages delivered since the current `receive` began.
    reliable: u32,
    /// An `Accept` was sent again for a resent `Join` in the current `receive` (ADR-8734): one
    /// answer per receive is all a lost accept needs.
    accept_resent: bool,
    /// Rate buckets, flood timer and malformed score (ADR-8181).
    inbound: PeerInbound,
    /// Candidate addresses of an address change, waiting for path validation (ADR-8299).
    path: PathState,
    /// Validated moves to a new address.
    rebinds: u64,
    /// Records from a new address that were not let in (budget, ban, stale, key).
    rebinds_refused: u64,
    /// The resume ticket this connection's accept carried (spec 120 § 10.3).
    ticket: [u8; RESUME_TICKET_BYTES],
    /// The resumption ticket this connection's accept carried (wire 6, ADR-8858): kept so a
    /// resent accept carries the same one. `None` on a plain link.
    resumption: Option<[u8; RESUMPTION_TICKET_BYTES]>,
    /// The peer's resume epochs (ADR-8858 A2): they travel with the peer through dormancy and
    /// takeover.
    epochs: ResumeEpochs,
    /// The epoch of `resumption`, confirmed by this connection's first authenticated packet
    /// (0: no ticket was minted).
    accept_epoch: u32,
    /// Store-ticket bytes the hook accepted, so leave and a late refusal can end that store
    /// session. Not the resume `ticket` above: that one is minted by this server.
    store_ticket: Option<Vec<u8>>,
    /// Every older ticket of this peer the client may still hold, until it proves it holds this
    /// connection's accept (its first authenticated payload, ADR-8535). Until then the accept, and
    /// the new ticket in it, can be lost, or heard while an earlier attempt's datagram still
    /// carries an older ticket, so a retry with any of them resumes the same peer instead of
    /// joining as a new one. Bounded in number and time (ADR-8623).
    retry_tickets: UnconfirmedTokens,
}

/// A player whose connection timed out, held for a resume (spec 120 § 10.3).
#[derive(Debug, Clone)]
struct Dormant {
    ticket: [u8; RESUME_TICKET_BYTES],
    /// The address the player timed out on: its block keeps the player's per-address slot for
    /// the grace (ADR-8733).
    ip: Option<IpAddr>,
    /// The store session the player held, kept open for the resume and ended with the hold.
    store_ticket: Option<Vec<u8>>,
    /// The timed-out connection's `PeerConn::retry_tickets`: its client never confirmed the
    /// accept, so it may still hold any of these older tickets (ADR-8535, ADR-8623).
    retry_tickets: UnconfirmedTokens,
    /// The timed-out connection's resume epochs (ADR-8858 A2).
    epochs: ResumeEpochs,
    /// The server clock at which the grace ends.
    until_ms: u64,
    /// The connection grade last shown for the player (`Interrupted` after the silence that
    /// timed it out): a resume shows the change back to the new connection's grade (ADR-8889).
    grade: LinkGrade,
}

/// Trial opens one candidate address may cost per peer per second: each is one AEAD attempt.
/// Counted per (peer, address) since ADR-8299, so forgeries from elsewhere cannot spend the budget
/// of the address the peer really moved to.
pub const REBIND_ATTEMPTS_PER_S: u32 = 8;
/// Before ADR-8299 a peer's address changed at most once in this long. Path validation replaced
/// the interval (a move needs the far end to answer a probe on the new path, so copied records
/// cannot make the traffic flap), and nothing reads it any more; it stays for source compatibility.
pub const REBIND_MIN_INTERVAL_MS: u64 = 1_000;

/// Join answers the admitted-nonce set remembers at most; past it the oldest row is evicted first.
const MAX_ADMITTED_NONCES: usize = 4_096;

/// Join answers that made a peer, by canonical IP and client nonce, for as long as the cookie they
/// carried could still verify (two cookie buckets after admission). The cookie and the password
/// nonce bind the IP but not the port (ADR-7946), so without this a captured plain join answer
/// replayed from another port of the same IP inside the window would make a second peer (review
/// row 21). Bounded: at most `MAX_ADMITTED_NONCES` rows, oldest evicted first; rows are pushed with
/// a monotonic expiry, so expired rows leave from the front.
#[derive(Debug, Default)]
struct AdmittedNonces {
    /// `(expires_ms, canonical IP, client nonce, address admitted from)`, oldest first.
    rows: VecDeque<(u64, IpAddr, u64, SocketAddr)>,
}

impl AdmittedNonces {
    /// Forgets rows whose cookie window closed.
    fn expire(&mut self, now_ms: u64) {
        while self.rows.front().is_some_and(|row| row.0 <= now_ms) {
            self.rows.pop_front();
        }
    }

    /// True when the answer with `nonce` from `from`'s IP already made a peer from another address
    /// and its cookie may still verify: a replay, not a new client.
    fn admitted_elsewhere(&mut self, from: SocketAddr, nonce: u64, now_ms: u64) -> bool {
        self.expire(now_ms);
        let ip = from.ip().to_canonical();
        self.rows
            .iter()
            .any(|&(_, row_ip, row_nonce, addr)| row_ip == ip && row_nonce == nonce && addr != from)
    }

    /// True when the answer with `nonce` already made a peer from `from` itself. Reached only
    /// after the "our accept was lost, answer again" check, so the peer it made no longer holds
    /// `from` with that nonce (a later attempt took it over, or it moved or went dormant): a
    /// datagram of that attempt still in flight, not a new client (ADR-8623). A live client whose
    /// answer this was starts a fresh attempt with a new nonce.
    fn admitted_here(&mut self, from: SocketAddr, nonce: u64, now_ms: u64) -> bool {
        self.expire(now_ms);
        self.rows
            .iter()
            .any(|&(_, _, row_nonce, addr)| row_nonce == nonce && addr == from)
    }

    /// Remembers an admitted answer until every cookie it could carry has expired.
    fn insert(&mut self, from: SocketAddr, nonce: u64, now_ms: u64) {
        self.expire(now_ms);
        if self.rows.len() >= MAX_ADMITTED_NONCES {
            self.rows.pop_front();
        }
        let expires_ms = now_ms.saturating_add(2 * crate::handshake::COOKIE_BUCKET_MS);
        self.rows
            .push_back((expires_ms, from.ip().to_canonical(), nonce, from));
    }
}

impl PeerConn {
    fn header_bytes(&self) -> usize {
        if self.link.is_some() {
            SEALED_RESERVED_HEADER_BYTES
        } else {
            CONNECTED_HEADER_BYTES
        }
    }
}

/// The authoritative server. See the module docs.
pub struct NetServer<G: NetGame, S: DatagramSocket> {
    socket: S,
    config: ServerConfig,
    /// Relay IPs (spec 120 § 10.6): every client relayed by one relay shares its IP, each on its
    /// own allocation port, so the per-IP caps count these by address instead.
    relay_ips: Vec<std::net::IpAddr>,
    protocol_id: u64,
    /// The run-time content hash clients must match ([`NetServer::set_content_hash`]); `None`
    /// checks no content beyond the protocol id.
    runtime_content: Option<u64>,
    /// Answers the plaintext version probe on this socket (spec 120 § 12.2, ADR-7985).
    probe: ProbeResponder,
    /// The last join refused for `protocol_mismatch`, from the refused client's side.
    version_mismatch: Option<VersionMismatchReport>,
    minter: CookieMinter,
    gate: PasswordGate,
    identity: Option<ServerIdentity>,
    pending: PendingTable,
    key_governor: HandshakeGovernor,
    /// Pre-proof answers (ADR-4936): per-prefix fair shares under the server-wide cap, with a
    /// refund when the cookie of that exact answer comes back, once (spec 120 § 11.4, ADR-8718,
    /// ADR-8764).
    answer_governor: crate::answer_share::AnswerShare,
    session: Session,
    /// Join attempts per source, after the cookie proved the address.
    joins: SourceLimiter,
    /// Key exchanges per source, after the cookie proved the address: each costs curve work.
    key_exchanges: SourceLimiter,
    /// Join answers that made a peer, so a copy from another port of the IP makes no second one.
    admitted: AdmittedNonces,
    /// Every `ServerEvent::Rejected` counted per peer and code, peers that left included
    /// (KI-NT-025, ADR-10077; `net.rejections`).
    rejections_by_peer: crate::status::RejectionsByPeer,
    conns: BTreeMap<PeerId, PeerConn>,
    /// Session token -> peer, so a record from a new address finds its peer in one probe
    /// (ADR-8299).
    tokens: HashMap<u32, PeerId>,
    /// Trial opens per (peer, candidate address) per second (ADR-8299, adversarial row 12).
    path_limiter: CandidateLimiter,
    /// The shared uplink (spec 120 § 4.2 item 8), when configured.
    uplink: Option<crate::cc::SharedUplink>,
    driver: TickDriver<G::Input>,
    conditioner: NetConditioner,
    governor: ReliableSendGovernor<PeerId>,
    /// Out-of-order reliable fragments stored across every peer (spec 120 § 5.2).
    reassembly: ReassemblyBudget,
    /// The inbound rates in force (ADR-8181).
    inbound_rates: InboundRates,
    /// Datagrams of this receive one connected peer may have handled.
    peer_slice: u32,
    /// Datagrams of this receive not attributed to a connected peer that got real work.
    unattributed: usize,
    /// Storage [`NetServer::send`] encodes into before it copies the message at its exact size
    /// (ADR-10081).
    encode_scratch: Vec<u8>,
    events: VecDeque<ServerEvent<G>>,
    totals: EndpointTotals,
    token_counter: u64,
    /// Dormant players by id (spec 120 § 10.3).
    dormant: BTreeMap<PeerId, Dormant>,
    /// Per-address slots held for players that left on purpose holding a ticket (ADR-8733).
    address_holds: crate::address_hold::AddressHolds,
    /// The held slot each peer claimed by its join and has not confirmed yet (ADR-8883): spent,
    /// and counted in `held_slot_returns`, at its first authenticated payload; spent uncounted when
    /// the peer is gone first (a dormant peer keeps its claim).
    hold_claims: HashMap<PeerId, [u8; RESUME_TICKET_BYTES]>,
    ticket_counter: u64,
    /// The keys of the resumption tickets (wire 6, ADR-8858): stateless, rotated every resume
    /// grace.
    ticket_keys: TicketKeys,
    /// Why the last `ResumeInit` was not resumed.
    last_resume_refusal: Option<ResumeRefusal>,
    /// Pending one-round-trip resumes (ADR-8858 A1, A3-A5): answered, not yet confirmed.
    resumes: ResumeRegister,
    /// Resumes not answered or not admitted, by [`ResumeRefusal::index`].
    resume_refusals: [u64; ResumeRefusal::ALL.len()],
    now_ms: u64,
    /// The unix-epoch second the monotonic clock started from (the wall second at
    /// construction). Until the host injects a clock explicitly, ban decisions read
    /// `unix_base_s + now_ms / 1000`, so a directly driven server still honors timed bans
    /// (audit fixrev-15-k net3-determinism H1, ADR-5444).
    unix_base_s: u64,
    /// Unix-epoch seconds on the injected clock, for ban decisions (ADR-4772). Derived from
    /// [`Self::unix_base_s`] and the monotonic progression until the first
    /// [`Self::set_unix_now_s`]; afterwards the injected value alone drives it.
    now_unix_s: u64,
    /// True after the first [`Self::set_unix_now_s`]; the injected clock replaces the derivation.
    unix_injected: bool,
    /// Store-ticket hook. `None` leaves admission unchanged.
    ticket_check: Option<Box<dyn JoinTicketCheck>>,
    closed: bool,
    _game: PhantomData<fn() -> G>,
}

impl<G: NetGame, S: DatagramSocket> std::fmt::Debug for NetServer<G, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetServer")
            .field("game", &G::GAME_ID)
            .field("peers", &self.session.len())
            .field("tick", &self.driver.tick())
            .finish_non_exhaustive()
    }
}

impl<G: NetGame, S: DatagramSocket> NetServer<G, S> {
    /// A server on `socket` with a fresh random secret.
    pub fn new(socket: S, config: ServerConfig) -> Self {
        Self::with_secret(socket, config, fresh_secret())
    }

    /// A server with a fixed secret (deterministic tests).
    pub fn with_secret(socket: S, config: ServerConfig, secret: [u8; 32]) -> Self {
        let minter = CookieMinter::new(secret);
        let gate = match &config.password {
            Some(pw) => {
                let mut salt = [0u8; 16];
                salt.copy_from_slice(&blake3::keyed_hash(&secret, b"regia_net salt").as_bytes()[..16]);
                PasswordGate::with_password(pw, salt)
            }
            None => PasswordGate::open(),
        };
        let identity = config
            .crypto
            .can_encrypt()
            .then(|| {
                config
                    .identity
                    .clone()
                    .or_else(|| ServerIdentity::derived(&secret))
            })
            .flatten();
        let unix_base_s = crate::session::unix_now_s();
        Self {
            socket,
            relay_ips: Vec::new(),
            protocol_id: protocol_id::<G>(),
            runtime_content: None,
            probe: ProbeResponder::with_secret(
                VersionStamp::of::<G>(),
                crate::version_probe::PROBE_ANSWERS_PER_PREFIX_PER_S,
                blake3::derive_key("regia_net probe limiter v1", &secret),
            ),
            version_mismatch: None,
            minter,
            gate,
            identity,
            pending: PendingTable::new(config.max_peers),
            key_governor: HandshakeGovernor::new(config.max_handshakes_per_s),
            answer_governor: crate::answer_share::AnswerShare::new(config.pre_proof_answers_per_s),
            session: Session::new(config.max_peers, config.health),
            joins: SourceLimiter::new(
                config.handshakes_per_ip_per_s,
                MAX_HANDSHAKE_SOURCES,
                blake3::derive_key("regia_net join limiter v1", &secret),
            ),
            key_exchanges: SourceLimiter::new(
                config.handshakes_per_ip_per_s,
                MAX_HANDSHAKE_SOURCES,
                blake3::derive_key("regia_net key exchange limiter v1", &secret),
            ),
            admitted: AdmittedNonces::default(),
            rejections_by_peer: crate::status::RejectionsByPeer::default(),
            conns: BTreeMap::new(),
            tokens: HashMap::new(),
            uplink: (config.shared_uplink || config.uplink_bytes_per_second > 0)
                .then(|| crate::cc::SharedUplink::new(config.uplink_bytes_per_second)),
            path_limiter: CandidateLimiter::new(
                REBIND_ATTEMPTS_PER_S,
                blake3::derive_key("regia_net path candidate limiter v1", &secret),
            ),
            driver: {
                // Spec 120 § 6.3: an input that misses its tick merges into the next one.
                let mut driver = TickDriver::new(config.input_buffer_ticks);
                driver.set_late_merge(Some(<G::Input as NetCodec>::merge_late));
                driver
            },
            conditioner: NetConditioner::new(config.conditioner.clone()),
            governor: ReliableSendGovernor::new(config.reliable_msgs_per_s),
            reassembly: ReassemblyBudget::new(config.reassembly_budget_bytes()),
            inbound_rates: config.inbound.resolve(
                config.tick_rate_hz,
                config.link.max_packet_bytes,
                config.link.max_packets_per_poll,
                MAX_PEER_MESSAGES_PER_RECEIVE,
            ),
            peer_slice: MIN_PEER_SLICE,
            unattributed: 0,
            encode_scratch: Vec::new(),
            events: VecDeque::new(),
            totals: EndpointTotals::default(),
            token_counter: 0,
            dormant: BTreeMap::new(),
            address_holds: crate::address_hold::AddressHolds::default(),
            hold_claims: HashMap::new(),
            ticket_counter: 0,
            ticket_keys: TicketKeys::new(&secret, config.resume_grace_ms),
            last_resume_refusal: None,
            resumes: ResumeRegister::new(config.max_peers),
            resume_refusals: [0; ResumeRefusal::ALL.len()],
            now_ms: 0,
            unix_base_s,
            now_unix_s: unix_base_s,
            unix_injected: false,
            ticket_check: None,
            closed: false,
            config,
            _game: PhantomData,
        }
    }

    /// The configuration.
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }

    /// Installs the store-ticket hook. It runs after the password gate, after
    /// `NetGame::validate_join`, and after the live-peer, per-address and capacity checks,
    /// immediately before the peer is added. A refusal denies the join with detail
    /// `ticket_refused`. A successful check's store session is ended when that peer leaves and
    /// when admission still fails afterwards. Replacing the hook drops the previous one. The host
    /// password stays the access control: a wrong password never reaches the hook.
    pub fn set_ticket_check(&mut self, check: impl JoinTicketCheck + 'static) {
        self.ticket_check = Some(Box::new(check));
    }

    /// Applies a late store verdict for a peer the hook admitted as [`TicketVerdict::Pending`].
    ///
    /// `admitted == true` leaves the peer in place. The store session still ends when the peer
    /// leaves. `false` removes the peer with [`DisconnectReason::Kicked`] and ends the session.
    /// Returns false when `peer` is not connected or joined with no ticket.
    pub fn finish_ticket(&mut self, peer: PeerId, admitted: bool) -> bool {
        let has_ticket = self.conns.get(&peer).is_some_and(|pc| pc.store_ticket.is_some());
        if !has_ticket {
            return false;
        }
        if admitted {
            return true;
        }
        self.drop_peer(peer, DisconnectReason::Kicked, true)
    }

    /// The bound address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.socket.local_addr().ok()
    }

    /// Peers and bans.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// The tick driver (read).
    pub fn driver(&self) -> &TickDriver<G::Input> {
        &self.driver
    }

    /// The tick driver (write), for games that drive ticks themselves.
    pub fn driver_mut(&mut self) -> &mut TickDriver<G::Input> {
        &mut self.driver
    }

    /// Endpoint counters.
    pub fn totals(&self) -> EndpointTotals {
        self.totals
    }

    /// The server-wide reassembly budget: its limit and the out-of-order reliable bytes all peers
    /// hold now (spec 120 § 5.2, ADR-7981).
    pub fn reassembly_budget(&self) -> ReassemblyBudget {
        self.reassembly
    }

    /// The public key clients pin; `None` when this server does not encrypt.
    pub fn server_key(&self) -> Option<ServerKey> {
        self.identity.as_ref().map(ServerIdentity::public)
    }

    /// True when `peer`'s link is sealed.
    pub fn peer_sealed(&self, peer: PeerId) -> bool {
        self.conns.get(&peer).is_some_and(|pc| pc.link.is_some())
    }

    /// The Noise handshake hash of `peer`'s sealed link (channel binding: both sides of one link
    /// hold the same 32 bytes, no other link does); `None` for a plain or unknown peer.
    pub fn peer_handshake_hash(&self, peer: PeerId) -> Option<[u8; 32]> {
        self.conns
            .get(&peer)
            .and_then(|pc| pc.link.as_ref())
            .map(|link| *link.handshake_hash())
    }

    /// Replaces the outbound conditioner configuration.
    pub fn set_conditioner(&mut self, config: ConditionerConfig) {
        self.conditioner.set_config(config);
    }

    /// The conditioner.
    pub fn conditioner(&self) -> &NetConditioner {
        &self.conditioner
    }

    /// Next queued event.
    pub fn poll_event(&mut self) -> Option<ServerEvent<G>> {
        self.events.pop_front()
    }

    /// Unix-epoch seconds on the clock ban decisions read (ADR-4772): the wall second at
    /// construction plus the monotonic progression, until the first [`Self::set_unix_now_s`]
    /// replaces the derivation with the injected value.
    pub fn now_unix_s(&self) -> u64 {
        self.now_unix_s
    }

    /// Injects the unix-epoch second for ban decisions (`is_banned` checks, [`Self::ban`] and
    /// [`Self::ban_ip`] expiries). The owning layer sets it once per step from its own clock;
    /// tests and replays supply the recorded value so the peer set is a function of the session
    /// inputs, not of the host's system clock. The injection takes over from the derived clock
    /// for good: a session layer that injects keeps injecting every step.
    pub fn set_unix_now_s(&mut self, now_unix_s: u64) {
        self.now_unix_s = now_unix_s;
        self.unix_injected = true;
    }

    /// Recomputes the derived unix clock after `now_ms` moved. A no-op once the host injected a
    /// clock explicitly (ADR-4772): the injected value is authoritative from then on.
    fn stamp_unix_clock(&mut self) {
        if !self.unix_injected {
            self.now_unix_s = self.unix_base_s.saturating_add(self.now_ms / 1000);
        }
    }

    /// Marks `ip` as a relay's address (spec 120 § 10.6, ADR-8183): every client that relay
    /// forwards shares this IP, each on its own allocation port, so the per-IP peer cap
    /// (`ServerConfig::max_peers_per_ip`) and the per-IP handshake limits do not apply to it; the
    /// relay caps clients per host and per source itself, and the server-wide handshake limits
    /// still apply. At most 16 relay IPs are kept (the oldest is forgotten).
    pub fn add_relay_ip(&mut self, ip: std::net::IpAddr) {
        if self.relay_ips.contains(&ip) {
            return;
        }
        if self.relay_ips.len() >= 16 {
            self.relay_ips.remove(0);
        }
        self.relay_ips.push(ip);
    }

    /// Whether `ip` was marked with [`Self::add_relay_ip`].
    pub fn is_relay_ip(&self, ip: std::net::IpAddr) -> bool {
        self.relay_ips.contains(&ip)
    }

    /// Reads and handles every waiting datagram, then removes silent peers and kicks peers whose
    /// store ticket was refused late (`JoinTicketCheck::take_refusals`).
    pub fn receive(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("regia_net::server_receive").entered();
        self.now_ms = self.now_ms.max(now_ms);
        self.stamp_unix_clock();
        let now = self.now_ms;
        for pc in self.conns.values_mut() {
            pc.unreliable = 0;
            pc.reliable = 0;
            pc.accept_resent = false;
            pc.inbound.begin_receive(now);
        }
        let share = MAX_DATAGRAMS_PER_RECEIVE / self.conns.len().max(1);
        self.peer_slice = u32::try_from(share).unwrap_or(u32::MAX).max(MIN_PEER_SLICE);
        self.unattributed = 0;
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        for _ in 0..MAX_DATAGRAMS_READ_PER_RECEIVE {
            match self.socket.recv_from(&mut buf) {
                Ok(Some((n, from))) => {
                    self.totals.datagrams_in += 1;
                    self.handle_datagram(&buf[..n], from);
                }
                Ok(None) => break,
                Err(e) => {
                    self.totals.socket_errors += 1;
                    tracing::debug!(error = %e, "regia_net server receive error");
                    break;
                }
            }
        }
        self.end_inbound_receive();
        let interrupt_ms = self.config.health.timeouts().sanitized().interrupt_ms;
        // Silent for the interrupt time, or the rate controller's circuit breaker tripped: the path
        // cannot carry even the essential floor (spec 120 § 4.2 item 4).
        let interrupted: Vec<_> = self
            .session
            .peers()
            .filter(|p| {
                !p.interrupted
                    && (self.now_ms.saturating_sub(p.last_seen_ms) >= interrupt_ms
                        || self
                            .conns
                            .get(&p.id)
                            .is_some_and(|pc| pc.conn.rate_controller().breaker_tripped()))
            })
            .map(|p| p.id)
            .collect();
        for peer in interrupted {
            if let Some(p) = self.session.peer_mut(peer) {
                p.interrupted = true;
            }
            // The server keeps its keepalive floor toward a silent peer: only the client backs
            // off to probes (spec 120 § 10.2). A client that still hears the server must not see
            // gaps longer than its own interruption threshold (ADR-7995).
            self.events.push_back(ServerEvent::PeerInterrupted { peer });
            let now = self.now_ms;
            if let Some(change) = self
                .conns
                .get_mut(&peer)
                .and_then(|pc| pc.quality.set_interrupted(true, now))
            {
                self.events.push_back(quality_event(peer, change));
            }
        }
        for peer in self.session.timed_out(self.now_ms) {
            self.drop_peer(peer, DisconnectReason::Timeout, true);
        }
        let now = self.now_ms;
        let expired: Vec<PeerId> = self
            .dormant
            .iter()
            .filter(|(_, d)| now >= d.until_ms)
            .map(|(p, _)| *p)
            .collect();
        for peer in expired {
            self.end_dormancy(peer, DisconnectReason::Timeout);
        }
        self.pending.expire(self.now_ms);
        self.totals.resume_lapsed += self.resumes.expire(self.now_ms) as u64;
        // Here, not in `update`: the ECS loop calls `receive` and `flush` only (net-rev-b #11).
        self.apply_ticket_refusals();
    }

    /// Feeds each peer's flood timer with this receive and removes the peers that stayed over
    /// their inbound rate for `flood_disconnect_ms` (spec 120 § 11.3, ADR-8181).
    fn end_inbound_receive(&mut self) {
        let (now, limit) = (self.now_ms, self.config.inbound.flood_disconnect_ms);
        let flooded: Vec<PeerId> = self
            .conns
            .iter_mut()
            .filter_map(|(id, pc)| pc.inbound.end_receive(now, limit).then_some(*id))
            .collect();
        for peer in flooded {
            self.remove_for_flood(peer);
        }
    }

    /// Names and removes a flooding peer.
    fn remove_for_flood(&mut self, peer: PeerId) {
        let flood_ms = self.conns.get(&peer).map_or(0, |pc| pc.inbound.flood_ms());
        tracing::info!(target: "regia_net", %peer, flood_ms, "peer removed for flooding");
        self.totals.flood_disconnects += 1;
        self.raise_rejected(
            peer,
            RejectKind::Flood,
            WireError::Invalid("over the inbound rate"),
        );
        self.drop_peer(peer, DisconnectReason::Flood, true);
    }

    /// Raises [`ServerEvent::Rejected`] and counts it on the peer's row of
    /// [`Self::rejections_by_peer`] (KI-NT-025, ADR-10077).
    fn raise_rejected(&mut self, peer: PeerId, kind: RejectKind, error: WireError) {
        self.rejections_by_peer.note(peer.0, kind.as_str());
        self.events.push_back(ServerEvent::Rejected { peer, kind, error });
    }

    /// Every refusal this server raised as [`ServerEvent::Rejected`], counted per peer and
    /// lowercase code; a peer's row stays after it leaves, up to
    /// [`crate::status::MAX_REJECTION_PEERS`] peers (KI-NT-025, ADR-10077). The agent reads it as
    /// `net.rejections`.
    pub fn rejections_by_peer(&self) -> &crate::status::RejectionsByPeer {
        &self.rejections_by_peer
    }

    /// True when a datagram that no connected peer owns may still get real work this receive.
    fn take_unattributed(&mut self) -> bool {
        if self.unattributed >= MAX_DATAGRAMS_PER_RECEIVE {
            self.totals.over_rate += 1;
            return false;
        }
        self.unattributed += 1;
        true
    }

    /// Charges one datagram of `len` bytes to `peer` before any cipher work; true when it is over
    /// the peer's rate or slice and must be dropped unopened (ADR-8181).
    fn over_rate(&mut self, peer: PeerId, len: usize) -> bool {
        if !self.config.inbound.enabled {
            return false;
        }
        let (now, slice) = (self.now_ms, self.peer_slice);
        let Some(pc) = self.conns.get_mut(&peer) else {
            return false;
        };
        match pc.inbound.admit_packet(now, len, slice) {
            PacketVerdict::Admit => false,
            PacketVerdict::OverRate => {
                self.totals.over_rate += 1;
                true
            }
        }
    }

    /// Gives `peer` back the rate tokens and slice place of an admitted sealed record that failed
    /// its open (ADR-8606): a forged or replayed record from the peer's address is not the peer's.
    fn refund_unopened(&mut self, peer: PeerId, len: usize) {
        if !self.config.inbound.enabled {
            return;
        }
        if let Some(pc) = self.conns.get_mut(&peer) {
            pc.inbound.refund_packet(len);
        }
    }

    /// Advances the authoritative tick by one: hands the inputs that arrived for it to `step` and
    /// updates each peer's missed-input count.
    pub fn advance_tick<F: FnMut(u64, &TickInputs<G::Input>)>(&mut self, step: F) -> TickInputs<G::Input> {
        let inputs = self.driver.advance(step);
        let peers: Vec<PeerId> = self.driver.peers().collect();
        for peer in peers {
            let missed = self.driver.missed(peer);
            if let Some(p) = self.session.peer_mut(peer) {
                p.missed_input_ticks = missed;
            }
            if let Some(pc) = self.conns.get_mut(&peer) {
                pc.arrivals.note_missed(missed);
            }
        }
        inputs
    }

    /// Queues a game message for one peer.
    pub fn send(&mut self, peer: PeerId, channel: Channel, message: &G::Message) -> Result<(), SendError> {
        let bytes = self.encode_game_message(message);
        self.send_raw(peer, channel, bytes)
    }

    /// `message` on the game lane, encoded into storage the server keeps and copied once at its
    /// exact size (ADR-10081): a writer that grows from empty reallocates about ten times for a
    /// 1 KiB message, once per peer per tick for a state message.
    fn encode_game_message(&mut self, message: &G::Message) -> Vec<u8> {
        let mut w = WireWriter::reusing(std::mem::take(&mut self.encode_scratch));
        w.u8(LANE_GAME);
        message.encode(&mut w);
        let bytes = w.as_slice().to_vec();
        let scratch = w.finish();
        if scratch.capacity() <= ENCODE_SCRATCH_MAX_BYTES {
            self.encode_scratch = scratch;
        }
        bytes
    }

    /// Queues a game message in the essential class, ahead of the reliable stream and inside the
    /// rate controller's essential floor: bridge input feedback (ADR-8032) and lockstep host
    /// frames (ADR-8487).
    pub(crate) fn send_essential_message(
        &mut self,
        peer: PeerId,
        message: &G::Message,
    ) -> Result<(), SendError> {
        if !self.conns.contains_key(&peer) {
            return Err(SendError::NotConnected);
        }
        let bytes = self.encode_game_message(message);
        let pc = self.conns.get_mut(&peer).ok_or(SendError::NotConnected)?;
        pc.conn.send_essential(bytes, pc.header_bytes())
    }

    /// Queues a game message for every peer; returns how many peers refused it.
    pub fn broadcast(&mut self, channel: Channel, message: &G::Message) -> usize {
        let mut w = WireWriter::new();
        w.u8(LANE_GAME);
        message.encode(&mut w);
        let bytes = w.finish();
        let peers: Vec<PeerId> = self.conns.keys().copied().collect();
        peers
            .into_iter()
            .filter(|peer| self.send_raw(*peer, channel, bytes.clone()).is_err())
            .count()
    }

    fn send_raw(&mut self, peer: PeerId, channel: Channel, bytes: Vec<u8>) -> Result<(), SendError> {
        if !self.conns.contains_key(&peer) {
            return Err(SendError::NotConnected);
        }
        if channel.is_reliable() && !self.governor.try_admit(peer, self.now_ms) {
            self.totals.rate_limited += 1;
            return Err(SendError::RateLimited);
        }
        let pc = self.conns.get_mut(&peer).ok_or(SendError::NotConnected)?;
        let header_bytes = pc.header_bytes();
        pc.conn.send(channel, bytes, header_bytes)
    }

    /// Removes a peer and tells it why.
    pub fn kick(&mut self, peer: PeerId, reason: DisconnectReason) -> bool {
        self.drop_peer(peer, reason, true)
    }

    /// Removes a peer without telling it (no disconnect datagram): for a link its client already
    /// replaced, such as the old link of a session's token takeover (spec 120 § 10.3).
    /// `PeerLeft` still reports `reason`.
    pub fn drop_quietly(&mut self, peer: PeerId, reason: DisconnectReason) -> bool {
        self.drop_peer(peer, reason, false)
    }

    /// Bans a peer's address block for `seconds` (`0` = permanently) and removes it, with every
    /// other peer of that block: its IPv4 address, or its whole IPv6 /64 (ADR-3283).
    pub fn ban(&mut self, peer: PeerId, reason: &str, seconds: u64) -> bool {
        let Some(ip) = self.session.peer(peer).map(|p| p.addr.ip()) else {
            return false;
        };
        self.ban_ip(ip, reason, seconds);
        true
    }

    /// Lifts the ban on `ip`'s block ([`Self::ban`], [`Self::ban_ip`]); any address of a banned
    /// IPv6 /64 lifts it. True when one existed (ADR-2665 (4)).
    pub fn unban_ip(&mut self, ip: IpAddr) -> bool {
        self.session.unban(ip)
    }

    /// Bans `ip`'s block (the IPv4 address, or the whole IPv6 /64, ADR-3283) for `seconds` (`0` =
    /// permanently), also when it holds no peer (a handshake flooder never gets one). Peers
    /// already in that block are removed.
    pub fn ban_ip(&mut self, ip: IpAddr, reason: &str, seconds: u64) {
        let expires = if seconds == 0 {
            0
        } else {
            self.now_unix_s.saturating_add(seconds)
        };
        self.session.ban(ip, reason.to_owned(), expires);
        let on_ip: Vec<PeerId> = self
            .session
            .peers()
            .filter(|p| same_block(p.addr.ip(), ip))
            .map(|p| p.id)
            .collect();
        for peer in on_ip {
            self.drop_peer(peer, DisconnectReason::Banned, true);
        }
    }

    /// Bans exactly `ip` — no other address of its IPv6 /64 — for `seconds` (`0` = permanently).
    /// Only the peer at `ip` itself is removed. Mirrors an explicit `/128` of a `bans.ron`
    /// address entry so the game port refuses one address, not the block (ADR-4936).
    pub fn ban_ip_exact(&mut self, ip: IpAddr, reason: &str, seconds: u64) {
        let expires = if seconds == 0 {
            0
        } else {
            self.now_unix_s.saturating_add(seconds)
        };
        self.session.ban_exact(ip, reason.to_owned(), expires);
        let ip = ip.to_canonical();
        let on_ip: Vec<PeerId> = self
            .session
            .peers()
            .filter(|p| p.addr.ip().to_canonical() == ip)
            .map(|p| p.id)
            .collect();
        for peer in on_ip {
            self.drop_peer(peer, DisconnectReason::Banned, true);
        }
    }

    /// Requires a content hash known only at run time (a mounted content set, a `NetLink` app's
    /// replicated schema; ADR-7985): it goes into this server's version stamp, and a client whose
    /// stamp names another content hash is refused with `protocol_mismatch` and a
    /// `content_differs` detail. A client that predates stamps sends none and is not checked.
    /// Call it before clients join: peers already connected keep their link.
    pub fn set_content_hash(&mut self, content_hash: u64) {
        self.runtime_content = Some(content_hash);
        let mut stamp = self.probe.stamp().clone();
        stamp.content_hash = crate::payload::combined_content_hash(G::content_hash(), content_hash);
        self.probe.set_stamp(stamp);
    }

    /// The run-time content hash of [`NetServer::set_content_hash`] (`None` = not checked).
    pub fn content_hash(&self) -> Option<u64> {
        self.runtime_content
    }

    /// Replaces the versions this server announces in its version probe answers and
    /// `protocol_mismatch` denies (a session host announces its manifest's game version). The
    /// protocol id is not changed.
    pub fn set_version_stamp(&mut self, stamp: VersionStamp) {
        self.probe.set_stamp(stamp);
    }

    /// The versions this server announces (spec 120 § 12.2).
    pub fn version_stamp(&self) -> &VersionStamp {
        self.probe.stamp()
    }

    /// The version probe responder: answers and per-prefix sheds so far.
    pub fn probe_responder(&self) -> &ProbeResponder {
        &self.probe
    }

    /// The last join refused for `protocol_mismatch`, seen from the refused client (`net.status`
    /// `version_mismatch`).
    pub fn version_mismatch(&self) -> Option<&VersionMismatchReport> {
        self.version_mismatch.as_ref()
    }

    /// Proved handshake packets dropped by the per-address limit.
    pub fn handshakes_refused(&self) -> u64 {
        self.joins.refused_total() + self.key_exchanges.refused_total()
    }

    /// Tells every peer the server is closing and refuses new joins. Call [`NetServer::flush`]
    /// afterwards.
    pub fn shutdown(&mut self) {
        let peers: Vec<PeerId> = self.conns.keys().copied().collect();
        for peer in peers {
            self.drop_peer(peer, DisconnectReason::ServerShutdown, true);
        }
        self.closed = true;
        let dormant: Vec<PeerId> = self.dormant.keys().copied().collect();
        for peer in dormant {
            self.end_dormancy(peer, DisconnectReason::ServerShutdown);
        }
    }

    /// Removes a peer and tells it why, as [`Self::kick`] does, and holds its id, slot and
    /// ticket for the resume grace, as a timeout does (ADR-10088): the peer is
    /// [`ServerEvent::PeerDormant`], not `PeerLeft`, and a join with its ticket resumes it. For a
    /// layer above that ends a link whose player it still holds (the session's grace slot).
    /// With `resume_grace_ms` 0, for a spectator and on a closed server it is [`Self::kick`].
    pub fn kick_held(&mut self, peer: PeerId, reason: DisconnectReason) -> bool {
        self.drop_peer_with(peer, reason, true, true)
    }

    fn drop_peer(&mut self, peer: PeerId, reason: DisconnectReason, notify: bool) -> bool {
        self.drop_peer_with(peer, reason, notify, false)
    }

    /// [`Self::drop_peer`]; `hold` keeps the peer dormant whatever the reason, as a timeout does.
    fn drop_peer_with(&mut self, peer: PeerId, reason: DisconnectReason, notify: bool, hold: bool) -> bool {
        let Some(mut pc) = self.conns.remove(&peer) else {
            return false;
        };
        if self.tokens.get(&pc.token) == Some(&peer) {
            self.tokens.remove(&pc.token);
        }
        self.reassembly.release(pc.conn.reliable_buffered_bytes());
        let addr = self.session.peer(peer).map(|p| p.addr);
        let was_player = self
            .session
            .peer(peer)
            .is_some_and(|p| p.role == PeerRole::Player);
        self.session.remove_peer(peer);
        self.driver.remove_peer(peer);
        self.governor.remove(&peer);
        if notify && let Some(addr) = addr {
            for _ in 0..DISCONNECT_REPEATS {
                // A sealed goodbye gets a fresh counter each time, or the copies would be replays.
                let bytes = match pc.link.as_mut() {
                    Some(link) => link.seal(SealedKind::Disconnect, &[reason.to_u8()]).ok(),
                    None => {
                        let mut w = WireWriter::new();
                        w.raw(&connected_header(PacketKind::Disconnect, pc.token));
                        w.u8(reason.to_u8());
                        Some(w.finish())
                    }
                };
                if let Some(bytes) = bytes {
                    self.totals.sealed_out += u64::from(pc.link.is_some());
                    self.emit(bytes, addr);
                }
            }
        }
        if (reason == DisconnectReason::Timeout || hold)
            && self.config.resume_grace_ms > 0
            && !self.closed
            && was_player
        {
            // Spec 120 § 10.3: the slot and the id wait for the peer's ticket.
            self.session.reserve(peer);
            self.dormant.insert(
                peer,
                Dormant {
                    ticket: pc.ticket,
                    ip: addr.map(|a| a.ip()),
                    store_ticket: pc.store_ticket.take(),
                    retry_tickets: std::mem::take(&mut pc.retry_tickets),
                    epochs: pc.epochs,
                    until_ms: self.now_ms.saturating_add(self.config.resume_grace_ms),
                    grade: pc.quality.quality().grade,
                },
            );
            self.totals.dormant += 1;
            tracing::info!(target: "regia_net", %peer, "peer dormant: held for a resume");
            self.events.push_back(ServerEvent::PeerDormant { peer, reason });
            return true;
        }
        // Not held as dormant: a held slot it claimed and never confirmed is free again.
        self.release_hold_claim(peer);
        if reason == DisconnectReason::Left
            && self.config.resume_grace_ms > 0
            && !self.closed
            && was_player
            && let Some(addr) = addr
        {
            // ADR-8733: a player that says goodbye still holds its ticket; its block keeps the
            // slot for the grace, so another player of the same address cannot lock it out.
            self.address_holds.expire(self.now_ms);
            let until = self.now_ms.saturating_add(self.config.resume_grace_ms);
            self.address_holds.insert(addr.ip(), pc.ticket, until);
        }
        self.end_store_ticket(pc.store_ticket.as_deref());
        tracing::info!(target: "regia_net", %peer, reason = reason.as_str(), "peer left");
        self.events.push_back(ServerEvent::PeerLeft { peer, reason });
        true
    }

    /// Ends a dormant player's hold now (ADR-10088): its id and slot are freed, its ticket
    /// resumes nothing any more, and it leaves with [`ServerEvent::PeerLeft`] (`timeout`); not
    /// counted in `dormant_expired`. For a layer above that keeps its own record of who may come
    /// back (the session's grace slot) and holds none for this player. False when `peer` is not
    /// dormant.
    pub fn forget_dormant(&mut self, peer: PeerId) -> bool {
        let Some(held) = self.dormant.remove(&peer) else {
            return false;
        };
        self.end_store_ticket(held.store_ticket.as_deref());
        self.session.release(peer);
        tracing::info!(target: "regia_net", %peer, "dormant peer forgotten: nothing above holds it");
        self.events.push_back(ServerEvent::PeerLeft {
            peer,
            reason: DisconnectReason::Timeout,
        });
        true
    }

    /// Ends a dormant peer's hold: its id and slot are freed and it leaves with `reason`.
    fn end_dormancy(&mut self, peer: PeerId, reason: DisconnectReason) {
        let Some(held) = self.dormant.remove(&peer) else {
            return;
        };
        self.release_hold_claim(peer);
        self.end_store_ticket(held.store_ticket.as_deref());
        self.session.release(peer);
        if reason == DisconnectReason::Timeout {
            self.totals.dormant_expired += 1;
        }
        tracing::info!(target: "regia_net", %peer, reason = reason.as_str(), "dormant peer left");
        self.events.push_back(ServerEvent::PeerLeft { peer, reason });
    }

    /// Spends, uncounted, the held slot `peer` claimed and never confirmed (ADR-8883): the peer is
    /// gone, so the slot it stood for is free.
    fn release_hold_claim(&mut self, peer: PeerId) {
        if let Some(ticket) = self.hold_claims.remove(&peer) {
            self.address_holds.take(&ticket);
        }
    }

    /// Ends the store session the hook opened for `ticket`, if any.
    fn end_store_ticket(&self, ticket: Option<&[u8]>) {
        if let Some(ticket) = ticket
            && let Some(hook) = &self.ticket_check
        {
            hook.end_ticket(ticket);
        }
    }

    /// Ends a resumed peer's earlier store session once a new join replaced it. Nothing to end
    /// when the new join carried the same ticket: that session is now the new connection's.
    fn end_replaced_store_ticket(&self, old: Option<Vec<u8>>, new: Option<&[u8]>) {
        if let Some(old) = old
            && Some(old.as_slice()) != new
        {
            self.end_store_ticket(Some(&old));
        }
    }

    /// Dormant players (spec 120 § 10.3) and the server clock at which each one's grace ends.
    pub fn dormant_peers(&self) -> impl Iterator<Item = (PeerId, u64)> + '_ {
        self.dormant.iter().map(|(p, d)| (*p, d.until_ms))
    }

    /// Host migration (spec 120 § 10.5, ADR-8901): holds `peer` as a dormant player of this server
    /// with `ticket`, as if it had timed out here, so the client that presents the ticket in its
    /// join comes back under the same `PeerId` ([`ServerEvent::PeerResumed`]). A promoted
    /// successor calls it for every player of the lost host before the first `receive`; the hold
    /// ends like any dormant peer's, after [`ServerConfig::resume_grace_ms`]
    /// ([`ServerEvent::PeerLeft`] with `Timeout`). No event is raised for the adoption itself.
    pub fn adopt_dormant(
        &mut self,
        peer: PeerId,
        ticket: [u8; RESUME_TICKET_BYTES],
    ) -> Result<(), AdoptError> {
        if self.closed {
            return Err(AdoptError::Closed);
        }
        if self.config.resume_grace_ms == 0 {
            return Err(AdoptError::ResumeOff);
        }
        if peer == PeerId::SERVER {
            return Err(AdoptError::ServerId);
        }
        if ticket.iter().all(|b| *b == 0) {
            return Err(AdoptError::ZeroTicket);
        }
        if self.conns.contains_key(&peer)
            || self.dormant.contains_key(&peer)
            || self.session.peer(peer).is_some()
            || self.session.reserved().any(|held| held == peer)
        {
            return Err(AdoptError::IdInUse);
        }
        if self.ticket_owner(&ticket).is_some() {
            return Err(AdoptError::TicketInUse);
        }
        self.session.reserve(peer);
        self.dormant.insert(
            peer,
            Dormant {
                ticket,
                // Adopted from the lost host: no address on this server holds a slot for it
                // (ADR-8733), no resume epoch has been used here (ADR-8858 A2), and the player was
                // last seen through the lost host, so a resume reports the change back from
                // `Interrupted` (ADR-8889; integ-v07b ADR-10076).
                ip: None,
                store_ticket: None,
                retry_tickets: UnconfirmedTokens::default(),
                epochs: ResumeEpochs::default(),
                until_ms: self.now_ms.saturating_add(self.config.resume_grace_ms),
                grade: LinkGrade::Interrupted,
            },
        );
        self.totals.dormant += 1;
        tracing::info!(target: "regia_net", %peer, "peer adopted dormant: held for a resume after a host migration");
        Ok(())
    }

    /// Host migration (spec 120 § 10.5, ADR-8901): keeps `peer` for this server's own player (a
    /// promoted successor keeps the `PeerId` it had as a client). The id is never issued to a
    /// join and its slot counts as taken. False when the id is the server's, in use or already
    /// kept.
    pub fn keep_peer_id(&mut self, peer: PeerId) -> bool {
        if peer == PeerId::SERVER
            || self.conns.contains_key(&peer)
            || self.dormant.contains_key(&peer)
            || self.session.peer(peer).is_some()
            || self.session.reserved().any(|held| held == peer)
        {
            return false;
        }
        self.session.reserve(peer);
        true
    }

    /// Host migration (spec 120 § 10.5, ADR-8901): moves a server that has simulated nothing yet
    /// to `tick`, so the next [`Self::advance_tick`] simulates `tick` and the clients' tick sync
    /// carries on from the lost host's timeline. False (and nothing changes) once a tick was
    /// advanced.
    pub fn start_at_tick(&mut self, tick: u64) -> bool {
        if self.driver.tick() != 0 {
            return false;
        }
        self.driver.resume_at(tick);
        true
    }

    /// The peer a resume ticket belongs to: a dormant one, or a connected one (a takeover), and
    /// whether it matched an unconfirmed resume's older ticket (a retry after a lost accept,
    /// ADR-8535) rather than the newest one.
    fn ticket_owner(&self, ticket: &[u8; RESUME_TICKET_BYTES]) -> Option<(PeerId, bool)> {
        if self.config.resume_grace_ms == 0 || ticket.iter().all(|b| *b == 0) {
            return None;
        }
        // Every entry is compared, so the time taken does not say which one matched.
        let now = self.now_ms;
        let mut found = None;
        let held = self
            .dormant
            .iter()
            .map(|(peer, d)| (*peer, &d.ticket, &d.retry_tickets))
            .chain(
                self.conns
                    .iter()
                    .map(|(peer, pc)| (*peer, &pc.ticket, &pc.retry_tickets)),
            );
        for (peer, newest, retry) in held {
            if constant_time_eq(newest, ticket) {
                found = Some((peer, false));
            }
            if retry.matches(ticket, now) {
                found = Some((peer, true));
            }
        }
        found
    }

    /// The per-address cap at join (`ServerConfig::max_peers_per_ip`, ADR-3283 as amended by
    /// ADR-8733). `resume` is the peer a ticket brings back, `held` the block of the hold a ticket
    /// of a player that left on purpose names. A player coming back to its own block uses the slot
    /// it held (or holds, in a live takeover) and is never refused. Any other join counts the
    /// block's live peers, the resumed peer excluded; a fresh one also counts the block's held
    /// slots (`address_hold::fresh_join_refused`). A resume from another block moves a peer, not
    /// a new one, so only the live count binds it, as before.
    fn per_address_refuses(&self, ip: IpAddr, resume: Option<PeerId>, held: Option<IpAddr>) -> bool {
        let cap = self.config.max_peers_per_ip;
        if cap == 0 || self.is_relay_ip(ip) {
            return false;
        }
        if held.is_some_and(|h| same_block(h, ip)) {
            return false;
        }
        if let Some(target) = resume {
            let own = match self.session.peer(target) {
                Some(p) => Some(p.addr.ip()),
                None => self.dormant.get(&target).and_then(|d| d.ip),
            };
            if own.is_some_and(|o| same_block(o, ip)) {
                return false;
            }
        }
        let live = self
            .session
            .peers()
            .filter(|p| Some(p.id) != resume && same_block(p.addr.ip(), ip))
            .count();
        if resume.is_some() {
            return live >= cap;
        }
        let held = self
            .dormant
            .values()
            .filter(|d| d.ip.is_some_and(|o| same_block(o, ip)))
            .count()
            + self.address_holds.held_in_block(ip);
        crate::address_hold::fresh_join_refused(live, held, cap)
    }

    fn mint_ticket(&mut self, peer: PeerId) -> [u8; RESUME_TICKET_BYTES] {
        if self.config.resume_grace_ms == 0 {
            return [0; RESUME_TICKET_BYTES];
        }
        self.ticket_counter += 1;
        self.minter.derive_ticket(u64::from(peer.0), self.ticket_counter)
    }

    /// The resumption ticket of a sealed accept and its epoch (wire 6, ADR-8858 B2). No ticket
    /// (and epoch 0) on a plain link, which has no PSK, when the server keeps no dormant peers,
    /// or when the peer's epoch counter is used up.
    fn mint_resumption(
        &mut self,
        peer: PeerId,
        from: SocketAddr,
        resume_id: [u8; RESUME_TICKET_BYTES],
        psk: Option<[u8; 32]>,
        epochs: &mut ResumeEpochs,
        build: &str,
    ) -> (Option<[u8; RESUMPTION_TICKET_BYTES]>, u32) {
        let Some(psk) = psk.filter(|_| self.config.resume_grace_ms > 0) else {
            return (None, 0);
        };
        let Some(epoch) = epochs.mint() else {
            return (None, 0);
        };
        let claims = TicketClaims {
            peer: peer.0,
            epoch,
            psk,
            block: crate::addr_block::AddrBlock::of(from.ip()).key_bytes(),
            issued_ms: self.now_ms,
            expires_ms: self.now_ms.saturating_add(self.config.resume_grace_ms),
            protocol_id: self.protocol_id,
            build_hash: crypto::build_hash(build),
            password_verified: !self.gate.is_open(),
            resume_id,
        };
        (self.ticket_keys.seal(&claims, self.now_ms).ok(), epoch)
    }

    /// The keys that seal this server's resumption tickets (wire 6, ADR-8858): opens a ticket a
    /// client presents, for the resume admission and for tests.
    pub fn ticket_keys(&self) -> &TicketKeys {
        &self.ticket_keys
    }

    /// The resume epochs of a live or dormant peer (ADR-8858 A2).
    pub fn resume_epochs(&self, peer: PeerId) -> Option<ResumeEpochs> {
        self.conns
            .get(&peer)
            .map(|pc| pc.epochs)
            .or_else(|| self.dormant.get(&peer).map(|d| d.epochs))
    }

    /// Why the last `ResumeInit` that was not resumed was refused, `None` before the first: to
    /// the full handshake, to a `ResumeRetry`, or with no answer. Also set by a resume refused at
    /// its key confirmation.
    pub fn last_resume_refusal(&self) -> Option<ResumeRefusal> {
        self.last_resume_refusal
    }

    /// How many resumes were refused for `why` (the counts behind `NetStatus::resume_refusals`).
    pub fn resume_refusals(&self, why: ResumeRefusal) -> u64 {
        self.resume_refusals[why.index()]
    }

    /// Answered resume flights waiting for their key confirmation, over every ticket (ADR-8858
    /// A1): none of them is a peer yet.
    pub fn pending_resumes(&self) -> usize {
        self.resumes.pending_total()
    }

    /// Writes a peer's resume epochs back after a flight minted the next one.
    fn set_resume_epochs(&mut self, peer: PeerId, epochs: ResumeEpochs) {
        if let Some(pc) = self.conns.get_mut(&peer) {
            pc.epochs = epochs;
        } else if let Some(d) = self.dormant.get_mut(&peer) {
            d.epochs = epochs;
        }
    }

    fn note_resume_refusal(&mut self, why: ResumeRefusal) {
        self.last_resume_refusal = Some(why);
        self.resume_refusals[why.index()] += 1;
    }

    fn emit(&mut self, bytes: Vec<u8>, dest: SocketAddr) {
        for (bytes, dest) in self.conditioner.process(bytes, dest, self.now_ms) {
            self.write_socket(&bytes, dest);
        }
    }

    fn write_socket(&mut self, bytes: &[u8], dest: SocketAddr) {
        match self.socket.send_to(bytes, dest) {
            Ok(()) => self.totals.datagrams_out += 1,
            Err(e) => {
                self.totals.socket_errors += 1;
                self.totals.socket_drops += 1;
                tracing::debug!(error = %e, %dest, "regia_net server send error");
            }
        }
    }

    fn deny(&mut self, addr: SocketAddr, client_nonce: u64, reason: DenyReason, detail: &str) {
        self.totals.denied += 1;
        tracing::info!(target: "regia_net", %addr, reason = reason.as_str(), detail, "join denied");
        let deny = Deny {
            client_nonce,
            reason,
            detail: detail.to_owned(),
        };
        self.emit(deny.to_datagram(), addr);
    }

    /// Refuses a request of another protocol id (ADR-7985). The detail names both sides' versions
    /// and which one is older, from the client's side, so a build that predates stamps still shows
    /// the player what to do; a client that sent a stamp also gets the server's stamp after it.
    /// A plaintext `Deny` answering a valid cookie (ADR-8764). An answer of this second that the
    /// cookie just proved gives its slot of the pre-proof cap to this `Deny`; otherwise (a
    /// replayed cookie, or an answer of the last second) the `Deny` is a fresh pre-proof answer
    /// and passes the cap like one, or is shed without a packet.
    fn deny_after_proof(
        &mut self,
        proved: Option<crate::answer_share::Proved>,
        addr: SocketAddr,
        client_nonce: u64,
        reason: DenyReason,
        detail: &str,
    ) {
        if proved != Some(crate::answer_share::Proved::ThisSecond)
            && !self.answer_governor.try_admit(addr.ip(), self.now_ms)
        {
            self.totals.pre_proof_shed += 1;
            return;
        }
        self.deny(addr, client_nonce, reason, detail);
    }

    fn deny_version_mismatch(&mut self, addr: SocketAddr, client_nonce: u64, client: Option<VersionStamp>) {
        let stamped = client.is_some();
        let report = VersionMismatchReport::new(client, Some(self.probe.stamp().clone()));
        self.totals.denied += 1;
        tracing::info!(
            target: "regia_net",
            %addr,
            reason = DenyReason::ProtocolMismatch.as_str(),
            side = report.side.as_str(),
            detail = report.message.as_str(),
            "join denied"
        );
        let deny = Deny {
            client_nonce,
            reason: DenyReason::ProtocolMismatch,
            detail: report.message.clone(),
        };
        let bytes = if stamped {
            deny.to_datagram_with_stamp(self.probe.stamp())
        } else {
            deny.to_datagram()
        };
        self.version_mismatch = Some(report);
        self.emit(bytes, addr);
    }

    fn handle_datagram(&mut self, datagram: &[u8], from: SocketAddr) {
        // The plaintext version probe (spec 120 § 12.2): answered before any other parsing, never
        // larger than the request, limited per source prefix and by the global pre-proof cap.
        if is_probe_request(datagram) {
            if let Some(answer) = self.probe.answer(datagram, from, self.now_ms) {
                if self.answer_governor.try_admit(from.ip(), self.now_ms) {
                    self.emit(answer, from);
                } else {
                    self.totals.pre_proof_shed += 1;
                }
            }
            return;
        }
        let Some((kind, body)) = split_datagram(datagram) else {
            self.totals.foreign += 1;
            return;
        };
        // No per-source budget is charged here (finding 23, ADR-3283): a `ConnectRequest`'s source
        // is unproved, so charging it would let anyone forging a victim's address spend the
        // victim's budget. Answering costs one keyed hash and a reply no larger than the padded
        // request, and the answers are capped server-globally (ADR-4936,
        // `pre_proof_answers_per_s`), not per source; the per-address budget is charged on the
        // packets that bring the cookie back.
        match kind {
            PacketKind::ConnectRequest
            | PacketKind::ChallengeResponse
            | PacketKind::KeyInit
            | PacketKind::ResumeInit
                if !self.take_unattributed() => {}
            PacketKind::ResumeInit => self.on_resume_init(body, from),
            PacketKind::ConnectRequest => self.on_connect_request(body, from),
            PacketKind::ChallengeResponse => self.on_challenge_response(body, from),
            PacketKind::KeyInit => self.on_key_init(body, from),
            PacketKind::Sealed => self.on_sealed(body, from),
            PacketKind::Payload => self.on_payload(body, from),
            PacketKind::Disconnect => self.on_disconnect(body, from),
            PacketKind::Challenge
            | PacketKind::Accept
            | PacketKind::Deny
            | PacketKind::KeyReply
            | PacketKind::ResumeReply
            | PacketKind::ResumeRetry => {
                self.totals.malformed += 1;
            }
        }
    }

    /// `ResumeInit` (wire 6): the first flight of a one-round-trip resume (spec 120 § 10.3;
    /// ADR-8733 (3) as amended by ADR-8858; ADR-8902). Cheapest check first, and nothing here
    /// admits anyone: a flight that passes every check leaves a pending resume and is answered
    /// `ResumeReply`; the peer is admitted by [`Self::on_resume_confirm`], when the first sealed
    /// record under the new keys shows the sender holds the flight's ephemeral key.
    ///
    /// In order: the ticket opens under this server's keys (one AEAD, no state); the source is
    /// inside the ticket's address block, or brought the cookie of a `ResumeRetry`; a
    /// byte-identical copy of a pending flight gets its stored reply and costs nothing else (A4);
    /// message 1 opens under the ticket's PSK (one AEAD; a failure gets no answer); the hello
    /// and the ticket agree with this server; the ticket still names a peer, by its epoch (A2),
    /// or a held slot; the flight's number is above every one seen with this ticket (A3); the
    /// handshake budgets admit it, once per distinct flight; and only then the curve work (`ee`).
    fn on_resume_init(&mut self, body: &[u8], from: SocketAddr) {
        let Ok(init) = ResumeInit::decode(body) else {
            self.totals.malformed += 1;
            return;
        };
        self.totals.resume_inits += 1;
        let stamp = ResumeInit::stamp_of(body);
        if init.protocol_id != self.protocol_id {
            // Another build: the request's `protocol_mismatch` deny, with the version stamp.
            self.refuse_resume(&init, stamp, from, ResumeRefusal::Protocol);
            return;
        }
        if self.closed
            || self.config.resume_grace_ms == 0
            || self.identity.is_none()
            || init.crypto_version != CRYPTO_VERSION
        {
            self.refuse_resume(&init, stamp, from, ResumeRefusal::Unavailable);
            return;
        }
        let claims = match self.ticket_keys.open(&init.ticket, self.now_ms) {
            Ok(claims) => claims,
            Err(why) => {
                self.refuse_resume(&init, stamp, from, why);
                return;
            }
        };
        // The address. A source inside the ticket's block needs no cookie: the ticket was minted
        // to an address this server had proved (the address token of RFC 9000 § 8.1.3). Any other
        // source proves itself with the cookie of a `ResumeRetry` first.
        if !claims.in_block(from.ip()) {
            let cookie = init.cookie.filter(|cookie| {
                self.minter
                    .verify(from, init.client_nonce, cookie, self.now_ms)
                    .is_some()
            });
            if cookie.is_none() {
                self.retry_resume(&init, from);
                return;
            }
            // The cookie proves the exact answer that carried it, once (ADR-8764).
            if let Some(proved) = self
                .answer_governor
                .take_proved(from, init.client_nonce, self.now_ms)
            {
                self.answer_governor.refund_proved(from.ip(), proved, self.now_ms);
            }
        }
        if self.session.is_banned(from.ip(), self.now_unix_s) {
            // The full handshake says `banned` once its cookie proved the address.
            self.refuse_resume(&init, stamp, from, ResumeRefusal::Unavailable);
            return;
        }
        // A4: a lost reply. The stored bytes cost no key work and no handshake charge; they are
        // drawn from the pre-proof answer budget, like a `Challenge`.
        let id = ticket_id(&init.ticket);
        if let Some(reply) = self.resumes.replay(&id, &init.noise) {
            let reply = reply.to_vec();
            if self.answer_governor.try_admit(from.ip(), self.now_ms) {
                self.totals.resume_replays += 1;
                self.emit(reply, from);
            } else {
                self.totals.pre_proof_shed += 1;
            }
            return;
        }
        let responder = match ResumeResponder::open(
            &claims.psk,
            self.protocol_id,
            init.client_nonce,
            init.crypto_version,
            &init.ticket,
            &init.noise,
        ) {
            Ok(responder) => responder,
            Err(ResumeRefusal::Psk) => {
                // Not the PSK's holder: no answer (A6).
                self.totals.auth_failed += 1;
                self.drop_resume(from, ResumeRefusal::Psk);
                return;
            }
            Err(why) => {
                self.refuse_resume(&init, stamp, from, why);
                return;
            }
        };
        let hello = responder.hello().clone();
        if let Err(why) = self.check_resume_hello(&claims, &hello) {
            self.refuse_resume(&init, stamp, from, why);
            return;
        }
        let target = match self.resume_target(&claims, from) {
            Ok(target) => target,
            Err(why) => {
                self.refuse_resume(&init, stamp, from, why);
                return;
            }
        };
        // A3: flights are ordered by the client's own count, never by arrival.
        match self.resumes.classify(&id, hello.flight, self.now_ms) {
            Flight::New => {}
            Flight::Stale => {
                self.drop_resume(from, ResumeRefusal::StaleFlight);
                return;
            }
            Flight::Busy => {
                self.refuse_resume(&init, stamp, from, ResumeRefusal::Busy);
                return;
            }
        }
        // A distinct flight of the PSK's holder: the handshake budgets are charged now, once,
        // before the curve work (ADR-3283 (1), ADR-8858 A4).
        let relay = self.is_relay_ip(from.ip());
        if (!relay
            && !(self.key_exchanges.admit(from.ip(), self.now_ms)
                && self.joins.admit(from.ip(), self.now_ms)))
            || !self.key_governor.try_admit(self.now_ms)
        {
            self.totals.handshakes_refused += 1;
            self.refuse_resume(&init, stamp, from, ResumeRefusal::Busy);
            return;
        }
        let target = match target {
            // The id the returning player joins under is fixed now, because the accept names it;
            // no slot is taken and no peer exists until the confirmation.
            ResumeTarget::HeldSlot(_) => match self.session.issue_id() {
                Some(id) => ResumeTarget::HeldSlot(id),
                None => {
                    self.refuse_resume(&init, stamp, from, ResumeRefusal::Busy);
                    return;
                }
            },
            peer => peer,
        };
        let mut session_token = 0;
        for _ in 0..8 {
            self.token_counter += 1;
            session_token = self
                .minter
                .derive_u32(u64::from(u32::MAX), self.token_counter)
                .max(1);
            if !self.tokens.contains_key(&session_token) && !self.resumes.has_token(session_token) {
                break;
            }
        }
        let (mut link, noise) = match responder.reply(session_token) {
            Ok(done) => done,
            Err(e) => {
                self.totals.auth_failed += 1;
                tracing::debug!(target: "regia_net", %from, error = %e, "resume reply failed");
                return;
            }
        };
        link.set_rekey_policy(self.config.rekey);
        self.totals.handshakes += 1;
        // The accept: a new 16-byte ticket and a new resumption ticket of the peer's next epoch
        // (A2), sealed under the new link (A7). Minting retires nothing: the ticket the flight
        // presented stays good until the confirmation.
        let peer = target.peer();
        let mut epochs = match target {
            ResumeTarget::Peer(peer) => self.resume_epochs(peer).unwrap_or_default(),
            ResumeTarget::HeldSlot(_) => ResumeEpochs::default(),
        };
        let ticket = self.mint_ticket(peer);
        let psk = Some(*link.resume_psk());
        let (resumption, accept_epoch) =
            self.mint_resumption(peer, from, ticket, psk, &mut epochs, &hello.build);
        if let ResumeTarget::Peer(peer) = target {
            self.set_resume_epochs(peer, epochs);
        }
        let accept = Accept {
            client_nonce: init.client_nonce,
            session_token,
            peer_id: u16::try_from(peer.0).unwrap_or(u16::MAX),
            tick_rate_hz: self.config.tick_rate_hz.min(u32::from(u16::MAX)) as u16,
            server_tick: self.driver.tick(),
            resume_ticket: ticket,
            resumption,
        };
        let reply = link
            .seal(SealedKind::Accept, &accept.to_body())
            .ok()
            .and_then(|accept| {
                ResumeReply {
                    client_nonce: init.client_nonce,
                    noise,
                    accept,
                }
                .to_datagram()
                .ok()
            });
        let Some(reply) = reply else {
            self.totals.auth_failed += 1;
            return;
        };
        self.totals.sealed_out += 1;
        let pending = PendingResume {
            flight: hello.flight,
            noise: init.noise,
            reply: reply.clone(),
            link,
            client_nonce: init.client_nonce,
            from,
            claims,
            join: hello.into_join_token(claims.resume_id),
            target,
            ticket,
            resumption,
            accept_epoch,
            epochs,
            created_ms: self.now_ms,
        };
        if self.resumes.insert(id, claims.expires_ms, pending).is_some() {
            // The client made a newer flight: the older one of the two it had pending is given up.
            self.totals.resume_lapsed += 1;
        }
        tracing::debug!(target: "regia_net", %from, %peer, "resume answered: waiting for key confirmation");
        self.emit(reply, from);
    }

    /// What the hello and the ticket must agree on with this server before a resume is answered.
    fn check_resume_hello(
        &self,
        claims: &TicketClaims,
        hello: &crypto::ResumeHello,
    ) -> Result<(), ResumeRefusal> {
        claims.check_protocol(self.protocol_id, &hello.build)?;
        if !self.config.build.is_empty() && self.config.build != hello.build {
            // The full handshake names both builds in its deny.
            return Err(ResumeRefusal::Protocol);
        }
        if !self.gate.is_open() && !claims.password_verified {
            // The ticket does not vouch for the password this server asks for.
            return Err(ResumeRefusal::Unavailable);
        }
        // The full handshake's `invalid_join` deny carries the reason.
        let join = G::JoinData::from_bytes(&hello.join_data).map_err(|_| ResumeRefusal::Malformed)?;
        G::validate_join(&join).map_err(|_| ResumeRefusal::Malformed)
    }

    /// Who a ticket still names (ADR-8858 A2): its dormant or live peer, while the ticket's epoch
    /// is inside the peer's window, or the held slot of a player that left on purpose, from that
    /// slot's own block.
    fn resume_target(
        &mut self,
        claims: &TicketClaims,
        from: SocketAddr,
    ) -> Result<ResumeTarget, ResumeRefusal> {
        match self.ticket_owner(&claims.resume_id) {
            Some((peer, _)) if peer.0 == claims.peer => {
                self.resume_epochs(peer)
                    .ok_or(ResumeRefusal::NoPeer)?
                    .check(claims.epoch)?;
                Ok(ResumeTarget::Peer(peer))
            }
            Some(_) => Err(ResumeRefusal::NoPeer),
            None => {
                self.address_holds.expire(self.now_ms);
                let held = self
                    .address_holds
                    .find(&claims.resume_id)
                    .is_some_and(|held| same_block(held, from.ip()));
                if held {
                    // The id is issued when the flight is answered.
                    return Ok(ResumeTarget::HeldSlot(PeerId(0)));
                }
                // A peer that confirmed a newer accept made this ticket single use.
                let spent = self
                    .resume_epochs(PeerId(claims.peer))
                    .is_some_and(|epochs| epochs.check(claims.epoch).is_err());
                Err(if spent {
                    ResumeRefusal::Epoch
                } else {
                    ResumeRefusal::NoPeer
                })
            }
        }
    }

    /// Refuses a resume to the full handshake (ADR-8858 A6): the flight is answered as the
    /// `ConnectRequest` it stands in for, with a `Challenge` for the same nonce (or that
    /// request's deny: closing, another protocol id). Never larger than the padded flight, and
    /// drawn from the pre-proof answer budget like any challenge.
    fn refuse_resume(
        &mut self,
        init: &ResumeInit,
        stamp: Option<VersionStamp>,
        from: SocketAddr,
        why: ResumeRefusal,
    ) {
        self.totals.resume_fallbacks += 1;
        self.note_resume_refusal(why);
        tracing::debug!(target: "regia_net", %from, reason = why.as_str(), "resume refused: full handshake");
        self.answer_connect(init.as_connect_request(), stamp, from);
    }

    /// A flight that gets no answer (ADR-8858 A3, A6): message 1 did not open under the PSK, or
    /// its number is an older flight's.
    fn drop_resume(&mut self, from: SocketAddr, why: ResumeRefusal) {
        self.note_resume_refusal(why);
        tracing::debug!(target: "regia_net", %from, reason = why.as_str(), "resume dropped: no answer");
    }

    /// Asks a flight from outside its ticket's address block to come again with a cookie
    /// (ADR-8858 A6): `ResumeRetry`, 29 bytes, from the pre-proof answer budget.
    fn retry_resume(&mut self, init: &ResumeInit, from: SocketAddr) {
        self.note_resume_refusal(ResumeRefusal::OtherBlock);
        if !self
            .answer_governor
            .try_admit_answer(from, init.client_nonce, self.now_ms)
        {
            self.totals.pre_proof_shed += 1;
            return;
        }
        let retry = ResumeRetry {
            client_nonce: init.client_nonce,
            cookie: self.minter.cookie(from, init.client_nonce, self.now_ms),
        };
        self.emit(retry.to_datagram(), from);
    }

    /// The first sealed record under a pending resume's keys (ADR-8858 A1): only the holder of
    /// that flight's ephemeral key can seal it, so this is where the resume is admitted, by the
    /// rules every join passes ([`Self::admit`]): dormant or live takeover under the same peer
    /// id, or the held slot of a player that left on purpose. Before this, the flight changed
    /// nothing. A record that fails its tag changes nothing either. The record must come from
    /// the address the flight proved or from the ticket's own block.
    fn on_resume_confirm(&mut self, header: SealedHeader<'_>, from: SocketAddr) {
        let token = header.token;
        let Some(pending) = self.resumes.get_mut(token) else {
            return;
        };
        if from.ip().to_canonical() != pending.from.ip().to_canonical() && !pending.claims.in_block(from.ip())
        {
            self.totals.rebinds_refused += 1;
            return;
        }
        let Some((kind, plain)) = self.totals.note_open(pending.link.open(header)) else {
            return;
        };
        let Some((id, pending)) = self.resumes.take(token) else {
            return;
        };
        match kind {
            SealedKind::Payload => {}
            // The client connected and left at once: nothing to admit.
            SealedKind::Disconnect => {
                self.totals.resume_lapsed += 1;
                return;
            }
            SealedKind::Join | SealedKind::Accept | SealedKind::Deny => {
                self.totals.malformed += 1;
                self.totals.resume_lapsed += 1;
                return;
            }
        }
        let (peer, nonce, join) = (pending.target.peer(), pending.client_nonce, pending.join.clone());
        self.admit(from, nonce, join, [0; 16], JoinPath::Resume(Box::new(pending)));
        if !self.conns.get(&peer).is_some_and(|pc| pc.token == token) {
            return; // Refused: `admit` said why on the pending link.
        }
        // The ticket's row leaves with its other pending flight (A5): the peer's epoch, or the
        // spent hold, has made the ticket dead.
        self.totals.resume_lapsed += self.resumes.confirmed(&id) as u64;
        self.totals.resumed_one_rtt += 1;
        tracing::info!(target: "regia_net", %peer, %from, "peer resumed in one round trip");
        if let Some(pc) = self.conns.get_mut(&peer) {
            pc.path.heard_primary(self.now_ms);
        }
        self.on_peer_record(peer, SealedKind::Payload, &plain, from);
    }

    /// The password challenge of an attempt, with the time its cookie was minted.
    fn password_challenge(
        &self,
        from: SocketAddr,
        client_nonce: u64,
        minted_ms: u64,
    ) -> Option<PasswordChallenge> {
        (!self.gate.is_open()).then(|| PasswordChallenge {
            salt: self.gate.salt(),
            nonce: self.minter.password_nonce(from, client_nonce, minted_ms),
        })
    }

    fn on_connect_request(&mut self, body: &[u8], from: SocketAddr) {
        let Ok(req) = ConnectRequest::decode(body) else {
            self.totals.malformed += 1;
            return;
        };
        self.answer_connect(req, ConnectRequest::stamp_of(body), from);
    }

    /// Answers a connect request, or the `ResumeInit` that stood in for one: a `Challenge`, or a
    /// deny.
    fn answer_connect(&mut self, req: ConnectRequest, stamp: Option<VersionStamp>, from: SocketAddr) {
        // The global pre-proof shed (ADR-4936): every answer below costs a keyed hash, a ban-list
        // scan and a send, and the source is unproved, so the cap is server-wide rather than
        // per-source (a per-source cap would be spendable by a forger, finding 23). Over the cap
        // the request is dropped without an answer.
        if !self
            .answer_governor
            .try_admit_answer(from, req.client_nonce, self.now_ms)
        {
            self.totals.pre_proof_shed += 1;
            return;
        }
        if self.closed {
            self.deny(
                from,
                req.client_nonce,
                DenyReason::ShuttingDown,
                "server is closing",
            );
            return;
        }
        if req.protocol_id != self.protocol_id {
            self.deny_version_mismatch(from, req.client_nonce, stamp);
            return;
        }
        // The run-time content check (ADR-7985): same protocol, other content in the stamp.
        if self.runtime_content.is_some()
            && let Some(client) = stamp
            && client.content_hash != self.probe.stamp().content_hash
        {
            self.deny_version_mismatch(from, req.client_nonce, Some(client));
            return;
        }
        // No ban check here (ADR-4936): it scans the whole ban list per padded request on a hot
        // path. A banned source is refused after its cookie proves the address, in `on_key_init`
        // and again at join admission.
        let crypto_version = match crypto::negotiate(
            self.config.crypto,
            self.identity.is_some(),
            from,
            req.crypto_version,
            req.crypto_required,
        ) {
            Negotiated::Sealed => CRYPTO_VERSION,
            Negotiated::Plain => PLAIN_VERSION,
            Negotiated::Refuse(reason) => {
                let detail = format!(
                    "server encryption is `{}`, suite {CRYPTO_VERSION}; client offered suite {}",
                    self.config.crypto.as_str(),
                    req.crypto_version
                );
                self.deny(from, req.client_nonce, reason, &detail);
                return;
            }
        };
        let cookie = self.minter.cookie(from, req.client_nonce, self.now_ms);
        let password = self.password_challenge(from, req.client_nonce, self.now_ms);
        let challenge = Challenge {
            client_nonce: req.client_nonce,
            cookie,
            password,
            crypto_version,
        };
        self.emit(challenge.to_datagram(), from);
    }

    /// Refresh an unusable pre-key proof within the existing pre-proof answer budget.
    /// Never amplify the request, allocate a pending row, or charge proved-source budgets.
    fn refresh_challenge(&mut self, from: SocketAddr, nonce: u64, offered: u8, request_len: usize) {
        if self.closed {
            return;
        }
        let version =
            match crypto::negotiate(self.config.crypto, self.identity.is_some(), from, offered, false) {
                Negotiated::Sealed => CRYPTO_VERSION,
                Negotiated::Plain => PLAIN_VERSION,
                Negotiated::Refuse(_) => return,
            };
        if !self.answer_governor.try_admit_answer(from, nonce, self.now_ms) {
            self.totals.pre_proof_shed += 1;
            return;
        }
        let challenge = Challenge {
            client_nonce: nonce,
            cookie: self.minter.cookie(from, nonce, self.now_ms),
            password: self.password_challenge(from, nonce, self.now_ms),
            crypto_version: version,
        }
        .to_datagram();
        if challenge.len() > request_len {
            return;
        }
        self.emit(challenge, from);
    }

    /// `KeyInit`: the first message that costs curve work, so everything cheap comes first.
    fn on_key_init(&mut self, body: &[u8], from: SocketAddr) {
        let Ok(init) = KeyInit::decode(body) else {
            self.totals.malformed += 1;
            return;
        };
        let nonce = init.client_nonce;
        let Some(minted_ms) = self.minter.verify(from, nonce, &init.cookie, self.now_ms) else {
            // A stale cookie or a raced forged challenge cannot make progress. Return a
            // bounded fresh proof, without allocating pending state or doing curve work.
            self.totals.malformed += 1;
            self.refresh_challenge(from, nonce, init.version, body.len() + 5);
            return;
        };
        // The exact answer this cookie proves, at most once (ADR-8764): a replayed cookie proves
        // nothing, so it can neither refund nor fund a free `Deny`.
        let proved = self.answer_governor.take_proved(from, nonce, self.now_ms);
        if self.closed {
            self.deny_after_proof(proved, from, nonce, DenyReason::ShuttingDown, "server is closing");
            return;
        }
        if self.session.is_banned(from.ip(), self.now_unix_s) {
            self.deny_after_proof(proved, from, nonce, DenyReason::Banned, "banned");
            return;
        }
        let Some(identity) = self.identity.clone() else {
            self.deny_after_proof(
                proved,
                from,
                nonce,
                DenyReason::CryptoVersionMismatch,
                "this server does not encrypt",
            );
            return;
        };
        if init.version != CRYPTO_VERSION {
            let detail = format!("server runs suite {CRYPTO_VERSION}, client sent {}", init.version);
            self.deny_after_proof(proved, from, nonce, DenyReason::CryptoVersionMismatch, &detail);
            return;
        }
        // The address is proved: the answer that carried this cookie was no reflection, so its
        // prefix gets the token back (ADR-8718, ADR-8764).
        if let Some(proved) = proved {
            self.answer_governor.refund_proved(from.ip(), proved, self.now_ms);
        }
        if let Some(entry) = self.pending.get(from)
            && entry.client_nonce == nonce
        {
            if entry.first_message == init.noise {
                // Our reply was lost: the stored bytes cost no curve work.
                let reply = entry.reply.clone();
                self.emit(reply, from);
            } else {
                // Do no more curve work on this cookie. A replaced challenge may have
                // stranded the client on a different share; refresh so it can restart.
                self.totals.handshakes_refused += 1;
                self.refresh_challenge(from, nonce, init.version, body.len() + 5);
            }
            return;
        }
        if let Some(peer) = self.session.peer_by_addr(from)
            && self.conns.get(&peer).is_some_and(|pc| pc.client_nonce == nonce)
        {
            return; // A late copy from an attempt that already joined.
        }
        if !self.is_relay_ip(from.ip()) && !self.key_exchanges.admit(from.ip(), self.now_ms) {
            self.totals.handshakes_refused += 1;
            return;
        }
        if self.pending.has_room(from, self.now_ms).is_err() || !self.key_governor.try_admit(self.now_ms) {
            self.totals.handshakes_refused += 1;
            return;
        }
        self.token_counter += 1;
        let session_token = self
            .minter
            .derive_u32(u64::from(u32::MAX), self.token_counter)
            .max(1);
        let password = self.password_challenge(from, nonce, minted_ms);
        let prologue = crypto::prologue(
            self.protocol_id,
            nonce,
            init.version,
            &init.cookie,
            password.as_ref(),
        );
        match accept_handshake(&identity, &prologue, &init.noise, session_token) {
            Ok((mut link, noise)) => {
                link.set_rekey_policy(self.config.rekey);
                let reply = KeyReply {
                    client_nonce: nonce,
                    noise,
                }
                .to_datagram();
                self.totals.handshakes += 1;
                self.pending.insert(
                    from,
                    PendingHandshake {
                        client_nonce: nonce,
                        first_message: init.noise,
                        reply: reply.clone(),
                        link,
                        minted_ms,
                        created_ms: self.now_ms,
                    },
                );
                self.emit(reply, from);
            }
            Err(e) => {
                self.totals.auth_failed += 1;
                tracing::debug!(target: "regia_net", %from, error = %e, "key exchange refused");
            }
        }
    }

    /// A sealed record: from a connected peer, the join of a pending handshake (also from another
    /// port of the handshake's IP, when the NAT moved the client after the `KeyReply`), or a
    /// connected peer whose address changed (NAT rebinding, ADR-2416). Nothing here acts on a byte
    /// before the tag verified.
    fn on_sealed(&mut self, body: &[u8], from: SocketAddr) {
        let Some(header) = SealedHeader::parse(body) else {
            self.totals.malformed += 1;
            return;
        };
        let peer = self
            .session
            .peer_by_addr_counted(from)
            .filter(|p| self.conns.get(p).is_some_and(|pc| pc.token == header.token));
        if let Some(peer) = peer {
            // The rate check runs before the open (spec 120 § 11.3): a flood costs no cipher work.
            if self.over_rate(peer, body.len()) {
                return;
            }
            let (kind, plain) = match self.open_peer_record(peer, header) {
                Opened::Record(kind, plain) => (kind, plain),
                // The peer's own record, scored by `open_peer_record`: it keeps what the rate
                // check took.
                Opened::UnknownKind => return,
                Opened::Failed => {
                    // Not this peer's (a forged or replayed record from its address): give back
                    // what the rate check took, so it never counts toward the peer's rate, slice
                    // or flood timer (ADR-8606).
                    self.refund_unopened(peer, body.len());
                    return;
                }
            };
            if let Some(pc) = self.conns.get_mut(&peer) {
                pc.path.heard_primary(self.now_ms);
            }
            self.on_peer_record(peer, kind, &plain, from);
            return;
        }
        if !self.take_unattributed() {
            return;
        }
        if self.resumes.has_token(header.token) {
            self.on_resume_confirm(header, from);
            return;
        }
        let Some(at) = self.pending_join_row(header.token, from) else {
            self.on_rebind(header, from);
            return;
        };
        let Some(entry) = self.pending.get_mut(at) else {
            return;
        };
        let (nonce, minted_ms, hash) = (entry.client_nonce, entry.minted_ms, *entry.link.handshake_hash());
        // A record that fails here changes nothing: the window and the keys move only after the
        // tag verified, and the row stays where it is.
        let Some((kind, plain)) = self.totals.note_open(entry.link.open(header)) else {
            return;
        };
        if kind != SealedKind::Join {
            self.totals.malformed += 1;
            return;
        }
        if at != from {
            // The join authenticated under this handshake's keys: the client's NAT gave it a new
            // port after the `KeyReply`. The row follows, so the accept or deny goes to `from`.
            if !self.pending.move_entry(at, from) {
                return;
            }
            tracing::info!(target: "regia_net", old = %at, new = %from, "handshake moved to a new port");
        }
        let mut r = WireReader::new(&plain);
        let Ok(token) = JoinToken::decode(&mut r).and_then(|t| r.finish().map(|()| t)) else {
            self.totals.malformed += 1;
            return;
        };
        let pw_nonce =
            crypto::bind_password_nonce(&self.minter.password_nonce(from, nonce, minted_ms), &hash);
        self.admit(from, nonce, token, pw_nonce, JoinPath::Sealed);
    }

    /// The pending handshake a sealed record with session `token` from `from` may be the join of:
    /// the row at `from` itself, or, when `from` holds no row, the row of another port of the same
    /// IP (found by token through the table's index, no scan), whose client the NAT moved after the
    /// `KeyReply` (review row 20). Nothing is moved here: the caller opens the record under that
    /// row's keys first.
    fn pending_join_row(&self, token: u32, from: SocketAddr) -> Option<SocketAddr> {
        match self.pending.get(from) {
            Some(entry) => (entry.link.token() == token).then_some(from),
            None => self.pending.moved_from(from, token),
        }
    }

    /// One authenticated record of a connected peer. Every one proves the peer alive, a `Join`
    /// resend for a lost accept included (spec 120 § 10.3, adversarial row 9). `source` says
    /// whether its refusals may score the peer (ADR-8606).
    fn on_peer_record(&mut self, peer: PeerId, kind: SealedKind, plain: &[u8], from: SocketAddr) {
        self.session.touch(peer, self.now_ms);
        // A record that never reaches `Connection::receive` is still the peer's traffic: it runs
        // the stall clock of a waiting reassembly, or a drip of them would hold a gap past the
        // ceiling (spec 120 § 5.2, ADR-8883). Only an authenticated one counts (ADR-8717).
        // Every record here is the peer's own: plain links only bring payloads this way.
        if matches!(kind, SealedKind::Join | SealedKind::Accept | SealedKind::Deny)
            && !self.note_record_traffic(peer)
        {
            return;
        }
        match kind {
            SealedKind::Payload => self.deliver(peer, plain),
            // Clients only ever leave; whatever reason byte they send is reported as `Left`.
            SealedKind::Disconnect => {
                self.drop_peer(peer, DisconnectReason::Left, false);
            }
            // Our accept was lost; answer again. An honest client resends exactly its encoded
            // `JoinToken`: any other body is the peer's own malformation and scores (ADR-8734,
            // the ADR-8606 rule: authenticated, then failed decode).
            SealedKind::Join => {
                let mut r = WireReader::new(plain);
                if JoinToken::decode(&mut r).and_then(|_| r.finish()).is_err() {
                    self.totals.malformed += 1;
                    tracing::debug!(target: "regia_net", %peer, "connected peer's Join does not decode");
                    self.charge_malformed(peer, MALFORMED_WEIGHT);
                    return;
                }
                let Some(pc) = self.conns.get_mut(&peer) else {
                    return;
                };
                if std::mem::replace(&mut pc.accept_resent, true) {
                    return;
                }
                self.send_accept(peer, from);
            }
            // Only the server sends these. An authenticated one is a broken or hostile client,
            // never a lossy link (a damaged record fails its tag): it scores (ADR-8606 residual
            // (c), closed by ADR-8666).
            SealedKind::Accept | SealedKind::Deny => {
                self.totals.malformed += 1;
                tracing::debug!(target: "regia_net", %peer, ?kind, "server-only sealed kind from a client");
                self.charge_malformed(peer, MALFORMED_WEIGHT);
            }
        }
    }

    /// Counts one authenticated non-payload record of `peer` toward a stalled reassembly
    /// (`Connection::note_authenticated_record`). False when the stall passed its deadline: the
    /// peer has failed its link and is gone, the way `deliver` drops one.
    fn note_record_traffic(&mut self, peer: PeerId) -> bool {
        let now = self.now_ms;
        let Some(pc) = self.conns.get_mut(&peer) else {
            return true;
        };
        let Err(error) = pc.conn.note_authenticated_record(now) else {
            return true;
        };
        let kind = pc
            .conn
            .failure()
            .map_or(RejectKind::ReassemblyStalled, RejectKind::from);
        self.totals.malformed += 1;
        tracing::info!(target: "regia_net", %peer, kind = kind.as_str(), %error, "peer failed its link");
        self.events.push_back(ServerEvent::Rejected { peer, kind, error });
        self.drop_peer(peer, DisconnectReason::ProtocolError, true);
        false
    }

    /// Opens a sealed record under `peer`'s session keys and counts the result (ADR-8606,
    /// ADR-8666). A record whose tag verified but whose kind byte names no [`SealedKind`] is the
    /// peer's own malformation: it is counted in `totals.malformed` and scores
    /// [`MALFORMED_WEIGHT`] here ([`Opened::UnknownKind`]). A record that failed its open is
    /// counted by its error and charged to no one.
    fn open_peer_record(&mut self, peer: PeerId, header: SealedHeader<'_>) -> Opened {
        let Some(link) = self.conns.get_mut(&peer).and_then(|pc| pc.link.as_mut()) else {
            self.totals.auth_failed += 1;
            return Opened::Failed;
        };
        let (byte, plain) = match link.open_kind_byte(header) {
            Ok(record) => record,
            Err(e) => {
                let _ = self.totals.note_open(Err(e));
                return Opened::Failed;
            }
        };
        self.totals.opened_in += 1;
        match SealedKind::from_u8(byte) {
            Some(kind) => Opened::Record(kind, plain),
            None => {
                self.unknown_sealed_kind(peer, byte);
                Opened::UnknownKind
            }
        }
    }

    /// Counts and scores a record that authenticated under `peer`'s keys but whose kind byte
    /// names no [`SealedKind`] (ADR-8666): the tag proves it is the peer's, and no lossy or
    /// reordering link makes one.
    fn unknown_sealed_kind(&mut self, peer: PeerId, byte: u8) {
        self.totals.malformed += 1;
        tracing::debug!(target: "regia_net", %peer, kind = byte, "authenticated record of an unknown kind");
        self.charge_malformed(peer, MALFORMED_WEIGHT);
    }

    /// A sealed record from an address no peer holds, carrying a sealed peer's token: the client's
    /// NAT may have given it a new address mid-session, or someone copied the record and raced it
    /// from elsewhere (soak finding F1). The address proves nothing (RFC 9000 § 9.3), so the record
    /// only makes the address a *candidate* (ADR-8299): it must come within that address's
    /// trial-open budget, be newer than any record the peer sent, come from no banned address and
    /// authenticate under the peer's key. Its payload is then delivered (it is authentic), the peer
    /// stays on its validated address, and a probe goes to the candidate alone; the peer moves when
    /// a record from the candidate acknowledges the probe ([`crate::path`]).
    fn on_rebind(&mut self, header: SealedHeader<'_>, from: SocketAddr) {
        let Some(peer) = self
            .tokens
            .get(&header.token)
            .copied()
            // The token must still be the peer's own: a stale map row never selects it.
            .filter(|p| {
                self.conns
                    .get(p)
                    .is_some_and(|pc| pc.link.is_some() && pc.token == header.token)
            })
        else {
            self.totals.auth_failed += 1;
            return;
        };
        let len = crate::crypto::SEALED_HEADER_BYTES + header.ciphertext.len();
        if !self.candidate_allowed(peer, header.token, from) {
            self.refuse_rebind(peer);
            return;
        }
        let counter = header.record_counter();
        let stale = self.stale_sealed(peer, counter);
        let recent = self
            .conns
            .get(&peer)
            .and_then(|pc| pc.link.as_ref())
            .and_then(|l| l.newest_counter())
            .is_some_and(|n| n.saturating_sub(counter) < crate::path::DUPLICATE_CANDIDATE_RECORDS);
        let Some(link) = self.conns.get_mut(&peer).and_then(|pc| pc.link.as_mut()) else {
            return;
        };
        if stale {
            // Not newer than the peer's newest: it cannot start a move by itself (§ 9.3). A record
            // never seen (reordered across a move) is authentic content: deliver it on the
            // validated path.
            match link.open_kind_byte(header) {
                Ok((byte, plain)) => {
                    self.totals.opened_in += 1;
                    match SealedKind::from_u8(byte) {
                        Some(kind) => {
                            if let Some(primary) = self.session.peer(peer).map(|p| p.addr) {
                                self.on_peer_record(peer, kind, &plain, primary);
                            }
                        }
                        // Scored as the peer's, and a refused rebind like the fresh branch's
                        // (ADR-8734): the address does not become a candidate on it.
                        None => {
                            self.unknown_sealed_kind(peer, byte);
                            self.refuse_rebind(peer);
                        }
                    }
                    return;
                }
                Err(crate::crypto::CryptoError::Replay) => {}
                Err(e) => {
                    let _ = self.totals.note_open(Err(e));
                    self.refuse_rebind(peer);
                    return;
                }
            }
            let Some(link) = self.conns.get_mut(&peer).and_then(|pc| pc.link.as_mut()) else {
                return;
            };
            // Already seen: a copy raced from elsewhere may have delivered it first while the
            // peer's NAT really moved it here. The genuine duplicate still shows the peer at this
            // address and still answers a probe sent here; its content was delivered already.
            let opened = if recent {
                link.open_duplicate(header)
            } else {
                Err(crate::crypto::CryptoError::Replay)
            };
            match opened {
                Ok((SealedKind::Payload, plain)) => self.on_candidate_duplicate(peer, &plain, from, len),
                Ok(_) => self.refuse_rebind(peer),
                Err(_) => {
                    self.totals.replayed += 1;
                    self.refuse_rebind(peer);
                }
            }
            return;
        }
        match self.open_peer_record(peer, header) {
            Opened::Record(kind, plain) => {
                self.on_candidate_record(peer, kind, &plain, from, len);
            }
            // Scored as the peer's; the address does not become a candidate on it.
            Opened::UnknownKind | Opened::Failed => self.refuse_rebind(peer),
        }
    }

    /// A plain-link packet from an address no peer holds, carrying a plain peer's token: the same
    /// candidate and probe as [`Self::on_rebind`] (ADR-8299). A plain link has no key, so the
    /// token and a packet sequence newer than any received stand in for the tag; the probe still
    /// has to be answered from the new address before the peer moves.
    fn on_plain_rebind(&mut self, body: &[u8], from: SocketAddr) -> bool {
        let Some(token) = body
            .get(..4)
            .and_then(|b| b.try_into().ok())
            .map(u32::from_le_bytes)
        else {
            return false;
        };
        let Some(peer) = self.tokens.get(&token).copied().filter(|p| {
            self.conns
                .get(p)
                .is_some_and(|pc| pc.link.is_none() && pc.token == token)
        }) else {
            return false;
        };
        let packet = &body[4..];
        let newer = self
            .conns
            .get(&peer)
            .is_some_and(|pc| pc.conn.is_newer_packet(packet));
        if !self.candidate_allowed(peer, token, from) || !newer {
            self.refuse_rebind(peer);
            return true;
        }
        self.on_plain_candidate_packet(peer, packet, from, CONNECTED_HEADER_BYTES + packet.len());
        true
    }

    /// A plain-link packet from candidate address `from` (`len` datagram bytes) that passed the
    /// cheap checks (KI-NT-018, ADR-10077). A plain link has no tag: until a packet from `from`
    /// answers the probe, nothing proves the packet is the peer's, so it is held, not applied
    /// (the sealed path applies an authentic record at once). When one acknowledges the probe,
    /// the peer moves and the held packets are applied in arrival order, then this one. A
    /// candidate that never validates takes its packets with it when it goes idle or is replaced.
    fn on_plain_candidate_packet(&mut self, peer: PeerId, packet: &[u8], from: SocketAddr, len: usize) {
        let now = self.now_ms;
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        if pc.path.validates(from, packet) {
            let held = pc.path.take_held(from);
            self.move_peer(peer, from);
            for p in &held {
                self.on_peer_record(peer, SealedKind::Payload, p, from);
            }
            self.on_peer_record(peer, SealedKind::Payload, packet, from);
            return;
        }
        pc.path.note(from, len, now);
        if !pc.path.hold(from, packet) {
            // Over the hold's bound: dropped unapplied, counted like a refused record.
            self.refuse_rebind(peer);
        }
        self.probe_path(peer, from);
    }

    /// Counts a refused record from a new address, process-wide and on the peer's row.
    fn refuse_rebind(&mut self, peer: PeerId) {
        self.totals.rebinds_refused += 1;
        if let Some(pc) = self.conns.get_mut(&peer) {
            pc.rebinds_refused += 1;
        }
    }

    /// The cheap checks of a candidate address, before any cipher work and cheapest first: the
    /// address's trial-open budget for this peer (one keyed hash, counted here), no other peer at
    /// the address (one map probe), then the ban list. The per-IP cap is not applied: a peer that
    /// moves keeps its one slot, whatever block it moves into (ADR-8299, carrier-grade NAT). An
    /// address that already authenticated a record of this peer (a live candidate) is past the
    /// trial budget: its records are opened like the validated address's, so the genuine records
    /// that carry the probe's answer are never refused for budget (ADR-8422).
    fn candidate_allowed(&mut self, peer: PeerId, token: u32, from: SocketAddr) -> bool {
        let now = self.now_ms;
        let known = self
            .conns
            .get(&peer)
            .is_some_and(|pc| pc.path.is_candidate(from, now));
        if !known && !self.path_limiter.admit(token, from, now) {
            return false;
        }
        if self.session.peer_by_addr(from).is_some_and(|p| p != peer) {
            return false;
        }
        !self.session.is_banned(from.ip(), self.now_unix_s)
    }

    /// True when a sealed record's counter is not newer than every record the peer sent (§ 9.3:
    /// only the newest record may start a move).
    fn stale_sealed(&self, peer: PeerId, counter: u64) -> bool {
        self.conns
            .get(&peer)
            .and_then(|pc| pc.link.as_ref())
            .and_then(|l| l.newest_counter())
            .is_some_and(|n| counter <= n)
    }

    /// One authentic record from candidate address `from` (`len` datagram bytes). If it
    /// acknowledges the probe sent there, the path is validated and the peer moves; otherwise its
    /// content is handled on the validated path and the candidate is probed, within the
    /// anti-amplification limit. A plain link's packet never comes here: nothing authenticated
    /// it, so it is held until its address validates (`on_plain_candidate_packet`, ADR-10077).
    fn on_candidate_record(
        &mut self,
        peer: PeerId,
        kind: SealedKind,
        plain: &[u8],
        from: SocketAddr,
        len: usize,
    ) {
        let now = self.now_ms;
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        let validated = kind == SealedKind::Payload && pc.path.validates(from, plain);
        if validated {
            // It answered the probe sent to `from`: the address is the peer's now.
            self.move_peer(peer, from);
            self.on_peer_record(peer, kind, plain, from);
            return;
        }
        pc.path.note(from, len, now);
        let Some(primary) = self.session.peer(peer).map(|p| p.addr) else {
            return;
        };
        // Authentic, so it counts as the peer's (a copy of a genuine record carries the genuine
        // content); any answer goes to the validated address.
        self.on_peer_record(peer, kind, plain, primary);
        self.probe_path(peer, from);
    }

    /// The genuine copy of a record already delivered, from candidate address `from`: validates
    /// the candidate when it acknowledges the probe sent there; otherwise it counts as refused
    /// (nothing in it is let in), and the address is noted and probed. Nothing is delivered again.
    fn on_candidate_duplicate(&mut self, peer: PeerId, plain: &[u8], from: SocketAddr, len: usize) {
        let now = self.now_ms;
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        if pc.path.validates(from, plain) {
            self.move_peer(peer, from);
            return;
        }
        pc.path.note(from, len, now);
        self.refuse_rebind(peer);
        self.probe_path(peer, from);
    }

    /// Moves `peer` to the validated address `to`. A port-only change (a NAT rebinding) keeps the
    /// round-trip estimate (RFC 9000 § 9.4; ADR-8422); a new IP address starts it again.
    fn move_peer(&mut self, peer: PeerId, to: SocketAddr) {
        let now = self.now_ms;
        let old = self.session.peer(peer).map(|p| p.addr);
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        pc.path.moved(now);
        if old.is_none_or(|o| o.ip() != to.ip()) {
            pc.conn.path_changed();
        }
        pc.rebinds += 1;
        self.session.set_peer_addr(peer, to);
        self.totals.rebinds += 1;
        tracing::info!(target: "regia_net", %peer, ?old, new = %to, "peer moved to a validated address");
    }

    /// Sends one probe to candidate `to` when its outstanding probe is due and the bytes stay
    /// within three times what it sent (RFC 9000 § 8.1).
    fn probe_path(&mut self, peer: PeerId, to: SocketAddr) {
        let now = self.now_ms;
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        let len = pc.header_bytes() + crate::connection::PACKET_HEADER_BYTES;
        if !pc.path.get(to).is_some_and(|c| c.may_probe(len, now)) {
            return;
        }
        let bytes = match pc.link.as_mut() {
            Some(link) => {
                let reserved = [0u8; SEALED_RESERVED_HEADER_BYTES];
                let (seq, packet) = pc.conn.path_probe(now, &reserved);
                match link.seal(SealedKind::Payload, &packet[SEALED_RESERVED_HEADER_BYTES..]) {
                    Ok(sealed) => {
                        self.totals.sealed_out += 1;
                        Some((seq, sealed))
                    }
                    Err(_) => None,
                }
            }
            None => {
                let header = connected_header(PacketKind::Payload, pc.token);
                Some(pc.conn.path_probe(now, &header))
            }
        };
        let Some((seq, bytes)) = bytes else {
            return;
        };
        pc.path.probed(to, seq, bytes.len(), now);
        self.totals.path_probes += 1;
        self.emit(bytes, to);
    }

    /// True for a peer on a sealed link that authenticated traffic recently.
    fn is_live_sealed_peer(&self, peer: PeerId) -> bool {
        self.peer_sealed(peer)
            && self
                .session
                .peer(peer)
                .is_some_and(|p| self.now_ms.saturating_sub(p.last_seen_ms) < SEALED_PEER_QUIET_MS)
    }

    /// Refuses a join on the path it arrived by: sealed once there are keys. A pending resume's
    /// client already holds its accept, so its refusal is a sealed `Disconnect` on that link
    /// (ADR-8858 A1), repeated like any goodbye.
    fn deny_join(
        &mut self,
        from: SocketAddr,
        nonce: u64,
        reason: DenyReason,
        detail: &str,
        path: &mut JoinPath,
    ) {
        let pending = match path {
            JoinPath::Plain => {
                self.deny(from, nonce, reason, detail);
                return;
            }
            JoinPath::Sealed => None,
            JoinPath::Resume(pending) => Some(pending),
        };
        self.totals.denied += 1;
        tracing::info!(target: "regia_net", %from, reason = reason.as_str(), detail, "join denied");
        if let Some(pending) = pending {
            let goodbye = match reason {
                DenyReason::Full => DisconnectReason::ServerFull,
                DenyReason::Banned => DisconnectReason::Banned,
                DenyReason::ShuttingDown => DisconnectReason::ServerShutdown,
                _ => DisconnectReason::ProtocolError,
            };
            for _ in 0..DISCONNECT_REPEATS {
                // A fresh counter each time, or the copies would be replays.
                if let Ok(bytes) = pending.link.seal(SealedKind::Disconnect, &[goodbye.to_u8()]) {
                    self.totals.sealed_out += 1;
                    self.emit(bytes, from);
                }
            }
            return;
        }
        let deny = Deny {
            client_nonce: nonce,
            reason,
            detail: detail.to_owned(),
        };
        let bytes = self
            .pending
            .get_mut(from)
            .and_then(|e| e.link.seal(SealedKind::Deny, &deny.to_body()).ok());
        if let Some(bytes) = bytes {
            self.totals.sealed_out += 1;
            self.emit(bytes, from);
        }
    }

    fn on_challenge_response(&mut self, body: &[u8], from: SocketAddr) {
        let Ok(resp) = ChallengeResponse::decode(body) else {
            self.totals.malformed += 1;
            return;
        };
        let Some(minted_ms) = self
            .minter
            .verify(from, resp.client_nonce, &resp.cookie, self.now_ms)
        else {
            self.totals.malformed += 1;
            self.refresh_challenge(from, resp.client_nonce, PLAIN_VERSION, body.len() + 5);
            return;
        };
        let proved = self
            .answer_governor
            .take_proved(from, resp.client_nonce, self.now_ms);
        if self.config.crypto.requires(from) {
            let detail = "this link must be encrypted";
            self.deny_after_proof(
                proved,
                from,
                resp.client_nonce,
                DenyReason::EncryptionRequired,
                detail,
            );
            return;
        }
        if let Some(proved) = proved {
            self.answer_governor.refund_proved(from.ip(), proved, self.now_ms);
        }
        let pw_nonce = self.minter.password_nonce(from, resp.client_nonce, minted_ms);
        self.admit(from, resp.client_nonce, resp.token, pw_nonce, JoinPath::Plain);
    }

    /// The join checks, shared by the plain path, the sealed path and the key confirmation of a
    /// one-round-trip resume. `pw_nonce` is the nonce the proof answers; [`JoinPath::Sealed`]
    /// takes the session keys from the pending handshake of `from`, [`JoinPath::Resume`] brings
    /// its own link and the accept its client already holds.
    fn admit(
        &mut self,
        from: SocketAddr,
        nonce: u64,
        join_token: JoinToken,
        pw_nonce: [u8; 16],
        mut path: JoinPath,
    ) {
        let sealed = &mut path;
        let resuming = matches!(sealed, JoinPath::Resume(_));
        // After the cookie, so a forged source cannot use up a real client's joins. A resume's
        // flight was charged when it was answered (ADR-8858 A4).
        if !resuming && !self.is_relay_ip(from.ip()) && !self.joins.admit(from.ip(), self.now_ms) {
            return;
        }
        let existing = self.session.peer_by_addr(from);
        if let Some(existing) = existing
            && self
                .conns
                .get(&existing)
                .is_some_and(|pc| pc.client_nonce == nonce)
        {
            // Our accept was lost; answer again. The proof of address (cookie) or the session
            // key (sealed) shows the client is alive: keep it from timing out meanwhile.
            self.session.touch(existing, self.now_ms);
            self.send_accept(existing, from);
            return;
        }
        if self.admitted.admitted_elsewhere(from, nonce, self.now_ms) {
            // This answer already made a peer from another port of its IP, and its cookie and
            // password proof bind the IP only: a copy, not a new client. No peer and no answer.
            self.totals.handshakes_refused += 1;
            return;
        }
        if self.admitted.admitted_here(from, nonce, self.now_ms) {
            // The same answer from the same address, after the peer it made left that address
            // or that nonce: a stale copy of an attempt the client already moved on from. On
            // wifi-to-cell its ticket may be spent by then, and admitting it would make a ghost
            // second peer for the same player (ADR-8623). No peer and no answer.
            self.totals.handshakes_refused += 1;
            return;
        }
        if self.closed {
            self.deny_join(from, nonce, DenyReason::ShuttingDown, "server is closing", sealed);
            return;
        }
        if self.session.is_banned(from.ip(), self.now_unix_s) {
            self.deny_join(from, nonce, DenyReason::Banned, "banned", sealed);
            return;
        }
        // A resume presents no proof: its ticket says the password was verified, and the flight
        // was refused to the full handshake otherwise (`check_resume_hello`).
        if !resuming && !self.gate.accepts(&pw_nonce, &join_token.password_proof) {
            let detail = "wrong or missing password";
            self.deny_join(from, nonce, DenyReason::BadPassword, detail, sealed);
            // One key exchange buys one guess. If the refusal is lost the client times out.
            if matches!(sealed, JoinPath::Sealed) {
                self.pending.remove(from);
            }
            return;
        }
        if !self.config.build.is_empty() && self.config.build != join_token.build {
            let detail = format!(
                "server build {}, client build {}",
                self.config.build, join_token.build
            );
            self.deny_join(from, nonce, DenyReason::BuildMismatch, &detail, sealed);
            return;
        }
        let join = match G::JoinData::from_bytes(&join_token.join_data) {
            Ok(join) => join,
            Err(e) => {
                let detail = format!("join data: {e}");
                self.deny_join(from, nonce, DenyReason::InvalidJoin, &detail, sealed);
                return;
            }
        };
        if let Err(reason) = G::validate_join(&join) {
            self.deny_join(from, nonce, DenyReason::InvalidJoin, &reason, sealed);
            return;
        }
        // A resume ticket names the peer this join comes back as (spec 120 § 10.3). One that
        // matches nothing (spent, expired, forged) is a fresh join. A ticket is spent once the
        // client proves it holds the newer one (ADR-8535): until then a retry with it after a
        // lost accept is the same resume again.
        let resume_match = join_token.resume.as_ref().and_then(|t| self.ticket_owner(t));
        let resume_target = resume_match.map(|(peer, _)| peer);
        let resume_retry = resume_match.is_some_and(|(_, retry)| retry);
        // A ticket of a player that left on purpose names no peer, but its block's held slot
        // (ADR-8733): the join is a new peer that the per-address cap does not count again. Only
        // from the hold's own block (ADR-8883): from another one it is a fresh join, and the hold
        // stays for its player.
        self.address_holds.expire(self.now_ms);
        let held_return = if resume_target.is_none() {
            join_token
                .resume
                .as_ref()
                .and_then(|t| self.address_holds.find(t))
                .filter(|held| same_block(*held, from.ip()))
        } else {
            None
        };
        if join_token.resume.is_some() && resume_target.is_none() && held_return.is_none() {
            self.totals.resumes_refused += 1;
        }
        // A pending resume is admitted as what its flight was answered for, or not at all: the
        // same peer with the ticket's epoch still inside its window (another resume confirmed
        // since made it single use, ADR-8858 A2), or the held slot from its own block. It never
        // becomes a fresh join.
        if let JoinPath::Resume(pending) = &*sealed {
            let stands = match (pending.target, resume_target) {
                (ResumeTarget::Peer(want), Some(target)) if want == target => self
                    .resume_epochs(target)
                    .ok_or(ResumeRefusal::NoPeer)
                    .and_then(|epochs| epochs.check(pending.claims.epoch)),
                (ResumeTarget::HeldSlot(_), None)
                    if held_return.is_some_and(|h| same_block(h, from.ip())) =>
                {
                    Ok(())
                }
                _ => Err(ResumeRefusal::NoPeer),
            };
            if let Err(why) = stands {
                self.note_resume_refusal(why);
                self.deny_join(from, nonce, DenyReason::InvalidJoin, why.as_str(), sealed);
                return;
            }
        }
        // Only a join that passed every check may replace the peer at its address, so a refused
        // one costs that peer nothing. The store hook runs after the live-peer, per-address and
        // capacity checks, so a restarted client, a full address and a full server do not open a
        // store session they will not keep. A quiet peer at this address is dropped (to dormant)
        // before the hook runs, so a join the hook then refuses has still replaced it.
        if let Some(existing) = existing
            && Some(existing) != resume_target
        {
            if self.is_live_sealed_peer(existing) {
                // Reading traffic and forging its source is enough to get here. A restarted
                // client keeps resending until the old peer has gone quiet.
                self.totals.handshakes_refused += 1;
                if resuming {
                    // A resume's client believes it is connected: tell it.
                    let detail = "another live connection holds this address";
                    self.deny_join(from, nonce, DenyReason::Full, detail, sealed);
                }
                return;
            }
            // The old peer went quiet and a new handshake proved this address: a restarted
            // client, or a NAT that gave the port to the same player again. That is a lost link,
            // not a quit: `Timeout` lets the session hold the player's grace slot for the token
            // the new link will present (spec 120 § 10.3, adversarial row 8).
            self.drop_peer(existing, DisconnectReason::Timeout, false);
        }
        let name = join_token.name.clone();
        let role = join_token.role;
        if self.per_address_refuses(from.ip(), resume_target, held_return) {
            let detail = "too many connections from this address";
            self.deny_join(from, nonce, DenyReason::Full, detail, sealed);
            return;
        }
        if self.ticket_check.is_some() && resume_target.is_none() {
            // The same store ticket as a dormant hold: that player is back without its resume
            // token (a restarted process). Its hold, and the store session it kept, end first, so
            // the store does not see the ticket twice and the held slot is free again.
            let ticket = G::join_ticket(&join);
            let returning: Vec<PeerId> = self
                .dormant
                .iter()
                .filter(|(_, d)| d.store_ticket.as_deref() == Some(ticket))
                .map(|(peer, _)| *peer)
                .collect();
            for peer in returning {
                self.end_dormancy(peer, DisconnectReason::Timeout);
            }
        }
        if resume_target.is_none() && self.session.is_full() {
            self.deny_join(from, nonce, DenyReason::Full, "server is full", sealed);
            return;
        }
        // A resume that presents the store ticket its peer already holds keeps that session: the
        // store vouched for these bytes, and checking them again would be a duplicate.
        let carried = resume_target.and_then(|target| match self.conns.get(&target) {
            Some(pc) => pc.store_ticket.clone(),
            None => self.dormant.get(&target).and_then(|d| d.store_ticket.clone()),
        });
        let accepted_ticket = if let Some(hook) = &self.ticket_check {
            let ticket = G::join_ticket(&join);
            if carried.as_deref() == Some(ticket) {
                carried
            } else {
                match hook.verdict(ticket) {
                    TicketVerdict::Refuse => {
                        self.deny_join(from, nonce, DenyReason::InvalidJoin, TicketRefused::CODE, sealed);
                        return;
                    }
                    TicketVerdict::Admit | TicketVerdict::Pending => Some(ticket.to_vec()),
                }
            }
        } else {
            None
        };
        let mut retry_tickets = UnconfirmedTokens::default();
        // A resumed peer keeps its resume epochs (ADR-8858 A2); a new peer starts at none.
        let mut epochs = ResumeEpochs::default();
        // The grade the resumed peer showed last, on its old connection or its hold (ADR-8889).
        let mut shown_before = None;
        let peer = match resume_target {
            Some(target) => {
                // The resumed peer's earlier store session (live or dormant) is replaced by the
                // one this join's ticket just opened.
                let mut old_store_ticket = None;
                // The tickets the client may still hold: the ones the old connection or hold kept
                // unconfirmed, and its newest, which the accept this join gets replaces (ADR-8623).
                let mut held = None;
                if let Some(old) = self.conns.remove(&target) {
                    // Live-token takeover: the old connection goes without a goodbye or an
                    // event; the peer's game state carries on under the new one. Its session
                    // token goes with it, the way `drop_peer` removes it: a packet that still
                    // carries it must not select this peer (review finding 4).
                    if self.tokens.get(&old.token) == Some(&target) {
                        self.tokens.remove(&old.token);
                    }
                    self.reassembly.release(old.conn.reliable_buffered_bytes());
                    shown_before = Some(old.quality.quality().grade);
                    old_store_ticket = old.store_ticket;
                    epochs = old.epochs;
                    held = Some((old.ticket, old.retry_tickets));
                    self.session.remove_peer(target);
                    self.driver.remove_peer(target);
                    self.governor.remove(&target);
                    self.totals.resume_takeovers += 1;
                }
                if let Some(d) = self.dormant.remove(&target) {
                    if held.is_none() {
                        epochs = d.epochs;
                    }
                    shown_before = shown_before.or(Some(d.grade));
                    old_store_ticket = old_store_ticket.or(d.store_ticket);
                    held = held.or(Some((d.ticket, d.retry_tickets)));
                }
                if let (Some((newest, chain)), Some(presented)) = (held, join_token.resume) {
                    retry_tickets =
                        chain.after_resume(newest, presented, self.now_ms, self.config.resume_grace_ms);
                }
                self.end_replaced_store_ticket(old_store_ticket, accepted_ticket.as_deref());
                if !self
                    .session
                    .add_peer_as(target, from, name.clone(), role, self.now_ms)
                {
                    self.end_store_ticket(accepted_ticket.as_deref());
                    self.deny_join(from, nonce, DenyReason::Full, "server is full", sealed);
                    return;
                }
                self.totals.resumes += 1;
                if resume_retry {
                    self.totals.resume_retries += 1;
                }
                target
            }
            None => {
                let added = match &*sealed {
                    // The held slot's return joins under the id its accept already named.
                    JoinPath::Resume(pending) => {
                        let id = pending.target.peer();
                        self.session
                            .add_peer_as(id, from, name.clone(), role, self.now_ms)
                            .then_some(id)
                    }
                    JoinPath::Plain | JoinPath::Sealed => {
                        self.session.add_peer(from, name.clone(), role, self.now_ms)
                    }
                };
                let Some(peer) = added else {
                    self.end_store_ticket(accepted_ticket.as_deref());
                    self.deny_join(from, nonce, DenyReason::Full, "server is full", sealed);
                    return;
                };
                // A held-slot return claims the hold and keeps the ticket it presented on the
                // new peer's unconfirmed chain (ADR-8883): until the client confirms, a retry
                // with that ticket after a lost accept is this peer again (a takeover, or a resume
                // once it went dormant), and the hold is spent only at the confirmation.
                if held_return.is_some()
                    && let Some(ticket) = join_token.resume
                    && self.address_holds.claim(&ticket)
                {
                    self.hold_claims.insert(peer, ticket);
                    retry_tickets =
                        retry_tickets.after_resume(ticket, ticket, self.now_ms, self.config.resume_grace_ms);
                }
                peer
            }
        };
        self.admitted.insert(from, nonce, self.now_ms);
        // A resume's link, tickets and epoch were made when its flight was answered.
        let (link, accepted) = match path {
            JoinPath::Plain => (None, None),
            JoinPath::Sealed => (self.pending.remove(from).map(|e| Box::new(e.link)), None),
            JoinPath::Resume(pending) => {
                let pending = *pending;
                if resume_target.is_none() {
                    epochs = pending.epochs;
                }
                (
                    Some(Box::new(pending.link)),
                    Some((pending.ticket, pending.resumption, pending.accept_epoch)),
                )
            }
        };
        let token = match &link {
            Some(link) => link.token(),
            None => {
                self.token_counter += 1;
                self.minter
                    .derive_u32(u64::from(peer.0), self.token_counter)
                    .max(1)
            }
        };
        let mut conn = Connection::new(self.config.link.clone());
        conn.set_tick_rate(self.config.tick_rate_hz);
        let header_bytes = if link.is_some() {
            SEALED_RESERVED_HEADER_BYTES
        } else {
            CONNECTED_HEADER_BYTES
        };
        conn.limit_packet_bytes(self.socket.max_datagram_size(from), header_bytes);
        let (ticket, resumption, accept_epoch) = match accepted {
            Some(accepted) => accepted,
            None => {
                let ticket = self.mint_ticket(peer);
                let psk = link.as_ref().map(|link| *link.resume_psk());
                let (resumption, accept_epoch) =
                    self.mint_resumption(peer, from, ticket, psk, &mut epochs, &join_token.build);
                (ticket, resumption, accept_epoch)
            }
        };
        self.conns.insert(
            peer,
            PeerConn {
                conn,
                token,
                client_nonce: nonce,
                last_sync_ms: None,
                pending_sync: None,
                inputs_too_far_ahead: 0,
                arrivals: InputArrivals::default(),
                pending_feedback: None,
                last_feedback: None,
                quality: QualityTracker::default(),
                link,
                unreliable: 0,
                reliable: 0,
                accept_resent: false,
                inbound: PeerInbound::new(&self.inbound_rates, self.now_ms),
                path: PathState::default(),
                rebinds: 0,
                rebinds_refused: 0,
                ticket,
                resumption,
                epochs,
                accept_epoch,
                store_ticket: accepted_ticket,
                // Every older ticket stays good for a retry until the client confirms.
                retry_tickets,
            },
        );
        self.tokens.insert(token, peer);
        if role == PeerRole::Player {
            self.driver.add_peer(peer);
        }
        self.totals.accepted += 1;
        tracing::info!(target: "regia_net", %peer, %from, name = %name, role = role.as_str(), "peer joined");
        if !resuming {
            self.send_accept(peer, from);
        }
        if resume_target.is_some() {
            tracing::info!(target: "regia_net", %peer, "peer resumed");
            self.events.push_back(ServerEvent::PeerResumed {
                peer,
                name,
                role,
                join,
            });
            // The new connection grades afresh; a scoreboard that showed the old one's grade
            // (`Interrupted` after the timeout) hears the change (ADR-8889).
            let to = self
                .conns
                .get(&peer)
                .map_or(LinkGrade::Good, |pc| pc.quality.quality().grade);
            if let Some(from) = shown_before.filter(|from| *from != to) {
                self.events
                    .push_back(quality_event(peer, crate::quality::GradeChange { from, to }));
            }
        } else {
            self.events.push_back(ServerEvent::PeerJoined {
                peer,
                name,
                role,
                join,
            });
        }
    }

    fn send_accept(&mut self, peer: PeerId, to: SocketAddr) {
        let tick_rate_hz = self.config.tick_rate_hz.min(u32::from(u16::MAX)) as u16;
        let server_tick = self.driver.tick();
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        let accept = Accept {
            client_nonce: pc.client_nonce,
            session_token: pc.token,
            peer_id: u16::try_from(peer.0).unwrap_or(u16::MAX),
            tick_rate_hz,
            server_tick,
            resume_ticket: pc.ticket,
            resumption: pc.resumption,
        };
        let bytes = match pc.link.as_mut() {
            Some(link) => link.seal(SealedKind::Accept, &accept.to_body()).ok(),
            None => Some(accept.to_datagram()),
        };
        if let Some(bytes) = bytes {
            self.totals.sealed_out += u64::from(pc.link.is_some());
            self.emit(bytes, to);
        }
    }

    fn connected_peer(&mut self, body: &[u8], from: SocketAddr) -> Option<(PeerId, usize)> {
        let token = u32::from_le_bytes(body.get(..4)?.try_into().ok()?);
        let peer = self.session.peer_by_addr_counted(from)?;
        (self.conns.get(&peer)?.token == token).then_some((peer, 4))
    }

    fn on_payload(&mut self, body: &[u8], from: SocketAddr) {
        let Some((peer, skip)) = self.connected_peer(body, from) else {
            if !self.take_unattributed() {
                return;
            }
            if !(self.session.peer_by_addr(from).is_none() && self.on_plain_rebind(body, from)) {
                self.totals.malformed += 1;
            }
            return;
        };
        if self.peer_sealed(peer) {
            // The token is visible on the wire; on a sealed link it proves nothing.
            self.totals.auth_failed += 1;
            return;
        }
        if self.over_rate(peer, body.len()) {
            return;
        }
        if let Some(pc) = self.conns.get_mut(&peer) {
            pc.path.heard_primary(self.now_ms);
        }
        self.deliver(peer, &body[skip..]);
    }

    /// Hands one authenticated packet body (on a plain link: one from the peer's validated
    /// address) to the peer's connection; its refusals score the peer (ADR-8606). A plain packet
    /// from an address the peer has not validated never comes here (ADR-10077, KI-NT-018): it is
    /// held until the address validates, and dropped if it never does.
    fn deliver(&mut self, peer: PeerId, packet: &[u8]) {
        let now = self.now_ms;
        let received = {
            let Some(pc) = self.conns.get_mut(&peer) else {
                return;
            };
            pc.conn
                .receive_within(packet, now, &mut self.reassembly)
                .map(|()| {
                    // The client holds this connection's accept: every older ticket is spent
                    // for good (ADR-8535, ADR-8623).
                    pc.retry_tickets.confirm();
                    // And every resumption ticket of an older epoch (ADR-8858 A2).
                    pc.epochs.confirm(pc.accept_epoch);
                    let mut messages = Vec::new();
                    while let Some(m) = pc.conn.next_message() {
                        messages.push(m);
                    }
                    messages
                })
        };
        if received.is_ok()
            && !self.hold_claims.is_empty()
            && let Some(ticket) = self.hold_claims.remove(&peer)
            && self.address_holds.take(&ticket)
        {
            // The held-slot return confirmed its accept: the hold is spent (ADR-8883).
            self.totals.held_slot_returns += 1;
        }
        let messages = match received {
            Ok(messages) => messages,
            Err(e) => {
                self.totals.malformed += 1;
                let failure = self.conns.get(&peer).and_then(|pc| pc.conn.failure());
                if let Some(failure) = failure {
                    // A protocol violation of the reliable stream (spec 120 § 5.2, § 11.2): named,
                    // and the sender goes at once.
                    let kind = RejectKind::from(failure);
                    tracing::info!(target: "regia_net", %peer, kind = kind.as_str(), error = %e, "peer failed its link");
                    self.raise_rejected(peer, kind, e);
                    self.drop_peer(peer, DisconnectReason::ProtocolError, true);
                } else if matches!(e, WireError::TooLong { .. }) {
                    self.drop_peer(peer, DisconnectReason::ProtocolError, true);
                } else {
                    // An authenticated packet the connection could not parse (ADR-8606): a damaged
                    // datagram fails its tag first, and a late or duplicated one is dropped before
                    // this, so only a broken or hostile sender gets here.
                    self.charge_malformed(peer, MALFORMED_WEIGHT);
                }
                return;
            }
        };
        self.session.touch(peer, now);
        // Restored only on proof that the peer hears the server too: a probe from a client that
        // no longer receives anything acknowledges nothing new (ADR-7995).
        let heard = self
            .conns
            .get(&peer)
            .is_some_and(|pc| pc.conn.peer_hears_us(now) && !pc.conn.rate_controller().breaker_tripped());
        if heard
            && let Some(p) = self.session.peer_mut(peer)
            && p.interrupted
        {
            p.interrupted = false;
            self.events.push_back(ServerEvent::PeerRestored { peer });
            if let Some(change) = self
                .conns
                .get_mut(&peer)
                .and_then(|pc| pc.quality.set_interrupted(false, now))
            {
                self.events.push_back(quality_event(peer, change));
            }
        }
        let is_player = self
            .session
            .peer(peer)
            .is_some_and(|p| p.role == PeerRole::Player);
        let mut bad = 0u32;
        let mut points = 0u32;
        let police = self.config.inbound.enabled;
        for (channel, bytes) in messages {
            let Some(pc) = self.conns.get_mut(&peer) else {
                return;
            };
            let count = if channel.is_reliable() {
                &mut pc.reliable
            } else {
                &mut pc.unreliable
            };
            *count += 1;
            let over_receive = *count > MAX_PEER_MESSAGES_PER_RECEIVE;
            // The message bucket (ADR-8181): over it the peer is over rate; an unreliable message
            // is dropped, a reliable one still delivered (it cannot be dropped).
            let over_rate = police && !pc.inbound.admit_message(now);
            if over_receive && channel.is_reliable() {
                // A reliable message cannot be dropped, so the peer goes. An unvalidated plain
                // packet never gets here (ADR-10077): what it carries is applied only once its
                // address validated.
                self.totals.inbound_dropped += 1;
                self.remove_for_flood(peer);
                return;
            }
            if !channel.is_reliable() && (over_receive || over_rate) {
                self.totals.inbound_dropped += 1;
                if police {
                    pc.inbound.note_message_dropped();
                    self.totals.over_rate += 1;
                }
                continue;
            }
            let mut r = WireReader::new(&bytes);
            let rejected = match r.u8() {
                Ok(LANE_GAME) => match G::Message::decode(&mut r).and_then(|m| r.finish().map(|()| m)) {
                    Ok(message) => {
                        if let Some(p) = self.session.peer_mut(peer) {
                            p.started = true;
                        }
                        self.events.push_back(ServerEvent::Message {
                            peer,
                            channel,
                            message,
                        });
                        None
                    }
                    Err(e) => Some((RejectKind::MessageMalformed, e)),
                },
                // The whole message is the bundle: bytes after it are an error (ADR-7892).
                Ok(LANE_INPUT) if is_player => {
                    match InputBundle::<G::Input>::decode(&mut r).and_then(|b| r.finish().map(|()| b)) {
                        Ok(bundle) => {
                            if let Some(p) = self.session.peer_mut(peer) {
                                p.started = true;
                            }
                            for (tick, input) in bundle.iter() {
                                self.store_input(peer, tick, input.clone());
                            }
                            None
                        }
                        Err(e @ WireError::TooLong { .. }) => Some((RejectKind::InputTooLong, e)),
                        Err(e) => Some((RejectKind::InputMalformed, e)),
                    }
                }
                // A spectator has no seat: its input is refused undecoded and scored, never
                // dropped for free (spec 120 § 11.2, ADR-7984).
                Ok(LANE_INPUT | LANE_INPUT_RELIABLE) if !is_player => {
                    Some((RejectKind::SpectatorInput, WireError::Invalid("spectator input")))
                }
                // Reliable only: the lane exists for inputs no datagram holds (ADR-7896). On the
                // unreliable channel it is an unknown lane. An input over the cap is fatal like an
                // over-long bundle (`InputTooLong`).
                Ok(LANE_INPUT_RELIABLE) if channel.is_reliable() => {
                    match decode_reliable_input::<G::Input>(&mut r) {
                        Ok((tick, input)) => {
                            if let Some(p) = self.session.peer_mut(peer) {
                                p.started = true;
                            }
                            self.totals.reliable_inputs += 1;
                            self.store_input(peer, tick, input);
                            None
                        }
                        Err(e @ WireError::TooLong { .. }) => Some((RejectKind::InputTooLong, e)),
                        Err(e) => Some((RejectKind::InputMalformed, e)),
                    }
                }
                // The client's clock stamp for the upstream jitter (ADR-8182): exactly a `u32`.
                Ok(LANE_CLOCK) => match r.u32().and_then(|ms| r.finish().map(|()| ms)) {
                    Ok(sent_ms) => {
                        if let Some(pc) = self.conns.get_mut(&peer) {
                            pc.quality.on_stamp(sent_ms, now);
                        }
                        None
                    }
                    Err(e) => Some((RejectKind::MessageMalformed, e)),
                },
                Ok(_) => Some((RejectKind::UnknownLane, WireError::Invalid("unknown lane"))),
                Err(e) => Some((RejectKind::UnknownLane, e)),
            };
            let Some((kind, error)) = rejected else {
                continue;
            };
            tracing::debug!(target: "regia_net", %peer, kind = kind.as_str(), %error, "message rejected");
            self.raise_rejected(peer, kind, error);
            if kind.is_fatal() {
                // An over-cap field is a protocol violation, as at the connection layer: the sender
                // goes at once, and nothing after it in the record is read (ADR-7892).
                self.totals.malformed += 1 + u64::from(bad);
                self.drop_peer(peer, DisconnectReason::ProtocolError, true);
                return;
            }
            bad += 1;
            points = points.saturating_add(kind.malformed_weight());
        }
        // No packet earns credit (ADR-8181, replacing ADR-7892's record credit): the score leaks
        // with time instead, so bad records mixed with good or empty ones still add up. Each
        // refusal scores its kind's weight (ADR-8606).
        if bad > 0 {
            self.totals.malformed += u64::from(bad);
            self.charge_malformed(peer, points);
        }
    }

    /// Adds `points` to an attributed peer's malformed score; removes it at the limit.
    fn charge_malformed(&mut self, peer: PeerId, points: u32) {
        let Some(pc) = self.conns.get_mut(&peer) else {
            return;
        };
        if pc
            .inbound
            .charge_malformed(self.now_ms, points, MALFORMED_SCORE_LIMIT)
        {
            self.drop_peer(peer, DisconnectReason::MalformedPacket, true);
        }
    }

    /// One connected peer's inbound policing state (ADR-8181); `None` for an unknown peer.
    pub fn peer_inbound(&self, peer: PeerId) -> Option<PeerInboundStatus> {
        let pc = self.conns.get(&peer)?;
        Some(PeerInboundStatus {
            over_rate_drops: pc.inbound.dropped,
            flood_ms: pc.inbound.flood_ms(),
            malformed_points: pc.inbound.malformed_points(),
        })
    }

    /// The inbound rates in force, with the derived defaults filled in (ADR-8181).
    pub fn inbound_rates(&self) -> InboundRates {
        self.inbound_rates
    }

    fn on_disconnect(&mut self, body: &[u8], from: SocketAddr) {
        let Some((peer, _)) = self.connected_peer(body, from) else {
            return;
        };
        if self.peer_sealed(peer) {
            self.totals.auth_failed += 1;
            return;
        }
        // Clients only ever leave; whatever reason byte they send is reported as `Left`.
        self.drop_peer(peer, DisconnectReason::Left, false);
    }

    fn store_input(&mut self, peer: PeerId, tick: u64, input: G::Input) {
        let server_tick = self.driver.tick();
        if tick >= server_tick && tick - server_tick >= self.config.input_buffer_ticks.max(1) as u64 {
            self.totals.inputs_too_far_ahead += 1;
            if let Some(pc) = self.conns.get_mut(&peer) {
                pc.inputs_too_far_ahead += 1;
            }
        } else if let Some(pc) = self.conns.get_mut(&peer) {
            // Arrival margins of first copies feed the client's input lead (ADR-8077).
            pc.arrivals.record(tick, server_tick, self.now_ms);
        }
        self.driver.push_input(peer, tick, input);
    }

    // Used by the ECS bridge and session adapter only (ADR-8032).
    #[cfg(feature = "ecs")]
    pub(crate) fn max_unreliable_message_bytes(&self, peer: PeerId) -> Option<usize> {
        self.conns
            .get(&peer)
            .map(|pc| pc.conn.max_unreliable_bytes(pc.header_bytes()).saturating_sub(1))
    }

    /// Builds and sends every due packet (messages, acks, keepalives, tick syncs) and releases
    /// conditioner-held datagrams.
    pub fn flush(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("regia_net::server_flush").entered();
        self.now_ms = self.now_ms.max(now_ms);
        self.stamp_unix_clock();
        let now = self.now_ms;
        let tick = self.driver.tick();
        self.share_uplink(now);
        for (peer, pc) in &mut self.conns {
            let Some(addr) = self.session.peer(*peer).map(|p| p.addr) else {
                continue;
            };
            let inputs = pc.arrivals.active().then(|| {
                let (late, missing) = pc.arrivals.totals();
                InputTotals {
                    server_tick: tick,
                    late,
                    missing,
                    margin_p5: pc.last_feedback.map_or(0, |f| f.margin_p5),
                }
            });
            if let Some(change) = pc.quality.sample(now, &pc.conn, inputs, None) {
                self.events.push_back(quality_event(*peer, change));
            }
            pc.conn
                .limit_packet_bytes(self.socket.max_datagram_size(addr), pc.header_bytes());
            if pc
                .last_sync_ms
                .is_none_or(|t| now.saturating_sub(t) >= self.config.tick_sync_interval_ms)
            {
                let mut w = WireWriter::with_capacity(13);
                w.u8(LANE_TICK_SYNC);
                w.u64(tick);
                // The server's clock stamp for the client's downstream jitter (ADR-8182).
                w.u32(now as u32);
                // Cannot exceed the unreliable limit: thirteen bytes.
                let header_bytes = pc.header_bytes();
                if let Some(id) = pc.pending_sync.take() {
                    pc.conn.cancel_essential(id);
                }
                pc.pending_sync = pc.conn.queue_unreliable(w.finish(), header_bytes, true).ok();
                if pc.arrivals.active() {
                    queue_feedback(pc, tick, now, self.config.input_buffer_ticks);
                }
            } else if pc.arrivals.urgent_due(tick) {
                // A late or missing input: tell the client now, not at the next sync (§ 6.4).
                queue_feedback(pc, tick, now, self.config.input_buffer_ticks);
            }
            let reserved = [0; SEALED_RESERVED_HEADER_BYTES];
            let plain_header = connected_header(PacketKind::Payload, pc.token);
            let header: &[u8] = if pc.link.is_some() {
                &reserved
            } else {
                &plain_header
            };
            let path = &mut pc.path;
            pc.conn.poll_transmit(now, header, |packet| {
                let bytes = if let Some(link) = pc.link.as_mut() {
                    match link.seal(SealedKind::Payload, &packet[SEALED_RESERVED_HEADER_BYTES..]) {
                        Ok(bytes) => {
                            self.totals.sealed_out += 1;
                            bytes
                        }
                        Err(error) => {
                            tracing::warn!(target: "regia_net", %peer, %error, "seal failed");
                            return Err(crate::socket::TxRefused::TooLarge(
                                self.socket.max_datagram_size(addr),
                            ));
                        }
                    }
                } else {
                    packet
                };
                // The validated path went silent while a candidate sends: a copy goes there too,
                // within three times what it sent (ADR-8422; RFC 9000 §§ 8.1, 9.3).
                let mirror = path.mirror_target(bytes.len()).map(|to| (to, bytes.clone()));
                crate::socket::transmit_packet(
                    &mut self.socket,
                    &mut self.conditioner,
                    &mut self.totals,
                    bytes,
                    addr,
                    now,
                )?;
                if let Some((to, copy)) = mirror {
                    path.mirrored(to, copy.len());
                    self.totals.path_mirrored += 1;
                    // A copy the socket refuses changes nothing on the validated path.
                    let _ = crate::socket::transmit_packet(
                        &mut self.socket,
                        &mut self.conditioner,
                        &mut self.totals,
                        copy,
                        to,
                        now,
                    );
                }
                Ok(())
            });
            if pc.pending_feedback.is_some_and(|id| pc.conn.was_packed(id)) {
                pc.pending_feedback = None;
            }
            if pc.pending_sync.is_some_and(|id| pc.conn.was_packed(id)) {
                pc.last_sync_ms = Some(now);
                pc.pending_sync = None;
            }
            if let Some(p) = self.session.peer_mut(*peer) {
                p.link = pc.conn.stats();
            }
        }
        for (bytes, dest) in self.conditioner.release_due(now) {
            self.write_socket(&bytes, dest);
        }
    }

    /// Splits the shared uplink among the peers (spec 120 § 4.2 item 8): each peer's pacing rate
    /// is capped at `min(its own target, its weighted max-min share)`.
    fn share_uplink(&mut self, now: u64) {
        let Some(uplink) = self.uplink.as_mut() else {
            return;
        };
        let mut blocks = Vec::with_capacity(self.conns.len());
        let peers: Vec<crate::cc::UplinkPeer> = self
            .conns
            .iter()
            .map(|(id, pc)| {
                let c = pc.conn.rate_controller();
                let info = self.session.peer(*id);
                let spectator = info.is_some_and(|p| p.role == PeerRole::Spectator);
                // One witness of the host queue per address block (ADR-8883): the connections
                // of one machine count once. A relay's address carries many players, so each
                // relayed peer (and one with no address) is its own /64, named by its id inside
                // the IANA-reserved 100::/8, which no client address can share.
                let block = match info.map(|p| p.addr.ip()) {
                    Some(ip) if !self.relay_ips.contains(&ip) => crate::addr_block::AddrBlock::of(ip),
                    _ => crate::addr_block::AddrBlock::of(std::net::IpAddr::V6(std::net::Ipv6Addr::from(
                        (0x0100_u128 << 112) | (u128::from(id.0) << 64),
                    ))),
                };
                blocks.push(block);
                // The uplink carries every packet's headers too (ADR-10097): the peers' rates
                // go in as rates on the wire, and each share comes back as one.
                let wire = pc.conn.wire_bytes_per_body_byte();
                crate::cc::UplinkPeer {
                    target: f64::from(c.target_bytes_per_second()) * wire,
                    delivery: c.delivery_bytes_per_second() * wire,
                    queue_ms: c.queue_delay_ms(),
                    weight: if spectator { 0.25 } else { 1.0 },
                    loss: c.loss_fraction(),
                    starved: pc.conn.unanswered_ms(now) >= pc.conn.starved_after_ms(),
                }
            })
            .collect();
        let shares = uplink.update_by_block(now, &peers, &blocks);
        let limited = uplink.target_bytes_per_second().is_some();
        for (pc, share) in self.conns.values_mut().zip(shares) {
            let wire = pc.conn.wire_bytes_per_body_byte();
            pc.conn.set_rate_cap(limited.then_some(share / wire));
            // Whether or not the aggregate limits now: this server's peers share one uplink, and
            // their own cuts keep reading the round (ADR-8859).
            pc.conn.set_shared_uplink(true);
        }
    }

    /// The shared uplink's aggregate target, bytes per second (`None` when the server has no
    /// shared uplink or it has not seen a queue yet).
    pub fn uplink_target_bytes_per_second(&self) -> Option<u32> {
        self.uplink.as_ref()?.target_bytes_per_second()
    }

    /// Bytes per second the game may spend on `peer` now (spec 120 § 4.2 item 9): the rate
    /// controller's allowance after the essential class. `None` for an unknown peer or with the
    /// controller off.
    pub fn send_allowance(&self, peer: PeerId) -> Option<u32> {
        self.conns.get(&peer)?.conn.send_allowance_bytes_per_second()
    }

    /// True while `peer`'s sender is still in its first flight (ADR-10156, opt-in; ADR-10200):
    /// the path has not answered yet, and [`Self::send_allowance`] is the essential
    /// floor's rate by design, not a measurement of the path. `false` for an unknown peer.
    pub fn in_first_flight(&self, peer: PeerId) -> bool {
        self.conns
            .get(&peer)
            .is_some_and(|c| c.conn.rate_controller().first_flight())
    }

    /// What one reliable stream of `peer`'s link holds now: fragments not yet acknowledged, the
    /// flow-control credit, the stall clock. `None` for an unknown peer or a stream the link does
    /// not declare. The successor journal reads the bulk stream's row to send one payload at a
    /// time (ADR-8853).
    pub fn stream_status(
        &self,
        peer: PeerId,
        stream: crate::connection::StreamId,
    ) -> Option<crate::connection::StreamStatus> {
        self.conns.get(&peer)?.conn.stream_status(stream)
    }

    /// When the pacer lets the next waiting packet to any peer leave: the host may call
    /// [`NetServer::flush`] again then instead of waiting for the next tick (spec 120 § 4.2
    /// item 7). `None` when nothing waits for a pacer.
    pub fn next_send_at_ms(&self) -> Option<u64> {
        self.conns
            .values()
            .filter_map(|pc| pc.conn.next_send_at_ms(self.now_ms))
            .min()
    }

    /// [`NetServer::receive`] then [`NetServer::flush`]. A late ticket refusal is applied between
    /// the two, so the goodbye goes out in the same flush.
    pub fn update(&mut self, now_ms: u64) {
        self.receive(now_ms);
        self.flush(now_ms);
    }

    // The session adapter supplies gameplay readiness; keepalives and Hello do not end grace.
    // Only the ECS session driver (`NetSession`) calls it.
    #[cfg(feature = "ecs")]
    pub(crate) fn set_started(&mut self, peer: PeerId, started: bool) {
        if let Some(p) = self.session.peer_mut(peer) {
            p.started = started;
        }
    }

    /// Kicks peers whose store hook reported a late refusal, and ends the resume hold of a
    /// dormant player whose ticket the store refused. Runs at the end of every
    /// [`Self::receive`].
    fn apply_ticket_refusals(&mut self) {
        let Some(hook) = &self.ticket_check else {
            return;
        };
        let refused = hook.take_refusals();
        if refused.is_empty() {
            return;
        }
        let held = |ticket: &Option<Vec<u8>>| {
            ticket
                .as_ref()
                .is_some_and(|ticket| refused.iter().any(|row| row == ticket))
        };
        let peers: Vec<PeerId> = self
            .conns
            .iter()
            .filter(|(_, pc)| held(&pc.store_ticket))
            .map(|(id, _)| *id)
            .collect();
        let dormant: Vec<PeerId> = self
            .dormant
            .iter()
            .filter(|(_, d)| held(&d.store_ticket))
            .map(|(id, _)| *id)
            .collect();
        for peer in peers {
            self.drop_peer(peer, DisconnectReason::Kicked, true);
        }
        for peer in dormant {
            self.end_dormancy(peer, DisconnectReason::Kicked);
        }
    }

    /// The `net.status` view of this server.
    pub fn status(&self) -> NetStatus {
        NetStatus {
            role: "server".into(),
            interrupted: self.session.peers().any(|p| p.interrupted),
            state: if self.closed {
                "closed".into()
            } else {
                "listening".into()
            },
            local_addr: self.local_addr().map(|a| a.to_string()),
            remote_addr: None,
            peer_id: None,
            tick: self.driver.tick(),
            tick_rate_hz: self.config.tick_rate_hz,
            peer_count: self.session.len(),
            max_peers: self.session.max_peers(),
            conditioner: self.conditioner.config().clone(),
            conditioner_stats: self.conditioner.stats(),
            totals: self.totals,
            link: None,
            crypto: self.crypto_status(),
            version_mismatch: self.version_mismatch.clone(),
            input_timing: None,
            reconnect: None,
            streams: Vec::new(),
            socket: self.socket.socket_report(),
            resume_refusals: ResumeRefusal::ALL
                .iter()
                .filter(|why| self.resume_refusals[why.index()] > 0)
                .map(|why| (why.as_str().to_owned(), self.resume_refusals[why.index()]))
                .collect(),
            quality: None,
        }
    }

    fn crypto_status(&self) -> CryptoStatus {
        let sealed_peers = self.conns.values().filter(|pc| pc.link.is_some()).count();
        CryptoStatus {
            mode: self.config.crypto.as_str().into(),
            suite: NOISE_PATTERN.into(),
            server_key: self.server_key().map(|k| k.to_string()),
            pinned: false,
            sealed: sealed_peers > 0 && sealed_peers == self.conns.len(),
            sealed_peers,
            rekeys: self
                .conns
                .values()
                .filter_map(|pc| pc.link.as_ref())
                .map(|link| link.rekeys())
                .sum(),
        }
    }

    /// One peer's connection quality (spec 120 § 13.1, ADR-8182); `None` for an unknown peer.
    pub fn peer_quality(&self, peer: PeerId) -> Option<PeerQuality> {
        self.conns.get(&peer).map(|pc| pc.quality.quality())
    }

    /// The `net.peers` rows.
    pub fn peer_rows(&self) -> Vec<PeerRow> {
        self.session
            .peers()
            .map(|p| {
                let health = self.session.health(p.id, self.now_ms);
                PeerRow {
                    id: p.id.0,
                    name: p.name.clone(),
                    role: p.role.as_str().into(),
                    addr: p.addr.to_string(),
                    rtt_ms: p.link.rtt_ms,
                    jitter_ms: p.link.jitter_ms,
                    loss_fraction: p.link.loss_fraction,
                    silent_ms: health.map_or(0, |h| h.silent_ms),
                    flagged: health.is_some_and(|h| h.flagged),
                    interrupted: p.interrupted,
                    missed_input_ticks: p.missed_input_ticks,
                    inputs_too_far_ahead: self.conns.get(&p.id).map_or(0, |pc| pc.inputs_too_far_ahead),
                    input_feedback: self.conns.get(&p.id).and_then(|pc| pc.last_feedback),
                    grade: self
                        .conns
                        .get(&p.id)
                        .map_or(LinkGrade::Good, |pc| pc.quality.quality().grade),
                    quality: self.conns.get(&p.id).map(|pc| pc.quality.quality()),
                    link: p.link,
                    sealed: self.peer_sealed(p.id),
                    rebinds: self.conns.get(&p.id).map_or(0, |pc| pc.rebinds),
                    rebinds_refused: self.conns.get(&p.id).map_or(0, |pc| pc.rebinds_refused),
                    path_candidate: self
                        .conns
                        .get(&p.id)
                        .and_then(|pc| pc.path.newest())
                        .map(|a| a.to_string()),
                    path_probes: self.conns.get(&p.id).map_or(0, |pc| pc.path.probes),
                    path_mirrored: self.conns.get(&p.id).map_or(0, |pc| pc.path.mirrored),
                    inbound: self.peer_inbound(p.id).unwrap_or_default(),
                    streams: self
                        .conns
                        .get(&p.id)
                        .map_or_else(Vec::new, |pc| pc.conn.stream_statuses()),
                }
            })
            .collect()
    }

    /// Overlay data for a HUD.
    pub fn overlay(&self) -> DiagnosticsOverlay {
        DiagnosticsOverlay::new(self.status(), self.peer_rows())
    }

    /// Publishes this server into the agent bridge under `label` and applies a pending
    /// `net.conditioner_set` request for it.
    pub fn publish(&mut self, label: &str, state: &mut NetAgentState) {
        if let Some(config) = state.take_conditioner_for(label) {
            tracing::info!(target: "regia_net", label, "conditioner changed by the agent");
            self.set_conditioner(config);
        }
        state.publish(
            label,
            EndpointReport {
                status: self.status(),
                peers: self.peer_rows(),
                rejections: self.rejections_by_peer.rows(),
            },
        );
    }
}

fn quality_event<G: NetGame>(peer: PeerId, change: GradeChange) -> ServerEvent<G> {
    ServerEvent::QualityChanged {
        peer,
        from: change.from,
        to: change.to,
    }
}

/// Queues one [`LinkFeedback`] for `pc` on the essential unreliable class, replacing an unsent
/// one (ADR-8077).
fn queue_feedback(pc: &mut PeerConn, tick: u64, now: u64, input_buffer_ticks: usize) {
    let fb = pc.arrivals.feedback(
        tick,
        now,
        pc.inputs_too_far_ahead,
        input_buffer_ticks,
        pc.quality.quality().loss_recv,
    );
    let mut w = WireWriter::with_capacity(1 + crate::feedback::LINK_FEEDBACK_BYTES);
    w.u8(LANE_LINK_FEEDBACK);
    fb.encode(&mut w);
    let header_bytes = pc.header_bytes();
    if let Some(id) = pc.pending_feedback.take() {
        pc.conn.cancel_essential(id);
    }
    pc.pending_feedback = pc.conn.queue_unreliable(w.finish(), header_bytes, true).ok();
    pc.last_feedback = Some(fb);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{ClientConfig, NetClient};
    use crate::handshake::connected_header;
    use crate::socket::{LoopbackNet, LoopbackSocket};
    use crate::wire::WireError;

    #[derive(Debug, Clone, Copy)]
    struct Mini;

    #[derive(Debug, Clone, PartialEq)]
    struct Bytes(Vec<u8>);

    impl NetCodec for Bytes {
        fn encode(&self, w: &mut WireWriter) {
            self.0.encode(w);
        }
        fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
            Ok(Self(Vec::<u8>::decode(r)?))
        }
    }

    impl NetGame for Mini {
        const GAME_ID: &'static str = "server-tests";
        const PROTOCOL_VERSION: u32 = 1;
        type JoinData = ();
        type Input = u8;
        type Message = Bytes;
    }

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, last], port))
    }

    /// Audit fix-net3-determinism M2 (ADR-4772): a ban's stored expiry and the checks against it
    /// are functions of the injected clock, not of the system clock.
    #[test]
    fn ban_expiry_follows_the_injected_unix_clock() {
        let net = LoopbackNet::new();
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(addr(1, 7000)).expect("bind"),
            ServerConfig::default(),
            [9; 32],
        );
        server.set_unix_now_s(1_000);
        server.ban_ip(addr(2, 5000).ip(), "spam", 60);
        let stored = server.session().bans()[0].expires_unix_s;
        assert_eq!(stored, 1_060, "the expiry is now + seconds on the injected clock");
        assert!(
            server.session().is_banned(addr(2, 5000).ip(), 1_059),
            "still banned before the expiry"
        );
        assert!(
            !server.session().is_banned(addr(2, 5000).ip(), 1_060),
            "the expiry second lifts the ban"
        );
    }

    #[test]
    fn exact_address_ban_expiry_follows_the_injected_unix_clock() {
        let net = LoopbackNet::new();
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(addr(1, 7000)).expect("bind"),
            ServerConfig::default(),
            [9; 32],
        );
        server.set_unix_now_s(1_000);
        server.ban_ip_exact(addr(2, 5000).ip(), "spam", 60);
        let stored = server.session().bans()[0].expires_unix_s;
        assert_eq!(stored, 1_060, "the expiry is now + seconds on the injected clock");
        assert!(
            server.session().is_banned(addr(2, 5000).ip(), 1_059),
            "still banned before the expiry"
        );
        assert!(
            !server.session().is_banned(addr(2, 5000).ip(), 1_060),
            "the expiry second lifts the ban"
        );
    }

    /// Audit fixrev-15-k net3-determinism H1 (ADR-5444): a directly driven `NetServer` never got a
    /// clock unless the host injected one, so a timed ban stored `0 + seconds` (an expiry in
    /// 1970): it stayed active for the server's whole life, and every ban expired at once the
    /// moment the host later injected the real clock. The server now carries the wall second at
    /// construction and derives the unix clock from the monotonic progression until an explicit
    /// injection.
    #[test]
    fn a_timed_ban_expires_on_the_derived_unix_clock() {
        let net = LoopbackNet::new();
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(addr(1, 7000)).expect("bind"),
            ServerConfig::default(),
            [9; 32],
        );
        let base = server.now_unix_s();
        assert!(
            base > 1_000_000_000,
            "the derived clock starts at the wall base, not 1970: {base}"
        );
        let ip = addr(2, 5000).ip();
        server.ban_ip(ip, "spam", 60);
        let stored = server.session().bans()[0].expires_unix_s;
        assert!(
            (base + 60..=base + 62).contains(&stored),
            "the expiry is the wall base + 60: stored {stored}, base {base}"
        );
        // Drive 61 seconds of the monotonic clock without any injection.
        for sec in 0..=61u64 {
            server.update(sec * 1000);
        }
        assert!(
            !server.session().is_banned(ip, server.now_unix_s()),
            "the ban lifted after 61 s on the derived clock"
        );
    }

    struct Rig {
        net: LoopbackNet,
        server: NetServer<Mini, LoopbackSocket>,
        _client: NetClient<Mini, LoopbackSocket>,
        peer: PeerId,
        token: u32,
        client_addr: SocketAddr,
    }

    /// A joined client on a plain link; tests forge plain packets from its address.
    fn rig(mut config: ServerConfig) -> Rig {
        config.crypto = CryptoMode::Off;
        let net = LoopbackNet::new();
        let mut server: NetServer<Mini, _> =
            NetServer::with_secret(net.bind(addr(1, 7000)).expect("bind"), config, [9; 32]);
        let client_addr = addr(2, 5000);
        let mut client = NetClient::<Mini, _>::connect(
            net.bind(client_addr).expect("bind"),
            addr(1, 7000),
            ClientConfig {
                crypto: CryptoMode::Off,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("fits");
        for now in 0..10 {
            client.update(now);
            server.receive(now);
            server.flush(now);
        }
        let (&peer, pc) = server.conns.iter().next().expect("joined");
        let token = pc.token;
        Rig {
            net,
            server,
            _client: client,
            peer,
            token,
            client_addr,
        }
    }

    // `send_essential_message` is the ECS bridge's (ADR-8032), so this test needs `ecs`.
    #[cfg(feature = "ecs")]
    #[test]
    fn tick_sync_timestamp_waits_for_packet_admission() {
        let mut r = rig(ServerConfig {
            tick_sync_interval_ms: 1,
            ..ServerConfig::default()
        });
        let pc = r.server.conns.get_mut(&r.peer).expect("peer");
        pc.conn = Connection::new(LinkConfig {
            max_packets_per_poll: 1,
            ..LinkConfig::default()
        });
        pc.last_sync_ms = None;
        for now in 10..42 {
            r.server.flush(now);
        }
        let sent_at = r.server.conns[&r.peer].last_sync_ms;
        assert_eq!(sent_at, Some(41));
        // Fill the actual one-packet budget with earlier essential traffic. An outstanding
        // ACK horizon is no longer an admission barrier (ADR-7988).
        let pc = &r.server.conns[&r.peer];
        let max = pc.conn.max_unreliable_bytes(pc.header_bytes());
        r.server
            .send_essential_message(r.peer, &Bytes(vec![0; max - 3]))
            .expect("one full packet ahead of sync");
        r.server.flush(42);
        assert_eq!(
            r.server.conns[&r.peer].last_sync_ms, sent_at,
            "a queued sync must not advance last_sync_ms when the packet budget is full"
        );
        r._client.update(43);
        r.server.receive(43);
        r.server.flush(43);
        assert_eq!(
            r.server.conns[&r.peer].last_sync_ms,
            Some(43),
            "the next poll admits the queued sync"
        );
    }

    impl Rig {
        /// Delivers `body` to the server as a payload from the client's address.
        fn forge(&mut self, seq: u16, records: &[u8], now: u64) {
            let mut w = WireWriter::new();
            w.raw(&connected_header(PacketKind::Payload, self.token));
            // Wire 5: flags (a 2-byte packet number, no acknowledgment frame), then a number past
            // the client's own handshake traffic.
            w.u8(0x01);
            w.u16(seq + 500);
            w.raw(records);
            let bytes = w.finish();
            self.server.handle_datagram(&bytes, self.client_addr);
            self.server.now_ms = now;
        }

        fn begin_receive(&mut self) {
            for pc in self.server.conns.values_mut() {
                pc.unreliable = 0;
                pc.reliable = 0;
                pc.accept_resent = false;
            }
        }
    }

    fn events(s: &mut NetServer<Mini, LoopbackSocket>) -> Vec<ServerEvent<Mini>> {
        std::iter::from_fn(|| s.poll_event()).collect()
    }

    fn left_with(evs: &[ServerEvent<Mini>], reason: DisconnectReason) -> bool {
        evs.iter()
            .any(|e| matches!(e, ServerEvent::PeerLeft { reason: r, .. } if *r == reason))
    }

    /// `n` empty game messages as unreliable records: lane byte + a zero-length payload.
    fn unreliable_records(n: usize) -> Vec<u8> {
        let mut w = WireWriter::new();
        for _ in 0..n {
            w.u8(0x01);
            w.u16(3);
            w.u8(LANE_GAME);
            w.u16(0);
        }
        w.finish()
    }

    fn reliable_records(first_id: u16, n: u16) -> Vec<u8> {
        let mut w = WireWriter::new();
        for i in 0..n {
            w.u8(0x02);
            w.u16(first_id.wrapping_add(i));
            w.u16(3);
            w.u8(LANE_GAME);
            w.u16(0);
        }
        w.finish()
    }

    /// Audit H1 (muse2): one peer delivers a bounded number of messages per receive. Unreliable
    /// excess is dropped and the peer stays.
    #[test]
    fn unreliable_excess_is_dropped_per_receive() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let packets = MAX_PEER_MESSAGES_PER_RECEIVE as usize / 128 + 2;
        r.begin_receive();
        for i in 0..packets {
            r.forge(i as u16 + 1, &unreliable_records(128), 20);
        }
        let got = events(&mut r.server);
        assert_eq!(got.len(), MAX_PEER_MESSAGES_PER_RECEIVE as usize);
        assert_eq!(
            r.server.totals().inbound_dropped as usize,
            packets * 128 - MAX_PEER_MESSAGES_PER_RECEIVE as usize
        );
        assert!(r.server.conns.contains_key(&r.peer));
        // The next receive starts a fresh budget.
        r.begin_receive();
        r.forge(packets as u16 + 1, &unreliable_records(1), 21);
        assert_eq!(events(&mut r.server).len(), 1);
    }

    /// A reliable message past the cap cannot be dropped: the peer goes with `Flood`.
    #[test]
    fn reliable_excess_disconnects_with_flood() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        r.begin_receive();
        let mut id = 0u16;
        for i in 0..=(MAX_PEER_MESSAGES_PER_RECEIVE as u16 / 128) {
            r.forge(i + 1, &reliable_records(id, 128), 20);
            id = id.wrapping_add(128);
        }
        assert!(!r.server.conns.contains_key(&r.peer));
        assert!(left_with(&events(&mut r.server), DisconnectReason::Flood));
    }

    /// Audit H1 (muse2): garbage from a connected peer ends it even if a keepalive slips through
    /// now and then. A rare bad packet does not.
    #[test]
    fn sustained_garbage_removes_the_peer() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let bad_tag = [0x7Fu8, 0, 0];
        let mut seq = 0u16;
        // One packet a millisecond (1,000 a second, inside the inbound packet rate, ADR-8181).
        let mut now = 20;
        for _ in 0..12 {
            for _ in 0..100 {
                now += 1;
                r.forge(seq, &bad_tag, now);
            }
            seq += 1;
            now += 1;
            r.forge(seq, &[], now);
            seq += 1;
        }
        assert!(
            !r.server.conns.contains_key(&r.peer),
            "the keepalives bought nothing"
        );
        assert!(left_with(
            &events(&mut r.server),
            DisconnectReason::MalformedPacket
        ));

        // ADR-8181 replaced the credit with a leak in time: good traffic no longer pays bad traffic
        // off, so bad/good alternation is removed too (it used to stay forever).
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let mut now = 20;
        for i in 0..5000u16 {
            now += 1;
            r.forge(i * 2, &bad_tag, now);
            r.forge(i * 2 + 1, &[], now);
        }
        assert!(
            !r.server.conns.contains_key(&r.peer),
            "good traffic no longer pays bad traffic off"
        );
        assert!(left_with(
            &events(&mut r.server),
            DisconnectReason::MalformedPacket
        ));
    }

    /// A datagram with a wrong token never charges the peer whose address it claims.
    #[test]
    fn forged_packets_do_not_charge_the_victim() {
        let mut r = rig(ServerConfig::default());
        r.token = r.token.wrapping_add(1);
        for i in 0..(MALFORMED_SCORE_LIMIT as u16 + 8) {
            r.forge(i, &[0x7F, 0, 0], 20);
        }
        assert!(r.server.conns.contains_key(&r.peer));
    }

    /// Audit M3 (muse2) / M1 (grok), revised by finding 23 (ADR-3283) and the merge review
    /// (ADR-4936): a `ConnectRequest` is not charged to its unproved source, so a flood of them is
    /// answered one for one (each answer is no larger than its request) and spends no per-source
    /// budget; only the pre-proof answer budget (`pre_proof_answers_per_s`: per-prefix fair shares
    /// under a server-wide cap since ADR-8718; a prefix asking alone has all of it) bounds the
    /// answers, and the per-address budget binds the packets that bring a cookie back
    /// (`net_hardening_wave2::a_proved_source_is_still_held_to_its_handshake_budget`). A pre-join
    /// source can be banned: its `ConnectRequest` is still answered with a `Challenge` (the
    /// address is unproved, and the ban check was moved behind the cookie so the padded request
    /// no longer scans the ban list), and the ban refuses the join once the address proves
    /// itself, in `on_key_init` and the join admission
    /// (`loopback::kick_and_ban_reach_the_client_and_ban_blocks_rejoin`).
    #[test]
    fn connect_requests_spend_no_budget_and_a_pre_join_source_can_be_banned() {
        let mut r = rig(ServerConfig {
            handshakes_per_ip_per_s: 4,
            ..ServerConfig::default()
        });
        let request = ConnectRequest {
            protocol_id: crate::payload::protocol_id::<Mini>(),
            client_nonce: 7,
            crypto_version: CRYPTO_VERSION,
            crypto_required: false,
        }
        .to_datagram();
        let flood = addr(4, 1000);
        let mut answered = 0;
        let mut socket = r.net.bind(flood).expect("bind");
        for _ in 0..40 {
            r.server.handle_datagram(&request, flood);
            let mut buf = [0u8; 256];
            if socket.recv_from(&mut buf).expect("io").is_some() {
                answered += 1;
            }
        }
        assert_eq!(answered, 40, "a reply no larger than its request, never a budget");
        assert_eq!(r.server.handshakes_refused(), 0);
        let other = addr(5, 1000);
        let mut socket = r.net.bind(other).expect("bind");
        r.server.handle_datagram(&request, other);
        let mut buf = [0u8; 256];
        assert!(socket.recv_from(&mut buf).expect("io").is_some());
        r.server.ban_ip(other.ip(), "scanner", 0);
        r.server.now_ms += 10_000;
        r.server.handle_datagram(&request, other);
        let (n, _) = socket.recv_from(&mut buf).expect("io").expect("answer");
        // The ban check moved behind the cookie (ADR-4936): an unproved source still gets its
        // `Challenge`, and is refused once the address proves itself (in `on_key_init` / the join
        // admission), never at the ban-list scan this padded request used to cost.
        assert!(crate::handshake::split_datagram(&buf[..n]).is_some_and(|(k, _)| k == PacketKind::Challenge));
        assert!(
            r.server
                .session
                .is_banned(other.ip(), crate::session::unix_now_s())
        );
    }

    /// Merge review (ADR-4936): the answers a `ConnectRequest` can draw on are capped
    /// server-globally, so a spoofed-address flood cannot turn the server into a 1:1 reflector;
    /// over the cap a request is dropped without an answer. (All twenty sources here are one /24,
    /// which asks alone, so its fair share is the whole cap, ADR-8718.)
    #[test]
    fn pre_proof_answers_are_globally_shed() {
        let mut r = rig(ServerConfig {
            pre_proof_answers_per_s: 5,
            ..ServerConfig::default()
        });
        let request = ConnectRequest {
            protocol_id: crate::payload::protocol_id::<Mini>(),
            client_nonce: 7,
            crypto_version: CRYPTO_VERSION,
            crypto_required: false,
        }
        .to_datagram();
        // Every request names a different unproved address: no per-address cap can exist for them
        // (a forger would spend the victim's allowance), so only the global shed answers. A fresh
        // second, so the rig's own join handshake is not charged to this window.
        r.server.now_ms += 1_000;
        let mut answered = 0;
        for i in 0..20_u16 {
            let from = addr((20 + i) as u8, 1000);
            let mut socket = r.net.bind(from).expect("bind");
            r.server.handle_datagram(&request, from);
            let mut buf = [0u8; 256];
            if socket.recv_from(&mut buf).expect("io").is_some() {
                answered += 1;
            }
        }
        assert_eq!(answered, 5, "the cap, then silence");
        assert_eq!(r.server.totals().pre_proof_shed, 15);

        // The next second answers again, and `0` disables the shed entirely.
        r.server.now_ms += 1_000;
        let mut socket = r.net.bind(addr(50, 1000)).expect("bind");
        r.server.handle_datagram(&request, addr(50, 1000));
        let mut buf = [0u8; 256];
        assert!(socket.recv_from(&mut buf).expect("io").is_some(), "a new second");

        let mut open = rig(ServerConfig {
            pre_proof_answers_per_s: 0,
            ..ServerConfig::default()
        });
        let mut answered = 0;
        for i in 0..20_u16 {
            let from = addr((60 + i) as u8, 1000);
            let mut socket = open.net.bind(from).expect("bind");
            open.server.handle_datagram(&request, from);
            let mut buf = [0u8; 256];
            if socket.recv_from(&mut buf).expect("io").is_some() {
                answered += 1;
            }
        }
        assert_eq!(answered, 20, "0 = no limit");
    }

    /// Spec 120 § 11.4 (N218, ADR-8718; the soak's `forged-handshake` wedge): forged requests from
    /// 200 /24s, 20,000 a second (about five times the default cap), spend those prefixes' fair
    /// shares (about 19 answers a second each), and a real client in another /24 asking every
    /// 250 ms is answered every time. Before, one first-come server-wide cap was spent in the
    /// first fifth of every second, and the real client's requests after that went unanswered.
    #[test]
    fn a_forged_flood_from_many_prefixes_leaves_a_real_prefix_its_answers() {
        let mut r = rig(ServerConfig::default());
        assert_eq!(
            r.server.config.pre_proof_answers_per_s, 4_096,
            "spec 120 § 11.4's default"
        );
        let request = ConnectRequest {
            protocol_id: crate::payload::protocol_id::<Mini>(),
            client_nonce: 7,
            crypto_version: CRYPTO_VERSION,
            crypto_required: false,
        }
        .to_datagram();
        let real = SocketAddr::from(([10, 9, 0, 5], 1000));
        let mut socket = r.net.bind(real).expect("bind");
        let start = r.server.now_ms + 1_000;
        let (mut asked, mut answered) = (0, 0);
        for step in 0..300_u64 {
            // One server read per 10 ms step, as a host polls: each read has its own
            // unattributed-datagram allowance (`MAX_DATAGRAMS_PER_RECEIVE`).
            r.server.receive(start + step * 10);
            for k in 0..200_u64 {
                let p = (step * 200 + k) % 200;
                let forged = SocketAddr::from(([20, p as u8, 7, 9], 1000));
                r.server.handle_datagram(&request, forged);
            }
            // The flood's first second fills every bucket; judge the real client after it.
            if step >= 100 && step.is_multiple_of(25) {
                asked += 1;
                r.server.handle_datagram(&request, real);
                let mut buf = [0u8; 256];
                if socket.recv_from(&mut buf).expect("io").is_some() {
                    answered += 1;
                }
            }
        }
        assert_eq!(asked, 8);
        assert_eq!(answered, asked, "every real request answered under the flood");
        assert!(r.server.totals().pre_proof_shed > 40_000, "the flood was shed");
    }

    fn drain_kinds(socket: &mut LoopbackSocket) -> Vec<PacketKind> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = socket.recv_from(&mut buf).expect("loopback recv") {
            if let Some((kind, _)) = split_datagram(&buf[..n]) {
                out.push(kind);
            }
        }
        out
    }

    fn drain_parsed(socket: &mut LoopbackSocket) -> Vec<(PacketKind, Vec<u8>)> {
        let mut out = Vec::new();
        let mut buf = [0u8; 2048];
        while let Some((n, _)) = socket.recv_from(&mut buf).expect("loopback recv") {
            if let Some((kind, body)) = split_datagram(&buf[..n]) {
                let cookie = (kind == PacketKind::Challenge)
                    .then(|| Challenge::decode(body).ok().map(|c| c.cookie))
                    .flatten();
                out.push((kind, cookie.unwrap_or_default().to_vec()));
            }
        }
        out
    }

    /// Review (v06-net-soak-gate-reds): `CookieMinter::verify` accepts the same cookie for the
    /// whole time bucket. `on_key_init` refunds on every verify, before `key_exchanges` admits
    /// the packet, so replaying one cookie refills the prefix and the server-wide count.
    #[test]
    fn a_replayed_key_init_does_not_refill_the_pre_proof_cap() {
        let net = LoopbackNet::new();
        let cap: usize = 4;
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(addr(1, 7000)).expect("bind"),
            ServerConfig {
                crypto: CryptoMode::Required,
                pre_proof_answers_per_s: cap as u32,
                handshakes_per_ip_per_s: 1,
                ..ServerConfig::default()
            },
            [9; 32],
        );
        server.now_ms = 20_000;
        let from = addr(4, 1000);
        let mut socket = net.bind(from).expect("bind");
        let request = ConnectRequest {
            protocol_id: protocol_id::<Mini>(),
            client_nonce: 7,
            crypto_version: CRYPTO_VERSION,
            crypto_required: true,
        }
        .to_datagram();
        let cookie = {
            let mut found = None;
            let mut challenges: usize = 0;
            for _ in 0..cap {
                server.handle_datagram(&request, from);
                for (kind, body) in drain_parsed(&mut socket) {
                    if kind == PacketKind::Challenge {
                        challenges += 1;
                        if found.is_none() && body.len() == 16 {
                            let mut c = [0u8; 16];
                            c.copy_from_slice(&body);
                            found = Some(c);
                        }
                    }
                }
            }
            assert_eq!(challenges, cap);
            found.expect("cookie")
        };
        let init = KeyInit {
            client_nonce: 7,
            cookie,
            version: CRYPTO_VERSION,
            noise: [0; 32],
        }
        .to_datagram();
        for _ in 0..cap {
            server.handle_datagram(&init, from);
            let _ = drain_kinds(&mut socket);
        }
        let mut second_wave = 0;
        for _ in 0..cap {
            server.handle_datagram(&request, from);
            second_wave += drain_kinds(&mut socket)
                .into_iter()
                .filter(|k| *k == PacketKind::Challenge)
                .count();
        }
        assert!(
            second_wave <= 1,
            "one cookie replayed {cap} times funded {second_wave} more challenges after the cap of {cap} was spent"
        );
    }

    /// Review: on an encrypting server a valid `ChallengeResponse` is refunded and then refused
    /// with a plaintext `Deny` that never passes `try_admit`. Replays of one cookie are one
    /// `Deny` each, and they reopen the cap for another wave of challenges.
    #[test]
    fn a_replayed_challenge_response_is_not_an_uncapped_deny() {
        let net = LoopbackNet::new();
        let cap: usize = 4;
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(addr(1, 7100)).expect("bind"),
            ServerConfig {
                crypto: CryptoMode::Required,
                pre_proof_answers_per_s: cap as u32,
                ..ServerConfig::default()
            },
            [9; 32],
        );
        server.now_ms = 20_000;
        let from = addr(6, 1000);
        let mut socket = net.bind(from).expect("bind");
        let request = ConnectRequest {
            protocol_id: protocol_id::<Mini>(),
            client_nonce: 11,
            crypto_version: CRYPTO_VERSION,
            crypto_required: true,
        }
        .to_datagram();
        let mut cookie = [0u8; 16];
        let mut challenges: usize = 0;
        for _ in 0..cap {
            server.handle_datagram(&request, from);
            for (kind, body) in drain_parsed(&mut socket) {
                if kind == PacketKind::Challenge {
                    challenges += 1;
                    if body.len() == 16 {
                        cookie.copy_from_slice(&body);
                    }
                }
            }
        }
        assert_eq!(challenges, cap);
        let response = ChallengeResponse {
            client_nonce: 11,
            cookie,
            token: JoinToken {
                name: "a".to_owned(),
                role: PeerRole::Player,
                build: String::new(),
                password_proof: [0; 32],
                join_data: Vec::new(),
                resume: None,
            },
        }
        .to_datagram()
        .expect("token fits");
        let mut denies = 0;
        for _ in 0..cap {
            server.handle_datagram(&response, from);
            denies += drain_kinds(&mut socket)
                .into_iter()
                .filter(|k| *k == PacketKind::Deny)
                .count();
        }
        let mut second_wave = 0;
        for _ in 0..cap {
            server.handle_datagram(&request, from);
            second_wave += drain_kinds(&mut socket)
                .into_iter()
                .filter(|k| *k == PacketKind::Challenge)
                .count();
        }
        assert!(
            denies <= 1,
            "one cookie replayed {cap} times produced {denies} Denies, none of them admitted"
        );
        assert!(
            second_wave <= 1,
            "those refunds funded {second_wave} more challenges in the same second"
        );
    }

    /// ADR-8764: a proof gives back a slot of the count of the second its answer was sent in,
    /// never of a later one. A source that reads its own `Challenge`s in one second, then spends
    /// the next second's cap on requests forged from its neighbours' addresses, cannot use its
    /// still-valid cookies to reopen that cap for more answers to the neighbours.
    #[test]
    fn last_seconds_cookies_do_not_reopen_this_seconds_cap() {
        let net = LoopbackNet::new();
        let cap: usize = 4;
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(addr(1, 7200)).expect("bind"),
            ServerConfig {
                crypto: CryptoMode::Required,
                pre_proof_answers_per_s: cap as u32,
                ..ServerConfig::default()
            },
            [9; 32],
        );
        let request = |nonce: u64| {
            ConnectRequest {
                protocol_id: protocol_id::<Mini>(),
                client_nonce: nonce,
                crypto_version: CRYPTO_VERSION,
                crypto_required: true,
            }
            .to_datagram()
        };
        // Second 20: the reader collects two cookies of its own.
        server.now_ms = 20_000;
        let reader = addr(4, 1000);
        let mut reader_socket = net.bind(reader).expect("bind");
        let mut cookies = Vec::new();
        for nonce in [1, 2] {
            server.handle_datagram(&request(nonce), reader);
            for (kind, body) in drain_parsed(&mut reader_socket) {
                if kind == PacketKind::Challenge && body.len() == 16 {
                    let mut c = [0u8; 16];
                    c.copy_from_slice(&body);
                    cookies.push((nonce, c));
                }
            }
        }
        assert_eq!(cookies.len(), 2);
        // Second 21: requests forged from neighbours in the same /24 spend the cap.
        server.now_ms = 21_000;
        let mut neighbours: Vec<_> = (0..2 * cap as u16)
            .map(|k| {
                let a = addr(5, 2000 + k);
                (a, net.bind(a).expect("bind"))
            })
            .collect();
        let mut answered = 0;
        for (k, (a, socket)) in neighbours.iter_mut().take(cap).enumerate() {
            server.handle_datagram(&request(100 + k as u64), *a);
            answered += drain_kinds(socket).len();
        }
        assert_eq!(answered, cap, "the forged requests spent this second's cap");
        // Both cookies are valid (one cookie bucket) and prove last second's answers.
        for (nonce, cookie) in &cookies {
            let init = KeyInit {
                client_nonce: *nonce,
                cookie: *cookie,
                version: CRYPTO_VERSION,
                noise: [0; 32],
            }
            .to_datagram();
            server.handle_datagram(&init, reader);
            let _ = drain_kinds(&mut reader_socket);
        }
        let mut more = 0;
        for (k, (a, socket)) in neighbours.iter_mut().skip(cap).enumerate() {
            server.handle_datagram(&request(200 + k as u64), *a);
            more += drain_kinds(socket).len();
        }
        assert_eq!(
            more, 0,
            "last second's proofs reopened this second's cap for {more} more forged answers"
        );
    }

    /// Review: a burst of unreliable traffic does not count against the reliable stream.
    #[test]
    fn the_channels_have_separate_budgets() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let packets = MAX_PEER_MESSAGES_PER_RECEIVE as usize / 128 + 2;
        r.begin_receive();
        for i in 0..packets {
            r.forge(i as u16 + 1, &unreliable_records(128), 20);
        }
        r.forge(packets as u16 + 1, &reliable_records(0, 4), 20);
        assert!(r.server.conns.contains_key(&r.peer), "still connected");
        let got = events(&mut r.server);
        assert_eq!(got.len(), MAX_PEER_MESSAGES_PER_RECEIVE as usize + 4);
    }

    /// Review: forged `ConnectRequest`s in a victim's name cannot use up its joins.
    #[test]
    fn forged_requests_do_not_spend_a_real_clients_joins() {
        let net = LoopbackNet::new();
        let config = ServerConfig {
            handshakes_per_ip_per_s: 2,
            crypto: CryptoMode::Off,
            ..ServerConfig::default()
        };
        let mut server: NetServer<Mini, _> =
            NetServer::with_secret(net.bind(addr(1, 7000)).expect("bind"), config, [9; 32]);
        let victim = addr(2, 5000);
        let mut client = NetClient::<Mini, _>::connect(
            net.bind(victim).expect("bind"),
            addr(1, 7000),
            ClientConfig {
                crypto: CryptoMode::Off,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("fits");
        client.update(0);
        server.receive(0);
        server.flush(0);
        client.update(1);
        // The cookie is out. Now the bucket for this IP is spent by a spoofer.
        let request = ConnectRequest {
            protocol_id: crate::payload::protocol_id::<Mini>(),
            client_nonce: 99,
            crypto_version: CRYPTO_VERSION,
            crypto_required: false,
        }
        .to_datagram();
        for _ in 0..16 {
            server.handle_datagram(&request, addr(2, 6000));
        }
        server.receive(2);
        server.flush(2);
        client.update(3);
        assert!(client.is_connected(), "the join went through");
    }

    /// Audit L1 (muse, grok): one IP cannot take every slot by default.
    #[test]
    fn one_ip_holds_a_limited_number_of_slots_by_default() {
        assert_eq!(ServerConfig::default().max_peers_per_ip, 8);
        assert!(ServerConfig::default().max_peers_per_ip < ServerConfig::default().max_peers);
    }

    /// Audit L4 (grok): a scanner learns no game id from a protocol mismatch.
    #[test]
    fn a_protocol_mismatch_names_no_game() {
        let mut r = rig(ServerConfig::default());
        let request = ConnectRequest {
            protocol_id: 1,
            client_nonce: 7,
            crypto_version: CRYPTO_VERSION,
            crypto_required: false,
        }
        .to_datagram();
        let scanner = addr(6, 1000);
        let mut socket = r.net.bind(scanner).expect("bind");
        r.server.handle_datagram(&request, scanner);
        let mut buf = [0u8; 256];
        let (n, _) = socket.recv_from(&mut buf).expect("io").expect("deny");
        let (_, body) = crate::handshake::split_datagram(&buf[..n]).expect("regia traffic");
        let deny = Deny::decode(body).expect("deny");
        assert!(!deny.detail.contains(Mini::GAME_ID), "{}", deny.detail);
    }

    /// One reliable message split into forged fragments of at most `frag` bytes, ids from
    /// `first_id`, one record per returned packet body.
    fn reliable_fragments(first_id: u16, message: &[u8], frag: usize) -> Vec<Vec<u8>> {
        let chunks: Vec<&[u8]> = message.chunks(frag).collect();
        let last = chunks.len().saturating_sub(1);
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let mut w = WireWriter::new();
                w.u8(if i < last { 0x82 } else { 0x02 });
                w.u16(first_id.wrapping_add(i as u16));
                w.u16(chunk.len() as u16);
                w.raw(chunk);
                w.finish()
            })
            .collect()
    }

    /// `LANE_INPUT_RELIABLE | tick | tail`.
    fn reliable_input(tick: u64, tail: &[u8]) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.u8(LANE_INPUT_RELIABLE);
        w.u64(tick);
        w.raw(tail);
        w.finish()
    }

    /// ADR-7896: a reliable input, fragmented, lands in the tick driver on its tick.
    #[test]
    fn a_fragmented_reliable_input_lands_on_its_tick() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let message = reliable_input(2, &[42]);
        for (i, body) in reliable_fragments(0, &message, 4).iter().enumerate() {
            r.begin_receive();
            r.forge(i as u16 + 1, body, 20);
        }
        assert_eq!(r.server.totals().reliable_inputs, 1);
        assert_eq!(r.server.totals().malformed, 0);
        r.server.advance_tick(|_, _| {});
        r.server.advance_tick(|_, _| {});
        let inputs = r.server.advance_tick(|_, _| {});
        assert_eq!(inputs.get(&r.peer), Some(&42), "on tick 2");
    }

    /// ADR-7896: a reliable input longer than `MAX_RELIABLE_INPUT_BYTES` is refused after its
    /// fragments reassemble, before the input decoder runs, and counted against the peer.
    #[test]
    fn an_over_limit_reliable_input_stream_is_refused() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let message = reliable_input(0, &vec![7u8; crate::tick::MAX_RELIABLE_INPUT_BYTES + 1]);
        for (i, body) in reliable_fragments(0, &message, 1000).iter().enumerate() {
            r.begin_receive();
            r.forge(i as u16 + 1, body, 20);
        }
        assert_eq!(r.server.totals().reliable_inputs, 0, "not accepted");
        assert_eq!(r.server.totals().malformed, 1, "one hostile message");
        // Over the cap is a protocol violation (ADR-7892): the sender goes at once.
        let evs = events(&mut r.server);
        assert!(
            evs.iter().any(|e| matches!(e, ServerEvent::Rejected { peer, kind: RejectKind::InputTooLong, .. } if *peer == r.peer)),
            "{evs:?}"
        );
        assert!(left_with(&evs, DisconnectReason::ProtocolError), "{evs:?}");
        assert!(!r.server.conns.contains_key(&r.peer));
        assert!(!r.server.driver().all_ready_for(0), "no input for tick 0");
    }

    /// ADR-7896: reliable inputs that do not decode, and the reliable lane byte on the unreliable
    /// channel, are refused and counted against the peer.
    #[test]
    fn malformed_reliable_inputs_are_refused() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let truncated_tick = vec![LANE_INPUT_RELIABLE, 1, 2, 3];
        let no_input = reliable_input(0, &[]);
        let trailing = reliable_input(0, &[1, 2]);
        let mut seq = 1u16;
        for (id, message) in [truncated_tick, no_input, trailing].iter().enumerate() {
            r.begin_receive();
            r.forge(seq, &reliable_fragments(id as u16, message, 1000)[0], 20);
            seq += 1;
        }
        // On the unreliable channel the lane is refused whatever it holds.
        let valid = reliable_input(0, &[5]);
        let mut w = WireWriter::new();
        w.u8(0x01);
        w.u16(valid.len() as u16);
        w.raw(&valid);
        r.begin_receive();
        r.forge(seq, &w.finish(), 20);
        assert_eq!(r.server.totals().malformed, 4);
        assert_eq!(r.server.totals().reliable_inputs, 0);
        let kinds: Vec<RejectKind> = events(&mut r.server)
            .into_iter()
            .filter_map(|e| match e {
                ServerEvent::Rejected { kind, .. } => Some(kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                RejectKind::InputMalformed,
                RejectKind::InputMalformed,
                RejectKind::InputMalformed,
                RejectKind::UnknownLane
            ],
            "scored, not fatal"
        );
        assert!(r.server.conns.contains_key(&r.peer), "the peer stays");
        assert!(!r.server.driver().all_ready_for(0), "no input for tick 0");
    }

    /// Review row 20: a sealed join from another port of the handshake's IP moves the half-open row
    /// only after it opened under the row's keys, and the accept goes to the new port. A record
    /// with the right token and a broken tag moves nothing.
    #[test]
    fn a_pending_join_moves_to_a_new_port_only_after_it_verified() {
        let net = LoopbackNet::new();
        let server_addr = addr(1, 7000);
        let config = ServerConfig {
            crypto: CryptoMode::Required,
            ..ServerConfig::default()
        };
        let mut server: NetServer<Mini, _> =
            NetServer::with_secret(net.bind(server_addr).expect("bind"), config, [9; 32]);
        let (a, b, c) = (addr(2, 5000), addr(2, 5001), addr(2, 5002));
        let mut client = NetClient::<Mini, _>::connect(
            net.bind(a).expect("bind"),
            server_addr,
            ClientConfig {
                crypto: CryptoMode::Required,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("fits");
        for now in 0..10 {
            if !server.pending.is_empty() {
                break;
            }
            client.update(now);
            server.receive(now);
            server.flush(now);
        }
        assert!(server.pending.get(a).is_some(), "a half-open handshake at port A");
        // The client reads the key reply and seals its join; take it off the server's socket.
        client.update(10);
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        let mut join = None;
        while let Some((n, from)) = server.socket.recv_from(&mut buf).expect("io") {
            if from == a && split_datagram(&buf[..n]).is_some_and(|(k, _)| k == PacketKind::Sealed) {
                join = Some(buf[..n].to_vec());
            }
        }
        let join = join.expect("the client sealed its join");
        server.now_ms = 10;
        let mut forged = join.clone();
        if let Some(tag) = forged.last_mut() {
            *tag ^= 1;
        }
        let failed = server.totals().auth_failed;
        server.handle_datagram(&forged, c);
        assert_eq!(server.totals().auth_failed, failed + 1);
        assert!(server.pending.get(a).is_some(), "a forgery moves nothing");
        assert!(server.pending.get(c).is_none());
        assert_eq!(server.session().len(), 0);
        let mut at_b = net.bind(b).expect("bind B");
        server.handle_datagram(&join, b);
        assert!(server.pending.is_empty(), "the row moved to B and joined");
        assert_eq!(server.session().peers().next().map(|p| p.addr), Some(b));
        let (n, _) = at_b
            .recv_from(&mut buf)
            .expect("io")
            .expect("the accept went to port B");
        assert_eq!(
            split_datagram(&buf[..n]).map(|(k, _)| k),
            Some(PacketKind::Sealed)
        );
    }

    /// Review row 21: the admitted-nonce set matches another port of the IP only, forgets a row
    /// when its cookie window closes and never holds more than its cap.
    #[test]
    fn admitted_nonces_expire_and_stay_bounded() {
        let mut set = AdmittedNonces::default();
        let (a, b) = (addr(2, 5000), addr(2, 5001));
        set.insert(a, 7, 0);
        assert!(set.admitted_elsewhere(b, 7, 1), "another port of the IP");
        assert!(
            !set.admitted_elsewhere(a, 7, 1),
            "the same address keeps its resend path"
        );
        assert!(!set.admitted_elsewhere(b, 8, 1), "another nonce");
        assert!(!set.admitted_elsewhere(addr(3, 5000), 7, 1), "another IP");
        let mapped: SocketAddr = "[::ffff:10.0.0.2]:5001".parse().expect("mapped");
        assert!(set.admitted_elsewhere(mapped, 7, 1), "canonical IP");
        let window = 2 * crate::handshake::COOKIE_BUCKET_MS;
        assert!(set.admitted_elsewhere(b, 7, window - 1));
        assert!(!set.admitted_elsewhere(b, 7, window), "the cookie window closed");
        assert!(set.rows.is_empty());
        for nonce in 0..(MAX_ADMITTED_NONCES as u64 + 10) {
            set.insert(a, nonce, window);
        }
        assert_eq!(set.rows.len(), MAX_ADMITTED_NONCES);
        assert!(
            !set.admitted_elsewhere(b, 0, window),
            "the oldest row was evicted"
        );
        assert!(set.admitted_elsewhere(b, MAX_ADMITTED_NONCES as u64 + 9, window));
    }

    /// A game whose messages carry a `u32` length, so one can be a full 256 KiB reliable message.
    #[derive(Debug, Clone, Copy)]
    struct Big;

    #[derive(Debug, Clone, PartialEq)]
    struct BigBytes(Vec<u8>);

    impl NetCodec for BigBytes {
        fn encode(&self, w: &mut WireWriter) {
            w.u32(self.0.len() as u32);
            w.raw(&self.0);
        }
        fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
            let len = r.u32()? as usize;
            Ok(Self(r.take(len)?.to_vec()))
        }
    }

    impl NetGame for Big {
        const GAME_ID: &'static str = "server-tests-big";
        const PROTOCOL_VERSION: u32 = 1;
        type JoinData = ();
        type Input = u8;
        type Message = BigBytes;
    }

    /// Joins a plain client from `from`, stepping virtual time from `*now`.
    fn join_from<G: NetGame<JoinData = ()>>(
        server: &mut NetServer<G, LoopbackSocket>,
        net: &LoopbackNet,
        from: SocketAddr,
        conditioner: ConditionerConfig,
        now: &mut u64,
    ) -> (NetClient<G, LoopbackSocket>, PeerId, u32) {
        let mut client = NetClient::<G, _>::connect(
            net.bind(from).expect("bind"),
            addr(1, 7000),
            ClientConfig {
                crypto: CryptoMode::Off,
                conditioner,
                ..ClientConfig::default()
            },
            &(),
        )
        .expect("fits");
        // Over a lossy link the handshake may need its retries.
        for _ in 0..30_000 {
            *now += 1;
            client.update(*now);
            server.receive(*now);
            server.flush(*now);
            if client.is_connected()
                && let Some(peer) = server.session.peer_by_addr(from)
            {
                let token = server.conns[&peer].token;
                return (client, peer, token);
            }
        }
        panic!("{from} did not join");
    }

    /// A plain payload of `records` from `from` under `token`, packet number `seq`.
    fn forge_from<G: NetGame>(
        server: &mut NetServer<G, LoopbackSocket>,
        token: u32,
        from: SocketAddr,
        seq: u16,
        records: &[u8],
    ) {
        let mut w = WireWriter::new();
        w.raw(&connected_header(PacketKind::Payload, token));
        // Wire 5: flags (a 2-byte packet number, no acknowledgment frame), then a number past the
        // client's own handshake traffic.
        w.u8(0x01);
        w.u16(seq.wrapping_add(500));
        w.raw(records);
        let bytes = w.finish();
        server.handle_datagram(&bytes, from);
    }

    /// A hostile peer that sends the E6 bomb: reliable fragments `1..=ids` (never the next
    /// expected id 0) once a second, and a keepalive every 100 ms.
    struct Bomber<G: NetGame> {
        from: SocketAddr,
        peer: PeerId,
        token: u32,
        seq: u16,
        ids: u16,
        _client: NetClient<G, LoopbackSocket>,
    }

    impl<G: NetGame> Bomber<G> {
        fn step(&mut self, server: &mut NetServer<G, LoopbackSocket>, now: u64, frag: usize) {
            if now.is_multiple_of(1_000) {
                for id in 1..=self.ids {
                    let mut w = WireWriter::new();
                    w.u8(0x82);
                    w.u16(id);
                    w.u16(frag as u16);
                    w.raw(&vec![0xEE; frag]);
                    self.seq = self.seq.wrapping_add(1);
                    forge_from(server, self.token, self.from, self.seq, &w.finish());
                }
            } else if now.is_multiple_of(100) {
                self.seq = self.seq.wrapping_add(1);
                forge_from(server, self.token, self.from, self.seq, &[]);
            }
        }
    }

    /// Asserts the per-peer and server-wide caps hold right now; returns the bytes all peers hold.
    fn assert_caps<G: NetGame>(server: &NetServer<G, LoopbackSocket>) -> usize {
        let mut sum = 0;
        for (peer, pc) in &server.conns {
            let held = pc.conn.reliable_buffered_bytes();
            assert!(
                held <= pc.conn.reassembly_cap_bytes(),
                "{peer} holds {held} B over its cap {}",
                pc.conn.reassembly_cap_bytes()
            );
            assert_eq!(pc.conn.stats().reliable_buffered_bytes, held);
            sum += held;
        }
        let budget = server.reassembly_budget();
        assert_eq!(budget.used(), sum, "the budget tracks every peer");
        assert!(
            budget.used() <= budget.limit(),
            "{} B over the server cap {}",
            budget.used(),
            budget.limit()
        );
        sum
    }

    /// Spec 120 § 5.2 / audit H4 (ADR-7981): eight peers on one IP send the E6 bomb, resent every
    /// second, with keepalives. Each holds at most its cap, all eight hold at most the server's
    /// budget, every one is dropped named (`reassembly_stalled`, or `stream_over_cap` once one
    /// stream passes its window, ADR-8302), and meanwhile an honest peer's
    /// 256 KiB reliable message over 10 % loss and reorders both ways still arrives.
    #[test]
    fn eight_bombers_on_one_ip_are_capped_and_dropped_while_a_legit_transfer_arrives() {
        let bad_link = |seed| ConditionerConfig {
            enabled: true,
            seed,
            loss: 0.10,
            duplicate: 0.01,
            reorder: 0.10,
            reorder_max_ms: 30,
            delay_ms: 8,
            jitter_ms: 30,
            ..ConditionerConfig::default()
        };
        let net = LoopbackNet::new();
        let config = ServerConfig {
            crypto: CryptoMode::Off,
            conditioner: bad_link(3),
            ..ServerConfig::default()
        };
        let mut server: NetServer<Big, _> =
            NetServer::with_secret(net.bind(addr(1, 7000)).expect("bind"), config, [9; 32]);
        let mut now = 0;
        let mut bombers: Vec<Bomber<Big>> = (0..8_u16)
            .map(|i| {
                let from = addr(2, 5000 + i);
                let (client, peer, token) =
                    join_from(&mut server, &net, from, ConditionerConfig::default(), &mut now);
                Bomber {
                    from,
                    peer,
                    token,
                    seq: 500,
                    ids: 1023,
                    _client: client,
                }
            })
            .collect();
        let legit_addr = addr(3, 5000);
        let (mut legit, legit_peer, _) = join_from(&mut server, &net, legit_addr, bad_link(4), &mut now);
        let frag = server.conns[&legit_peer].conn.max_fragment_bytes();
        let message: Vec<u8> = (0..256 * 1024 - 8_u32).map(|i| (i * 31 % 251) as u8).collect();
        legit
            .send(Channel::GAME_RPC, &BigBytes(message.clone()))
            .expect("fits");
        let start = now.next_multiple_of(1_000);
        let mut arrived = false;
        let mut stalled = Vec::new();
        let mut peak = 0;
        for now in start..start + 60_000 {
            for b in &mut bombers {
                if server.conns.contains_key(&b.peer) {
                    b.step(&mut server, now, frag);
                }
            }
            legit.update(now);
            server.receive(now);
            server.flush(now);
            peak = peak.max(assert_caps(&server));
            for e in std::iter::from_fn(|| server.poll_event()).collect::<Vec<_>>() {
                match e {
                    ServerEvent::Message {
                        peer,
                        message: BigBytes(got),
                        ..
                    } if peer == legit_peer => {
                        assert_eq!(got, message, "the message arrives whole");
                        arrived = true;
                    }
                    // A bomb of 1,023 fragments on stream 2 passes that stream's 256 KiB window
                    // (ADR-8302) unless the server budget refuses it first; then the gap stalls.
                    ServerEvent::Rejected {
                        peer,
                        kind: RejectKind::ReassemblyStalled | RejectKind::StreamOverCap,
                        ..
                    } => stalled.push(peer),
                    ServerEvent::PeerLeft { peer, reason } => {
                        assert_ne!(peer, legit_peer, "the honest peer stays ({reason:?})");
                        assert_eq!(reason, DisconnectReason::ProtocolError);
                    }
                    _ => {}
                }
            }
            if arrived && bombers.iter().all(|b| !server.conns.contains_key(&b.peer)) {
                break;
            }
        }
        stalled.sort();
        let mut expected: Vec<PeerId> = bombers.iter().map(|b| b.peer).collect();
        expected.sort();
        assert_eq!(
            stalled, expected,
            "every bomber is dropped, named reassembly_stalled or stream_over_cap"
        );
        assert!(
            arrived,
            "the honest 256 KiB message arrives (connected {}, client link {:?}, client totals {:?}, server link {:?}, budget {:?}, peak {peak})",
            legit.is_connected(),
            legit.status().link,
            legit.totals(),
            server.conns.get(&legit_peer).map(|pc| pc.conn.stats()),
            server.reassembly_budget()
        );
        assert!(server.conns.contains_key(&legit_peer));
        assert!(peak > 0, "the bombers did store something");
        assert_eq!(
            server.reassembly_budget().used(),
            server.conns[&legit_peer].conn.reliable_buffered_bytes()
        );
    }

    /// Spec 120 § 5.2: the server-wide cap holds with 64 bombers (counted bytes, no memory timing):
    /// together they never hold more than `min(64 MiB, 32 KiB x max_peers)`, the refused packets
    /// are counted, and every bomber is dropped in the end.
    #[test]
    fn the_server_wide_cap_holds_against_64_bombers() {
        let net = LoopbackNet::new();
        let config = ServerConfig {
            crypto: CryptoMode::Off,
            max_peers: 64,
            max_peers_per_ip: 0,
            ..ServerConfig::default()
        };
        assert_eq!(config.reassembly_budget_bytes(), 64 * 32 * 1024);
        let mut server: NetServer<Mini, _> =
            NetServer::with_secret(net.bind(addr(1, 7000)).expect("bind"), config, [9; 32]);
        let mut now = 0;
        let mut bombers: Vec<Bomber<Mini>> = (0..64_u16)
            .map(|i| {
                let from = addr(2 + (i / 8) as u8, 5000 + i);
                let (client, peer, token) =
                    join_from(&mut server, &net, from, ConditionerConfig::default(), &mut now);
                Bomber {
                    from,
                    peer,
                    token,
                    seq: 500,
                    ids: 200,
                    _client: client,
                }
            })
            .collect();
        let frag = server.conns[&bombers[0].peer].conn.max_fragment_bytes();
        let start = now.next_multiple_of(1_000);
        let mut peak = 0;
        for now in (start..start + 120_000).step_by(100) {
            server.now_ms = now;
            for b in &mut bombers {
                if server.conns.contains_key(&b.peer) {
                    b.step(&mut server, now, frag);
                }
            }
            peak = peak.max(assert_caps(&server));
            let _ = events(&mut server);
            if server.conns.is_empty() {
                break;
            }
        }
        // 64 x 200 fragments asked for about 15 MB; the budget held about 2 MiB of it.
        assert!(
            peak > server.reassembly_budget().limit() / 2,
            "the bombers reached the cap ({peak} B)"
        );
        assert!(server.conns.is_empty(), "every bomber is dropped");
        assert_eq!(server.reassembly_budget().used(), 0, "and everything is released");
    }

    /// Spec 120 § 5.2: a peer whose fragment is longer than its link's packet carries, or whose
    /// out-of-order fragments pass its own cap, is dropped at once and named.
    #[test]
    fn an_over_long_fragment_or_an_over_cap_peer_is_dropped_named() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let frag = r.server.conns[&r.peer].conn.max_fragment_bytes();
        let mut w = WireWriter::new();
        w.u8(0x02);
        w.u16(0);
        w.u16(frag as u16 + 1);
        w.raw(&vec![1; frag + 1]);
        r.forge(1, &w.finish(), 20);
        let evs = events(&mut r.server);
        assert!(
            evs.iter().any(|e| matches!(e, ServerEvent::Rejected { peer, kind: RejectKind::FragmentTooLong, .. } if *peer == r.peer)),
            "{evs:?}"
        );
        assert!(left_with(&evs, DisconnectReason::ProtocolError), "{evs:?}");

        let mut r = rig(ServerConfig {
            link: LinkConfig {
                connection_buffer_bytes: 10_000,
                ..LinkConfig::default()
            },
            ..ServerConfig::default()
        });
        let _ = events(&mut r.server);
        assert_eq!(r.server.conns[&r.peer].conn.reassembly_cap_bytes(), 10_000);
        for id in 1..=9_u16 {
            let mut w = WireWriter::new();
            w.u8(0x82);
            w.u16(id);
            w.u16(1_000);
            w.raw(&[2; 1_000]);
            r.forge(id, &w.finish(), 20);
        }
        assert!(r.server.conns.contains_key(&r.peer), "9,000 B is within the cap");
        let mut w = WireWriter::new();
        w.u8(0x82);
        w.u16(10);
        w.u16(1_001);
        w.raw(&[2; 1_001]);
        r.forge(10, &w.finish(), 20);
        let evs = events(&mut r.server);
        assert!(
            evs.iter().any(|e| matches!(e, ServerEvent::Rejected { peer, kind: RejectKind::ReassemblyOverCap, .. } if *peer == r.peer)),
            "{evs:?}"
        );
        assert!(!r.server.conns.contains_key(&r.peer));
        assert_eq!(
            r.server.reassembly_budget().used(),
            0,
            "the dropped peer's bytes are released"
        );
    }

    /// A joined client on a sealed non-loopback link (`CryptoMode::Auto`).
    struct SealedPair {
        _net: LoopbackNet,
        server: NetServer<Mini, LoopbackSocket>,
        client: NetClient<Mini, LoopbackSocket>,
        peer: PeerId,
        client_addr: SocketAddr,
    }

    fn sealed_pair() -> SealedPair {
        let net = LoopbackNet::new();
        let server_addr = addr(1, 7100);
        let client_addr = addr(2, 5100);
        let mut server: NetServer<Mini, _> = NetServer::with_secret(
            net.bind(server_addr).expect("bind"),
            ServerConfig::default(),
            [9; 32],
        );
        let mut client = NetClient::<Mini, _>::connect(
            net.bind(client_addr).expect("bind"),
            server_addr,
            ClientConfig::default(),
            &(),
        )
        .expect("join fits");
        let mut connected = false;
        for now in 0..40 {
            client.update(now);
            server.receive(now);
            server.flush(now);
            if client.is_connected() && client.is_sealed() && server.conns.len() == 1 {
                connected = true;
                break;
            }
        }
        assert!(connected, "the sealed handshake did not finish");
        let (&peer, _) = server.conns.iter().next().expect("joined");
        assert!(server.peer_sealed(peer), "Auto seals a non-loopback peer");
        let _ = events(&mut server);
        SealedPair {
            _net: net,
            server,
            client,
            peer,
            client_addr,
        }
    }

    /// Connection body: a 2-byte packet number and one record tag the connection cannot name.
    fn unknown_record(pn: u16) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.u8(0x01);
        w.u16(pn);
        w.raw(&[0x7F, 0, 0]);
        w.finish()
    }

    fn deliver_at(pair: &mut SealedPair, datagram: &[u8], now: u64) {
        pair.server.now_ms = now;
        pair.server.handle_datagram(datagram, pair.client_addr);
    }

    /// ADR-8606: a sealed record whose tag verifies and whose kind byte is not a [`SealedKind`]
    /// authenticated, then failed decode. Eighty of them are 1,280 points and must remove the peer.
    /// On 358533475 `open` returns `CryptoError::Malformed` after committing the window, `note_open`
    /// drops the record, and `on_sealed` refunds it, so the score stays 0.
    #[test]
    fn authenticated_unknown_sealed_kind_scores_and_a_burst_drops_the_peer() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let now = pair.server.now_ms.saturating_add(1_000);
        let before = pair.server.totals().malformed;
        for _ in 0..80 {
            let datagram = pair.client.testing_seal_kind(0x00, &[]).expect("seal");
            deliver_at(&mut pair, &datagram, now);
        }
        let opened = pair.server.totals().malformed.saturating_sub(before);
        // ADR-8666: each record up to the removal is counted; the 64th reaches 1,024 points and
        // removes the peer, and the 16 after it come from an address that holds no peer (they fail
        // the token lookup, `auth_failed`). Before the fix all 80 were counted and none scored.
        assert_eq!(
            opened,
            u64::from(MALFORMED_SCORE_LIMIT / MALFORMED_WEIGHT),
            "each record verified its tag and then failed kind decode, until the peer went"
        );
        let evs = events(&mut pair.server);
        assert!(
            left_with(&evs, DisconnectReason::MalformedPacket),
            "80 authenticated unknown-kind records are 1,280 points and must drop the peer; points {:?} events {evs:?}",
            pair.server.peer_inbound(peer).map(|s| s.malformed_points)
        );
    }

    /// ADR-8606 residual (c): an authenticated `Accept` or `Deny` is a real malformation (a client
    /// never sends one). Eighty must drop the peer. On 358533475 `on_peer_record` only increments
    /// `totals.malformed`.
    #[test]
    fn authenticated_accept_records_score_and_a_burst_drops_the_peer() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let now = pair.server.now_ms.saturating_add(1_000);
        let before = pair.server.totals().malformed;
        for _ in 0..80 {
            let datagram = pair
                .client
                .testing_seal_kind(SealedKind::Accept as u8, &[])
                .expect("seal");
            deliver_at(&mut pair, &datagram, now);
        }
        let opened = pair.server.totals().malformed.saturating_sub(before);
        // As above: counted up to the removal at the 64th (ADR-8666).
        assert_eq!(
            opened,
            u64::from(MALFORMED_SCORE_LIMIT / MALFORMED_WEIGHT),
            "each Accept verified and was counted, until the peer went"
        );
        let evs = events(&mut pair.server);
        assert!(
            left_with(&evs, DisconnectReason::MalformedPacket),
            "80 authenticated Accept records are 1,280 points and must drop the peer; points {:?} events {evs:?}",
            pair.server.peer_inbound(peer).map(|s| s.malformed_points)
        );
    }

    /// v06-security-fixes-verification-c, next to ADR-8666's Accept/Deny fix: a connected peer's
    /// authenticated `Join` is answered with an `Accept` without its body being decoded. An honest
    /// client resends the same `JoinToken` (a lost accept), so a `Join` whose body is not a
    /// `JoinToken` is a real malformation the tag proves is the peer's (ADR-8606: authenticated,
    /// then failed decode). Eighty of them must score like any other and drop the peer. Fails while
    /// each one scores 0 and is answered with a fresh sealed `Accept`.
    #[test]
    fn authenticated_join_with_a_malformed_body_scores_and_a_burst_drops_the_peer() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let now = pair.server.now_ms.saturating_add(1_000);
        let sealed_before = pair.server.totals().sealed_out;
        for _ in 0..80 {
            let datagram = pair
                .client
                .testing_seal_kind(SealedKind::Join as u8, &[])
                .expect("seal");
            deliver_at(&mut pair, &datagram, now);
        }
        let answered = pair.server.totals().sealed_out.saturating_sub(sealed_before);
        let evs = events(&mut pair.server);
        assert!(
            left_with(&evs, DisconnectReason::MalformedPacket),
            "80 authenticated Join records with an empty body are 1,280 points and must drop the peer; \
             points {:?}, sealed records sent back {answered}, events {evs:?}",
            pair.server.peer_inbound(peer).map(|s| s.malformed_points)
        );
    }

    /// ADR-8734: a resent `Join` whose body is a `JoinToken` (a lost accept) is still answered,
    /// scores nothing and keeps the peer, but a burst of them in one receive gets one `Accept`,
    /// and the next receive answers again. A `Join` with bytes left over after its token scores.
    #[test]
    fn a_resent_join_is_answered_once_per_receive_and_trailing_bytes_score() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let mut now = pair.server.now_ms.saturating_add(1_000);
        let token = JoinToken {
            name: "p".into(),
            role: PeerRole::Player,
            build: String::new(),
            password_proof: [0; 32],
            join_data: Vec::new(),
            resume: None,
        };
        let mut w = WireWriter::new();
        token.encode(&mut w).expect("encode");
        let body = w.finish();
        for round in 0..2u64 {
            // A fresh receive: the flag of the setup's own receives is cleared.
            now += 10;
            pair.server.receive(now);
            let sealed_before = pair.server.totals().sealed_out;
            for _ in 0..10 {
                let datagram = pair
                    .client
                    .testing_seal_kind(SealedKind::Join as u8, &body)
                    .expect("seal");
                deliver_at(&mut pair, &datagram, now);
            }
            assert_eq!(
                pair.server.totals().sealed_out.saturating_sub(sealed_before),
                1,
                "round {round}: ten resent Joins in one receive get one Accept"
            );
        }
        assert_eq!(
            pair.server.peer_inbound(peer).map(|s| s.malformed_points),
            Some(0),
            "a decodable resend is not a malformation"
        );
        assert!(!left_with(
            &events(&mut pair.server),
            DisconnectReason::MalformedPacket
        ));

        // A fresh receive again, so a missing answer is the decode's refusal, not the flag.
        now += 10;
        pair.server.receive(now);
        let mut trailing = body.clone();
        trailing.push(0);
        let datagram = pair
            .client
            .testing_seal_kind(SealedKind::Join as u8, &trailing)
            .expect("seal");
        let sealed_before = pair.server.totals().sealed_out;
        deliver_at(&mut pair, &datagram, now);
        assert_eq!(
            pair.server.peer_inbound(peer).map(|s| s.malformed_points),
            Some(u64::from(MALFORMED_WEIGHT)),
            "a token with a trailing byte scores"
        );
        assert_eq!(
            pair.server.totals().sealed_out,
            sealed_before,
            "and is not answered"
        );
    }

    /// Spec 120 § 5.2: an authenticated payload the connection cannot parse is still the peer's
    /// traffic. A gap, then one such payload every 10 s, must not keep the peer and its buffered
    /// fragments once that traffic has stood for the 4 s ceiling (each gap counts at most one
    /// minimum RTO, so forty payloads). The malformed score leaks this drip (one weight every
    /// 10 s; the bar is one every 4 s), and `on_peer_record` refreshes `last_seen` before
    /// `deliver`, so neither the score nor the 20 s silence timeout removes the peer.
    #[test]
    fn an_unparseable_authenticated_drip_cannot_hold_a_reassembly_gap_past_the_ceiling() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let t0 = pair.server.now_ms;
        let mut gap = WireWriter::new();
        // Wire 5: flags (2-byte packet number, no ack), the number, one fragment ahead of id 0.
        gap.u8(1);
        gap.u16(10);
        gap.u8(0x82);
        gap.u16(1);
        gap.u16(300);
        gap.raw(&[1; 300]);
        let datagram = pair
            .client
            .testing_seal_kind(SealedKind::Payload as u8, &gap.finish())
            .expect("seal");
        deliver_at(&mut pair, &datagram, t0);
        let held = pair
            .server
            .conns
            .get(&peer)
            .map(|pc| pc.conn.reliable_buffered_bytes())
            .unwrap_or(0);
        assert!(held > 0, "the gap fragment is stored ({held} B)");
        // Unknown packet flags: `receive_inner` rejects them before `last_rx` and the stall clock.
        let bad = {
            let mut w = WireWriter::new();
            w.u8(0x80);
            w.finish()
        };
        // Forty gaps of 10 s. Counted at the 100 ms minimum RTO that is the stall cap before a
        // sample, that is 4 s of the peer's traffic: `max(4 x RTO, 2 s)` at the unanswered cap.
        for k in 1..=40u64 {
            let now = t0 + k * 10_000;
            let datagram = pair
                .client
                .testing_seal_kind(SealedKind::Payload as u8, &bad)
                .expect("seal");
            deliver_at(&mut pair, &datagram, now);
            pair.server.receive(now);
        }
        let still = pair.server.conns.get(&peer);
        let held_after = still.map(|pc| pc.conn.reliable_buffered_bytes()).unwrap_or(0);
        let stall_ms = still.map(|pc| pc.conn.stats().reassembly_stall_ms).unwrap_or(0);
        assert!(
            still.is_none(),
            "peer {peer} still holds {held_after} B after 400 s of authenticated unparseable \
             payloads (stall {stall_ms} ms); spec 120 § 5.2"
        );
    }

    /// Spec 120 § 5.2, next to ADR-8764's payload fix: an authenticated `Join` is the peer's
    /// traffic and refreshes liveness (`on_peer_record` touches before the kind match), but it
    /// never enters `Connection::receive`, so the stall clock stays at 0. A gap, then one `Join`
    /// every 10 s, must not keep the peer and its buffered fragments once that traffic has stood
    /// for the 4 s ceiling. `Join` is not scored, so the malformed leak cannot remove the peer
    /// either, and the touch keeps the 20 s silence timeout from firing. `Accept` and `Deny` are
    /// the same match arms (they score, and one weight every 10 s leaks away).
    #[test]
    fn an_authenticated_join_drip_cannot_hold_a_reassembly_gap_past_the_ceiling() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let t0 = pair.server.now_ms;
        let mut gap = WireWriter::new();
        gap.u8(1);
        gap.u16(10);
        gap.u8(0x82);
        gap.u16(1);
        gap.u16(300);
        gap.raw(&[1; 300]);
        let datagram = pair
            .client
            .testing_seal_kind(SealedKind::Payload as u8, &gap.finish())
            .expect("seal");
        deliver_at(&mut pair, &datagram, t0);
        let held = pair
            .server
            .conns
            .get(&peer)
            .map(|pc| pc.conn.reliable_buffered_bytes())
            .unwrap_or(0);
        assert!(held > 0, "the gap fragment is stored ({held} B)");
        for k in 1..=40u64 {
            let now = t0 + k * 10_000;
            let datagram = pair
                .client
                .testing_seal_kind(SealedKind::Join as u8, &[])
                .expect("seal");
            deliver_at(&mut pair, &datagram, now);
            pair.server.receive(now);
        }
        let still = pair.server.conns.get(&peer);
        let held_after = still.map(|pc| pc.conn.reliable_buffered_bytes()).unwrap_or(0);
        let stall_ms = still.map(|pc| pc.conn.stats().reassembly_stall_ms).unwrap_or(0);
        assert!(
            still.is_none(),
            "peer {peer} still holds {held_after} B after 400 s of authenticated Join records \
             (stall {stall_ms} ms); spec 120 § 5.2"
        );
    }

    /// ADR-8606: once `slice` datagrams at the peer's address and token have failed their open,
    /// a later genuine datagram in that same receive is still opened and scored. On 358533475
    /// `admit_packet` returns `Unattributed` and `over_rate` drops it unopened, so the score stays
    /// at the control packet's 16.
    #[test]
    fn a_genuine_datagram_after_failed_opens_in_the_same_receive_is_still_scored() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let now = pair.server.now_ms.saturating_add(1_000);
        let first = pair
            .client
            .testing_seal_kind(SealedKind::Payload as u8, &unknown_record(40_000))
            .expect("seal");
        deliver_at(&mut pair, &first, now);
        let control = pair.server.peer_inbound(peer).map(|s| s.malformed_points);
        assert_eq!(
            control,
            Some(u64::from(MALFORMED_WEIGHT)),
            "control: an authenticated unknown record tag scores {MALFORMED_WEIGHT}"
        );
        pair.server.peer_slice = 2;
        deliver_at(&mut pair, &first, now);
        deliver_at(&mut pair, &first, now);
        let second = pair
            .client
            .testing_seal_kind(SealedKind::Payload as u8, &unknown_record(40_001))
            .expect("seal");
        deliver_at(&mut pair, &second, now);
        let after = pair.server.peer_inbound(peer).map(|s| s.malformed_points);
        assert!(
            pair.server.conns.contains_key(&peer),
            "32 points must not remove the peer"
        );
        assert!(
            after.is_some_and(|p| p >= u64::from(MALFORMED_WEIGHT) * 2),
            "a genuine datagram after slice failed opens in the same receive must still score; points {after:?}"
        );
    }

    /// ADR-8666: an authenticated record of an unknown kind is the peer's from a rebind candidate
    /// address too (its tag is the proof, not the address): it scores 16, and the address does not
    /// become a candidate on it. Before ADR-8666 it was refused as a failed open and scored 0.
    #[test]
    fn an_authenticated_unknown_kind_from_a_new_address_scores_and_moves_nothing() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let now = pair.server.now_ms.saturating_add(1_000);
        let refused = pair.server.totals().rebinds_refused;
        let datagram = pair.client.testing_seal_kind(0xFF, &[]).expect("seal");
        pair.server.now_ms = now;
        pair.server.handle_datagram(&datagram, addr(2, 5200));
        assert_eq!(
            pair.server.peer_inbound(peer).map(|s| s.malformed_points),
            Some(u64::from(MALFORMED_WEIGHT))
        );
        assert_eq!(pair.server.totals().rebinds_refused, refused + 1);
        assert_eq!(
            pair.server.session.peer(peer).map(|p| p.addr),
            Some(pair.client_addr),
            "the peer stays on its validated address"
        );
    }

    /// ADR-8734 (the verification-c nit): the stale branch of a rebind, a never-seen record older
    /// than the peer's newest from a new address, scores an authenticated unknown kind and counts
    /// a refused rebind, as the fresh branch does.
    #[test]
    fn a_stale_unknown_kind_from_a_new_address_scores_and_counts_a_refused_rebind() {
        let mut pair = sealed_pair();
        let peer = pair.peer;
        let now = pair.server.now_ms.saturating_add(1_000);
        let old = pair.client.testing_seal_kind(0xFF, &[]).expect("seal");
        let newer = pair
            .client
            .testing_seal_kind(SealedKind::Payload as u8, &[])
            .expect("seal");
        deliver_at(&mut pair, &newer, now);
        let points = pair.server.peer_inbound(peer).map(|s| s.malformed_points);
        let refused = pair.server.totals().rebinds_refused;
        pair.server.handle_datagram(&old, addr(2, 5300));
        assert_eq!(
            pair.server.peer_inbound(peer).map(|s| s.malformed_points),
            points.map(|p| p + u64::from(MALFORMED_WEIGHT)),
            "the stale record is the peer's own and scores"
        );
        assert_eq!(pair.server.totals().rebinds_refused, refused + 1);
        assert_eq!(
            pair.server.session.peer(peer).map(|p| p.addr),
            Some(pair.client_addr)
        );
    }

    /// ADR-8606 residual (a): a plain packet from an address the peer has not validated must not
    /// fail the link or score the peer, including a reliable fragment longer than the link allows.
    /// On 358533475 `FragmentTooLong` is a `LinkFailure` and `deliver` dropped the peer; since
    /// ADR-10077 the packet is held, not applied, until its address validates.
    #[test]
    fn an_unvalidated_overlong_fragment_does_not_drop_or_score_the_peer() {
        let mut r = rig(ServerConfig::default());
        let _ = events(&mut r.server);
        let frag = r.server.conns[&r.peer].conn.max_fragment_bytes();
        let len = u16::try_from(frag + 1).expect("fragment fits a u16 length");
        let mut w = WireWriter::new();
        w.raw(&connected_header(PacketKind::Payload, r.token));
        w.u8(0x01);
        w.u16(50_000);
        w.u8(0x02);
        w.u16(1);
        w.u16(len);
        w.raw(&vec![1; frag + 1]);
        let from = addr(2, 6000);
        let refused = r.server.totals().rebinds_refused;
        r.server.now_ms = r.server.now_ms.max(20);
        r.server.handle_datagram(&w.finish(), from);
        assert_eq!(
            r.server.totals().rebinds_refused,
            refused,
            "the new port was admitted as a candidate"
        );
        assert!(
            r.server.conns.contains_key(&r.peer),
            "an unvalidated over-long fragment must not fail the peer's link"
        );
        assert_eq!(
            r.server.peer_inbound(r.peer).map(|s| s.malformed_points),
            Some(0),
            "an unvalidated packet must not score"
        );
    }
}
