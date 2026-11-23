// SPDX-License-Identifier: MPL-2.0
//! The client endpoint.
//!
//! [`NetClient`] runs the handshake (retrying requests until accepted, denied or timed out), then
//! carries game messages and redundant input bundles to the server and keeps an estimate of the
//! server's tick from the tick syncs the server sends. Like the server it is driven by
//! [`NetClient::receive`] / [`NetClient::flush`] with an explicit millisecond clock.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

use crate::capture::{CaptureError, CaptureRecord, CaptureTap, ReplayFeed};
use crate::conditioner::{ConditionerConfig, NetConditioner};
use crate::connection::{Channel, Connection, LinkConfig, SendError};
use crate::crypto::{
    self, CRYPTO_VERSION, ClientHandshake, CryptoError, CryptoMode, KeyInit, KeyReply, NOISE_PATTERN,
    PLAIN_VERSION, RESUME_INIT_BYTES, RESUME_INIT_UNANSWERED_SENDS, RekeyPolicy, ResumeCredential,
    ResumeFlight, ResumeHello, ResumeInit, ResumeReply, ResumeRetry, SEALED_RESERVED_HEADER_BYTES,
    SealedHeader, SealedKind, SecureLink, ServerKey,
};
use crate::feedback::{ClockFilter, FEEDBACK_WINDOW_MS, InputTiming, LeadController, LinkFeedback};
use crate::handshake::{
    Accept, CONNECTED_HEADER_BYTES, Challenge, ChallengeResponse, ConnectRequest, Deny, DenyReason,
    DisconnectReason, JoinToken, MAX_JOIN_DATA_BYTES, PacketKind, PeerRole, RESUME_TICKET_BYTES,
    connected_header, fresh_secret, split_datagram,
};
use crate::password::prove;
use crate::path::{CandidateLimiter, PathState};
use crate::payload::{NetCodec, NetGame, protocol_id};
use crate::quality::{GradeChange, InputTotals, LinkGrade, PeerQuality, QUALITY_SAMPLE_MS, QualityTracker};
use crate::server::{
    LANE_CLOCK, LANE_GAME, LANE_INPUT, LANE_INPUT_RELIABLE, LANE_LINK_FEEDBACK, LANE_TICK_SYNC,
    RELIABLE_INPUT_CHANNEL,
};
use crate::session::PeerId;
use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};
use crate::status::{CryptoStatus, EndpointReport, EndpointTotals, NetAgentState, NetStatus};
use crate::tick::{InputBundle, InputHistory, MAX_INPUT_BUNDLES_PER_SEND, encode_reliable_input};
use crate::version_probe::{
    MismatchSide, PROBE_RESEND_MS, PROBE_WAIT_MS, ProbeAnswer, ProbeRequest, VersionMismatchReport,
    VersionStamp, is_probe_answer,
};
use crate::wire::{WireError, WireReader, WireWriter};

/// Messages of one channel one `receive` delivers at most. Past it unreliable messages are dropped;
/// a reliable one ends the connection.
pub const MAX_MESSAGES_PER_RECEIVE: u32 = 8192;

/// Furthest a tick sync may lead the estimate, in seconds of ticks.
const MAX_TICK_SYNC_JUMP_S: u64 = 10;

/// Whether a sync of `tick` leads `estimate` by more than a real clock can.
fn sync_too_far(tick: u64, estimate: u64, tick_rate_hz: u32) -> bool {
    tick.saturating_sub(estimate) > u64::from(tick_rate_hz.max(1)) * MAX_TICK_SYNC_JUMP_S
}

/// The longest gap between two polls of a connecting client that counts fully against its connect
/// timeout; a longer stall counts only this much (`credit_connect_stall`).
const CONNECT_STALL_MS: u64 = 1_000;

/// How long a due clock stamp waits for a datagram the client sends anyway (a tick's input or
/// acknowledgment) before it leaves on its own, milliseconds (ADR-10200): one quality sample.
const CLOCK_RIDE_WAIT_MS: u64 = QUALITY_SAMPLE_MS;

/// Age at which the server may have forgotten a handshake attempt, in milliseconds (ADR-4929):
/// a cookie stays valid for at least one `COOKIE_BUCKET_MS` after it was minted, a half-open key
/// exchange for `PENDING_TTL_MS`. Two seconds below both leave room for the next packet to arrive.
pub const HANDSHAKE_REFRESH_MS: u64 = 8_000;

/// Times one connect starts its handshake over with a fresh nonce because the server may have
/// forgotten the attempt (ADR-4929). Past it the client keeps resending until the timeout.
pub const MAX_HANDSHAKE_RESTARTS: u32 = 2;

const _: () = assert!(
    HANDSHAKE_REFRESH_MS < crate::handshake::COOKIE_BUCKET_MS
        && HANDSHAKE_REFRESH_MS < crypto::PENDING_TTL_MS
);

/// Client settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    /// Display name sent in the join token.
    pub name: String,
    /// Player or spectator.
    pub role: PeerRole,
    /// This build's id (the server may require a match).
    pub build: String,
    /// Password for a private server.
    pub password: Option<String>,
    /// Channel tuning.
    pub link: LinkConfig,
    /// Outbound network conditioner (off by default).
    pub conditioner: ConditionerConfig,
    /// Most unacknowledged inputs repeated in every input packet (1..=32). An input the server
    /// acknowledged is not repeated again, and a measured loss that needs more copies for 0.1 %
    /// residual loss raises it (spec 120 § 6.1, ADR-8312). Default 32: every unacknowledged input
    /// while one bundle holds it.
    pub input_redundancy: usize,
    /// Give up connecting after this long, in milliseconds.
    pub connect_timeout_ms: u64,
    /// Resend handshake packets this often, in milliseconds.
    pub handshake_resend_ms: u64,
    /// Silence from the server after which the client gives up, in milliseconds.
    pub timeout_ms: u64,
    /// Silence before interruption, milliseconds.
    pub interrupt_ms: u64,
    /// Admission grace before the first game datagram, milliseconds.
    pub start_grace_ms: u64,
    /// When the link is encrypted. The default requires it for a non-loopback server (ADR-1901).
    pub crypto: CryptoMode,
    /// The server's public key. When set, the link must be sealed under exactly this key, on
    /// loopback too; without it the client takes any key, which stops a listener but not an
    /// attacker in the middle.
    pub server_key: Option<ServerKey>,
    /// When this client moves its sending key on (ADR-2416). The server's records are followed
    /// whatever this says.
    pub rekey: RekeyPolicy,
    /// The resume ticket of an earlier connection to this server ([`NetClient::resume_ticket`]),
    /// presented in the join so the server binds this connection to the old `PeerId` (spec 120
    /// § 10.3). `None` joins as a new peer.
    pub resume: Option<[u8; RESUME_TICKET_BYTES]>,
    /// The resumption credential of an earlier sealed connection to this server
    /// ([`NetClient::resumption`]): the connect starts with a one-round-trip `ResumeInit` instead
    /// of a `ConnectRequest` (wire 6, ADR-8858). Set [`ClientConfig::resume`] from the same
    /// connection too: a server that refuses the resume answers a challenge, and the full
    /// handshake that follows presents that ticket. Holds a secret, so it is never serialized.
    #[serde(skip)]
    pub resumption: Option<ResumeCredential>,
}

impl ClientConfig {
    /// Sets the common silence policy without changing handshake deadlines.
    pub fn with_timeouts(mut self, policy: crate::session::TimeoutPolicy) -> Self {
        self.timeout_ms = policy.timeout_ms;
        self.interrupt_ms = policy.interrupt_ms;
        self.start_grace_ms = policy.start_grace_ms;
        self
    }
    /// The effective policy for this endpoint, with every value clamped into range
    /// ([`crate::session::TimeoutPolicy::sanitized`]).
    pub fn timeouts(&self) -> crate::session::TimeoutPolicy {
        crate::session::TimeoutPolicy {
            timeout_ms: self.timeout_ms,
            interrupt_ms: self.interrupt_ms,
            start_grace_ms: self.start_grace_ms,
        }
        .sanitized()
    }
}

impl Default for ClientConfig {
    fn default() -> Self {
        let timeout = crate::session::TimeoutPolicy::default();
        Self {
            name: "player".into(),
            role: PeerRole::Player,
            build: String::new(),
            password: None,
            // The upstream essential floor is 16 KiB/s (spec 120 § 4.2 item 4).
            link: LinkConfig {
                rate: crate::cc::RateConfig::client(),
                ..LinkConfig::default()
            },
            conditioner: ConditionerConfig::default(),
            input_redundancy: crate::tick::MAX_BUNDLE_INPUTS,
            connect_timeout_ms: 5_000,
            handshake_resend_ms: 250,
            timeout_ms: timeout.timeout_ms,
            interrupt_ms: timeout.interrupt_ms,
            start_grace_ms: timeout.start_grace_ms,
            crypto: CryptoMode::default(),
            server_key: None,
            rekey: RekeyPolicy::default(),
            resume: None,
            resumption: None,
        }
    }
}

/// Why a client stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientEnd {
    /// The server refused the join.
    Denied {
        /// Why.
        reason: DenyReason,
        /// Server's detail text.
        detail: String,
    },
    /// The connection ended.
    Disconnected(DisconnectReason),
    /// No answer within the connect timeout.
    ConnectTimeout,
    /// The game closed the client.
    Closed,
    /// The key exchange could not start, or the sealed link could not be opened. A plain
    /// challenge is not this end: it is unauthenticated, so a client that requires a seal counts
    /// it and keeps waiting.
    EncryptionUnavailable,
    /// The server proved a key that is not the pinned one. No secret was sent.
    ServerKeyMismatch,
}

impl ClientEnd {
    /// Lowercase code for logs and the agent protocol.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Denied { reason, detail }
                if *reason == DenyReason::InvalidJoin && detail == crate::ticket::TicketRefused::CODE =>
            {
                crate::ticket::TicketRefused::CODE
            }
            Self::Denied { reason, .. } => reason.as_str(),
            Self::Disconnected(reason) => reason.as_str(),
            Self::ConnectTimeout => "connect_timeout",
            Self::Closed => "closed",
            Self::EncryptionUnavailable => "encryption_unavailable",
            Self::ServerKeyMismatch => "server_key_mismatch",
        }
    }
}

/// Connection state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientState {
    /// Handshaking.
    Connecting,
    /// Accepted.
    Connected {
        /// Our peer id.
        peer: PeerId,
    },
    /// Stopped; the client will not reconnect by itself.
    Ended(ClientEnd),
}

/// Something that happened on the client.
#[derive(Debug, Clone)]
pub enum ClientEvent<G: NetGame> {
    /// The server has been silent for the interruption interval; the connection is retained.
    PeerInterrupted,
    /// Authentic traffic returned after an interruption.
    PeerRestored,
    /// The server accepted us.
    Connected {
        /// Our peer id.
        peer: PeerId,
        /// The server's tick rate.
        tick_rate_hz: u32,
    },
    /// The client stopped (denied, disconnected, timed out).
    Ended(ClientEnd),
    /// A game message from the server.
    Message {
        /// On which channel.
        channel: Channel,
        /// The message.
        message: G::Message,
    },
    /// The server refused the join with `protocol_mismatch`, and this says which side is older
    /// (spec 120 § 12.2, ADR-7985). It follows the [`ClientEvent::Ended`] of the refusal: at once
    /// when the deny carried the server's version stamp, otherwise after the version probe was
    /// answered or [`crate::version_probe::PROBE_WAIT_MS`] passed without an answer (a server
    /// that predates the probe, so older than this build).
    VersionMismatch {
        /// The server's game version text (its wire and protocol versions when it set none);
        /// `None` when the server predates the probe.
        server_game_version: Option<String>,
        /// This build's game version text (the same fallback).
        ours: String,
        /// `Some(true)`: the server is older; `Some(false)`: this client is (update the game);
        /// `None`: the versions do not say (same versions with other content, another game).
        server_is_older: Option<bool>,
        /// Which side is older, as a code (`client_older`, `server_older`, `content_differs`,
        /// `unordered`).
        side: MismatchSide,
        /// The line to show the player.
        message: String,
    },
    /// The server reports this client's inputs arriving after their ticks (the p5 arrival margin
    /// went below zero; spec 120 § 6.4, ADR-8077). The input lead has already been raised; the
    /// event fires once per late spell and is for diagnostics and HUDs.
    InputsLate {
        /// The reported 5th percentile arrival margin, ticks (negative).
        margin_p5: i32,
    },
    /// The connection grade of the link to the server changed (spec 120 § 13.1, ADR-8182): with
    /// hysteresis, and `Interrupted` once per outage. [`NetClient::quality`] has the signals
    /// behind it, including the upstream loss and input lateness the server reported.
    QualityChanged {
        /// The grade shown before.
        from: LinkGrade,
        /// The grade shown now.
        to: LinkGrade,
    },
}

/// A version probe this client sends after a stamp-less `protocol_mismatch` deny.
#[derive(Debug, Clone, Copy)]
struct ProbeWait {
    nonce: u64,
    started_ms: u64,
    last_sent_ms: Option<u64>,
}

/// One resume flight of a connect attempt (ADR-8858).
struct ResumeAttempt {
    flight: ResumeFlight,
    credential: ResumeCredential,
    /// The cookie of the server's `ResumeRetry`, sent back with the same flight.
    cookie: Option<[u8; 16]>,
    /// `ResumeInit` datagrams sent so far.
    sends: u32,
    /// The server answered a `ResumeRetry`: it reads wire 6, so no `ConnectRequest` is needed
    /// beside the flight.
    answered: bool,
    /// The server answered a `Challenge`: the resume is refused (or raced), the full handshake
    /// runs, and the flight is only kept for a `ResumeReply` that still opens.
    refused: bool,
}

/// Where the resume of a connect attempt stands.
enum ResumeStage {
    /// Not started: the first handshake datagram decides.
    Idle,
    /// A flight is out.
    Flying(Box<ResumeAttempt>),
    /// No resume (no credential, a plain link, a path too small), or it is over.
    Off,
}

/// How far the key exchange got.
enum ClientKeys {
    /// No exchange: plain, or the challenge has not arrived.
    Plain,
    /// `KeyInit` is on its way.
    Exchanging(Box<ClientHandshake>),
    /// The link is sealed.
    Sealed(Box<SecureLink>),
}

/// The client. See the module docs.
pub struct NetClient<G: NetGame, S: DatagramSocket> {
    socket: S,
    server: SocketAddr,
    config: ClientConfig,
    join_data: Vec<u8>,
    client_nonce: u64,
    started_ms: Option<u64>,
    /// The last `receive` while connecting, for the stall credit (`CONNECT_STALL_MS`).
    last_poll_ms: Option<u64>,
    /// When this attempt sent its first connect request: no cookie of it is older.
    attempt_sent_ms: Option<u64>,
    /// When this attempt sent its first key init: the server's half-open exchange is not older.
    key_init_sent_ms: Option<u64>,
    /// Fresh attempts this connect started (at most `MAX_HANDSHAKE_RESTARTS`).
    restarts: u32,
    last_handshake_ms: Option<u64>,
    /// The `receive` before the current one: a reply read now was not in the socket then.
    prev_poll_ms: Option<u64>,
    /// When the handshake flight now going out was first sent (ADR-10096): the earliest send the
    /// next answer that moves the handshake on can be an answer to. A replaced challenge or
    /// cookie does not move it.
    flight_sent_ms: Option<u64>,
    /// The shortest handshake round trip of this connect, by its lower bound: from a flight's
    /// first send to the last poll that did not yet hold its answer (ADR-10096).
    handshake_rtt_ms: Option<u64>,
    /// The one-way delay the accept anchored the clock with, until the connection has a round
    /// trip of its own (ADR-10096).
    handshake_one_way_ms: f64,
    challenge: Option<Challenge>,
    /// A key exchange whose `KeyInit` had left was dropped for a changed challenge during this
    /// attempt: the server may hold a share this client no longer has.
    challenge_replaced: bool,
    /// The first key exchange of this attempt whose `KeyInit` left, kept when a later, different
    /// challenge replaced it: the server may have stored that share, and its `KeyReply` must
    /// still finish the handshake (ADR-7995, review row 18).
    first_exchange: Option<(Challenge, Box<ClientHandshake>)>,
    /// A refresh-induced restart must not buy another timeout on its first challenge.
    preserve_challenge_deadline: bool,
    state: ClientState,
    token: u32,
    /// The resume ticket of the server's accept (all zeros: none).
    ticket: [u8; RESUME_TICKET_BYTES],
    /// The resumption credential of the server's accept (a sealed link's only).
    resumption: Option<ResumeCredential>,
    /// The one-round-trip resume of this attempt.
    resume: ResumeStage,
    keys: ClientKeys,
    server_key: Option<ServerKey>,
    unreliable_this_receive: u32,
    reliable_this_receive: u32,
    conn: Option<Connection>,
    tick_rate_hz: u32,
    /// The server-tick estimate (spec 120 § 9.1, ADR-8077).
    clock: Option<ClockFilter>,
    /// The input lead steered from the server's feedback (§ 9.2).
    lead: LeadController,
    /// The server's last input feedback (§ 6.4).
    feedback: Option<LinkFeedback>,
    /// Newest tick [`NetClient::due_input_ticks`] handed out.
    last_due_tick: Option<u64>,
    /// [`ClientEvent::InputsLate`] events so far.
    late_spells: u64,
    /// The loss-aware copy count of the last input send (ADR-8312).
    input_copies: u8,
    /// The quality of the link to the server (ADR-8182).
    quality: QualityTracker,
    /// When the next clock stamp is due, and the queued one not packed yet.
    next_clock_ms: Option<u64>,
    pending_clock: Option<u64>,
    /// The server stamped a tick sync (ADR-8889): only then does this client send its own clock
    /// stamps, so a server without the clock lane never sees one.
    server_stamps: bool,
    server_input_buffer_ticks: usize,
    last_heard_ms: u64,
    joined_ms: u64,
    started: bool,
    interrupted: bool,
    inputs: InputHistory<G::Input>,
    conditioner: NetConditioner,
    events: VecDeque<ClientEvent<G>>,
    totals: EndpointTotals,
    now_ms: u64,
    /// Candidate server addresses waiting for path validation (ADR-8299, adversarial row 13).
    path: PathState,
    /// Trial opens per candidate server address per second.
    path_limiter: CandidateLimiter,
    /// The run-time content hash in this client's stamp ([`NetClient::set_content_hash`]).
    runtime_content: u64,
    /// The versions to announce instead of the game's ([`NetClient::set_version_stamp`]).
    stamp_override: Option<VersionStamp>,
    /// The version probe waiting for an answer.
    probe_wait: Option<ProbeWait>,
    /// The last `protocol_mismatch` refusal, resolved.
    version_mismatch: Option<VersionMismatchReport>,
    /// Client capture (spec 120 § 15.5, ADR-8221): every receive, decrypted packet, input and
    /// flush goes here.
    capture: Option<CaptureTap>,
    /// Replay: the captured session this client reads instead of its socket.
    replay: Option<ReplayFeed>,
    /// Replay: the captured link was sealed (sets the packet header size).
    replay_sealed: bool,
    _game: PhantomData<fn() -> G>,
}

impl<G: NetGame, S: DatagramSocket> std::fmt::Debug for NetClient<G, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetClient")
            .field("game", &G::GAME_ID)
            .field("server", &self.server)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl<G: NetGame, S: DatagramSocket> NetClient<G, S> {
    /// Starts connecting to `server` with `join` as the game's join data. Fails when the join
    /// data or a token field is too large for the handshake.
    pub fn connect(
        socket: S,
        server: SocketAddr,
        config: ClientConfig,
        join: &G::JoinData,
    ) -> Result<Self, WireError> {
        let join_data = join.to_bytes()?;
        if join_data.len() > MAX_JOIN_DATA_BYTES {
            return Err(WireError::TooLong {
                len: join_data.len(),
                max: MAX_JOIN_DATA_BYTES,
            });
        }
        // Validate the token fields now so a bad name fails here, not silently mid-handshake.
        ChallengeResponse {
            client_nonce: 0,
            cookie: [0; 16],
            token: JoinToken {
                name: config.name.clone(),
                role: config.role,
                build: config.build.clone(),
                password_proof: [0; 32],
                join_data: join_data.clone(),
                resume: config.resume,
            },
        }
        .to_datagram()?;
        Ok(Self::unconnected(
            socket,
            server,
            config,
            join_data,
            fresh_nonce(),
        ))
    }

    /// A client before its first receive: nothing sent, nothing heard.
    fn unconnected(
        socket: S,
        server: SocketAddr,
        config: ClientConfig,
        join_data: Vec<u8>,
        client_nonce: u64,
    ) -> Self {
        Self {
            socket,
            server,
            inputs: InputHistory::new(config.input_redundancy),
            conditioner: NetConditioner::new(config.conditioner.clone()),
            config,
            join_data,
            client_nonce,
            started_ms: None,
            last_poll_ms: None,
            attempt_sent_ms: None,
            key_init_sent_ms: None,
            restarts: 0,
            last_handshake_ms: None,
            prev_poll_ms: None,
            flight_sent_ms: None,
            handshake_rtt_ms: None,
            handshake_one_way_ms: 0.0,
            challenge: None,
            challenge_replaced: false,
            first_exchange: None,
            preserve_challenge_deadline: false,
            state: ClientState::Connecting,
            token: 0,
            ticket: [0; RESUME_TICKET_BYTES],
            resumption: None,
            resume: ResumeStage::Idle,
            keys: ClientKeys::Plain,
            server_key: None,
            unreliable_this_receive: 0,
            reliable_this_receive: 0,
            conn: None,
            tick_rate_hz: 0,
            clock: None,
            lead: LeadController::default(),
            feedback: None,
            last_due_tick: None,
            late_spells: 0,
            input_copies: 0,
            quality: QualityTracker::default(),
            next_clock_ms: None,
            pending_clock: None,
            server_stamps: false,
            server_input_buffer_ticks: 64,
            last_heard_ms: 0,
            joined_ms: 0,
            started: false,
            interrupted: false,
            events: VecDeque::new(),
            totals: EndpointTotals::default(),
            now_ms: 0,
            path: PathState::default(),
            path_limiter: CandidateLimiter::new(crate::server::REBIND_ATTEMPTS_PER_S, fresh_secret()),
            runtime_content: 0,
            stamp_override: None,
            probe_wait: None,
            version_mismatch: None,
            capture: None,
            replay: None,
            replay_sealed: false,
            _game: PhantomData,
        }
    }

    /// Declares a content hash known only at run time in this client's version stamp (ADR-7985);
    /// a server that requires one ([`crate::server::NetServer::set_content_hash`]) refuses another.
    /// Call it before the first [`NetClient::flush`] sends the connect request.
    pub fn set_content_hash(&mut self, content_hash: u64) {
        self.runtime_content = content_hash;
    }

    /// Replaces the versions this client announces in its connect request (a session client
    /// announces its manifest's game version). The protocol id is not changed. Call it before the
    /// first [`NetClient::flush`].
    pub fn set_version_stamp(&mut self, stamp: VersionStamp) {
        self.stamp_override = Some(stamp);
    }

    /// The versions this client sends in its connect request (spec 120 § 12.2).
    pub fn version_stamp(&self) -> VersionStamp {
        self.stamp_override
            .clone()
            .unwrap_or_else(|| VersionStamp::of_with::<G>(self.runtime_content))
    }

    /// The last `protocol_mismatch` refusal and which side is older, once known.
    pub fn version_mismatch(&self) -> Option<&VersionMismatchReport> {
        self.version_mismatch.as_ref()
    }

    /// True while a version probe waits for the server's answer.
    pub fn probe_pending(&self) -> bool {
        self.probe_wait.is_some()
    }

    /// A client that replays a captured session (spec 120 § 15.5, ADR-8221;
    /// [`crate::capture`]): each [`NetClient::receive`] takes the records of the next captured
    /// receive instead of reading `socket`, so the same decrypted packets reach the connection at
    /// the same milliseconds. It sends nothing (the datagrams are counted and dropped), runs no
    /// handshake (the captured accept connects it) and no connect timeout (a captured end while
    /// connecting ends it). Call it with the times the capture's receive and flush records give,
    /// and attach a [`CaptureTap`] to compare its records with [`crate::capture::compare_sessions`].
    ///
    /// # Errors
    ///
    /// `capture_protocol_mismatch` when the session was captured by another game or protocol.
    pub fn replay(socket: S, config: ClientConfig, feed: ReplayFeed) -> Result<Self, CaptureError> {
        let mine = (G::GAME_ID, protocol_id::<G>());
        match feed.protocol() {
            Some(theirs) if theirs == mine => {}
            other => {
                return Err(CaptureError::ProtocolMismatch {
                    captured: other.map_or_else(|| "nothing".to_owned(), |(g, p)| format!("{g} {p:#018x}")),
                    replaying: format!("{} {:#018x}", mine.0, mine.1),
                });
            }
        }
        let mut c = Self::unconnected(socket, SocketAddr::from(([0, 0, 0, 0], 0)), config, Vec::new(), 0);
        let mut feed = feed;
        feed.records.pop_front(); // The session record.
        c.replay = Some(feed);
        Ok(c)
    }

    /// Attaches a client capture (ADR-8221), or detaches it with `None`. Attaching writes a
    /// session record: attach it before the first [`NetClient::receive`] to capture the whole
    /// connection.
    pub fn set_capture(&mut self, tap: Option<CaptureTap>) {
        if let Some(t) = &tap {
            t.push(CaptureRecord::Session {
                game: G::GAME_ID.to_owned(),
                protocol: protocol_id::<G>(),
            });
        }
        self.capture = tap;
    }

    /// The attached capture.
    pub fn capture(&self) -> Option<&CaptureTap> {
        self.capture.as_ref()
    }

    /// The replay feed of a client made by [`NetClient::replay`].
    pub fn replay_feed(&self) -> Option<&ReplayFeed> {
        self.replay.as_ref()
    }

    fn record(&self, r: impl FnOnce() -> CaptureRecord) {
        if let Some(t) = &self.capture {
            t.push(r());
        }
    }

    /// Current state.
    pub fn state(&self) -> &ClientState {
        &self.state
    }

    /// Our peer id once connected.
    pub fn peer_id(&self) -> Option<PeerId> {
        match self.state {
            ClientState::Connected { peer } => Some(peer),
            _ => None,
        }
    }

    /// True while connected.
    pub fn is_connected(&self) -> bool {
        matches!(self.state, ClientState::Connected { .. })
    }

    /// The server's tick rate (0 before the accept).
    pub fn tick_rate_hz(&self) -> u32 {
        self.tick_rate_hz
    }

    /// True when the link is sealed.
    pub fn is_sealed(&self) -> bool {
        matches!(self.keys, ClientKeys::Sealed(_))
    }

    /// The Noise handshake hash of the sealed link (channel binding: the server's side of this
    /// link holds the same bytes); `None` before the link is sealed or on a plain link.
    pub fn handshake_hash(&self) -> Option<[u8; 32]> {
        match &self.keys {
            ClientKeys::Sealed(link) => Some(*link.handshake_hash()),
            _ => None,
        }
    }

    /// The key the server proved, once it did.
    pub fn server_key(&self) -> Option<ServerKey> {
        self.server_key
    }

    /// Replaces the request nonce this client drew, before its first update sends the
    /// `ConnectRequest`. For deterministic tests and fuzz targets only (ADR-3283): a real client
    /// keeps its random nonce, so a reply recorded from another attempt is dropped at the nonce
    /// check.
    pub fn with_request_nonce(mut self, nonce: u64) -> Self {
        self.client_nonce = nonce;
        self
    }

    /// True when this client refuses a plain link.
    fn crypto_required(&self) -> bool {
        self.config.server_key.is_some() || self.config.crypto.requires(self.server)
    }

    fn header_bytes(&self) -> usize {
        if self.is_sealed() || self.replay_sealed {
            SEALED_RESERVED_HEADER_BYTES
        } else {
            CONNECTED_HEADER_BYTES
        }
    }

    /// Next queued event.
    pub fn poll_event(&mut self) -> Option<ClientEvent<G>> {
        self.events.pop_front()
    }

    /// The resume ticket the server's accept carried (spec 120 § 10.3): pass it as
    /// [`ClientConfig::resume`] on the next connection to come back as the same peer. `None`
    /// before the accept, or when the server keeps no dormant peers.
    pub fn resume_ticket(&self) -> Option<[u8; RESUME_TICKET_BYTES]> {
        (self.ticket != [0; RESUME_TICKET_BYTES]).then_some(self.ticket)
    }

    /// The resumption credential of the server's accept (wire 6, ADR-8858): pass it as
    /// [`ClientConfig::resumption`] on the next connection, with [`NetClient::resume_ticket`] as
    /// [`ClientConfig::resume`], to come back as the same peer in one round trip. `None` before
    /// the accept, on a plain link, or when the server keeps no dormant peers.
    pub fn resumption(&self) -> Option<ResumeCredential> {
        self.resumption.clone()
    }

    /// True while connected and the server has been silent past the interruption interval
    /// (between [`ClientEvent::PeerInterrupted`] and [`ClientEvent::PeerRestored`]).
    pub fn is_interrupted(&self) -> bool {
        self.is_connected() && self.interrupted
    }

    /// Gives the socket back (the connection state is dropped, no goodbye is sent): a reconnect
    /// over a caller-owned socket (a Steam socket, a QUIC endpoint) starts its next handshake on
    /// the same transport (spec 120 § 10.3, ADR-8352).
    pub fn into_socket(self) -> S {
        self.socket
    }

    /// Endpoint counters.
    pub fn totals(&self) -> EndpointTotals {
        self.totals
    }

    /// Replaces the outbound conditioner configuration.
    pub fn set_conditioner(&mut self, config: ConditionerConfig) {
        self.conditioner.set_config(config);
    }

    /// The conditioner.
    pub fn conditioner(&self) -> &NetConditioner {
        &self.conditioner
    }

    /// Estimated server tick at `now_ms`: the tick the server will be on when a packet sent now
    /// arrives, from an NTP-style filter over the last eight tick syncs (spec 120 § 9.1,
    /// ADR-8077): the least delayed sync sets the offset, plus half the round-trip time bounded to
    /// the server input horizon with room for two lead ticks. The estimate never moves backward:
    /// it slews by at most 5 % toward a new offset and steps forward only over 8 ticks
    /// ([`NetClient::clock_resyncs`]). Stamp inputs [`NetClient::input_lead_ticks`] ahead of it,
    /// or use [`NetClient::due_input_ticks`].
    ///
    /// On a symmetric path this is the tick the server is on now: the way to the server is the
    /// lead's to cover, and the server's feedback sets it. Right after the accept, before the
    /// connection has a round-trip sample of its own, the round trip is the handshake's
    /// (ADR-10096), so the estimate is on the server's tick from the first frame on any path.
    pub fn estimated_server_tick(&self, now_ms: u64) -> u64 {
        self.clock.as_ref().map_or(0, |c| c.tick(now_ms))
    }

    /// Times the tick estimate stepped forward over 8 ticks instead of slewing (a resync, spec
    /// 120 § 9.1). The first eight syncs after connecting anchor the clock and are not counted.
    pub fn clock_resyncs(&self) -> u64 {
        self.clock.as_ref().map_or(0, ClockFilter::resyncs)
    }

    /// Ticks ahead of [`NetClient::estimated_server_tick`] to stamp inputs (spec 120 § 9.2,
    /// ADR-8077). Steered from the server's [`LinkFeedback`]: it aims for a 5th percentile
    /// arrival margin of at least one tick, rises at once by the deficit, falls by one tick
    /// after five calm seconds and stays inside the server's input buffer. Before the first
    /// feedback it is two ticks, two measured jitters and the path's one-way delay as the
    /// handshake measured it (half its shortest round trip less the server's tick, ADR-10096),
    /// so the first inputs of a slow path are stamped for ticks they can still reach and a LAN
    /// starts at two ticks.
    pub fn input_lead_ticks(&self) -> u64 {
        let max = self.max_input_lead();
        u64::from(
            self.lead
                .lead()
                .unwrap_or_else(|| self.initial_input_lead())
                .clamp(1, max),
        )
    }

    /// Ticks to sample and send inputs for now: every tick after the last one this returned, up
    /// to `estimated_server_tick(now_ms) + input_lead_ticks()`, so a raised lead or a clock step
    /// leaves no gap and no tick is stamped twice. Empty when no new tick is due (the server
    /// clock has not moved a tick, or the lead dropped). Call once per frame and
    /// [`NetClient::send_input`] each tick:
    /// `for tick in client.due_input_ticks(now_ms) { client.send_input(tick, sample())?; }`.
    pub fn due_input_ticks(&mut self, now_ms: u64) -> std::ops::Range<u64> {
        if !self.is_connected() {
            return 0..0;
        }
        let target = self.estimated_server_tick(now_ms) + self.input_lead_ticks();
        let from = self.last_due_tick.map_or(target, |t| t + 1);
        if from <= target {
            self.last_due_tick = Some(target);
        }
        from..(target + 1).max(from)
    }

    /// The server's last input feedback (spec 120 § 6.4): arrival margins, late, missing and
    /// too-far-ahead counts, upstream loss. `None` before the first.
    pub fn link_feedback(&self) -> Option<&LinkFeedback> {
        self.feedback.as_ref()
    }

    /// This client's input timing for `net.status` (ADR-8077); `None` before connecting.
    pub fn input_timing(&self) -> Option<InputTiming> {
        self.is_connected().then(|| InputTiming {
            lead_ticks: u32::try_from(self.input_lead_ticks()).unwrap_or(u32::MAX),
            clock_resyncs: self.clock_resyncs(),
            late_spells: self.late_spells,
            feedback: self.feedback,
            unacked_inputs: u32::try_from(self.inputs.len()).unwrap_or(u32::MAX),
            input_copies: self.input_copies,
        })
    }

    fn tick_ms(&self) -> f64 {
        1000.0 / f64::from(self.tick_rate_hz.max(1))
    }

    /// The largest input lead: two ticks inside the server's input buffer.
    fn max_input_lead(&self) -> u32 {
        u32::try_from(self.server_input_buffer_ticks.saturating_sub(2).max(1)).unwrap_or(u32::MAX)
    }

    /// The lead before the server's first feedback: two ticks and two jitters for the spread of
    /// the arrivals, and the way to the server (the estimate is the server's tick now; an input
    /// sent now arrives a one-way delay later; ADR-10096). Without the way every input of a
    /// path longer than the two ticks was late until the first feedback had come back and
    /// raised the lead, a round trip and a one-way delay after the connect.
    fn initial_input_lead(&self) -> u32 {
        let jitter_ms = self.conn.as_ref().map_or(0.0, |c| c.stats().jitter_ms);
        2 + ((2.0 * jitter_ms + self.lead_way_ms()) / self.tick_ms())
            .ceil()
            .clamp(0.0, 1024.0) as u32
    }

    /// The path's one-way delay for the startup lead: the handshake's (ADR-10096), which has the
    /// server's wait for its tick taken off already (the lead's two ticks hold that wait, so a
    /// LAN starts at two ticks as before). It is not taken from the connection's round trip:
    /// that one grows with every queue the first seconds build (the rate control finding a
    /// bottleneck), which is the server's feedback to answer. A startup lead that followed it
    /// kept `congested` over its bottleneck past the fifth second in 10 of 64 seeds. Bounded like
    /// the clock's.
    fn lead_way_ms(&self) -> f64 {
        let max_ticks = self.server_input_buffer_ticks.saturating_sub(3) as f64;
        self.handshake_one_way_ms.min(max_ticks * self.tick_ms())
    }

    /// One-way delay for the clock filter: half the round-trip time (the handshake's, until
    /// the connection has measured one, ADR-10096), bounded so the estimate leaves the input lead
    /// two ticks inside the server horizon.
    fn clock_one_way_ms(&self) -> f64 {
        let one_way = match self.conn.as_ref().map(|c| c.stats().rtt_ms) {
            Some(rtt) if rtt > 0.0 => rtt / 2.0,
            _ => self.handshake_one_way_ms,
        };
        let max_ticks = self.server_input_buffer_ticks.saturating_sub(3) as f64;
        one_way.min(max_ticks * self.tick_ms())
    }

    /// Matches a custom server input horizon for the clock estimate's RTT clamp and the input
    /// lead's clamp until the server's first [`LinkFeedback`] says the real value. The default
    /// matches the server's 64 ticks.
    pub fn set_server_input_buffer_ticks(&mut self, ticks: usize) {
        self.server_input_buffer_ticks = ticks.max(1);
    }

    // The essential class (inputs ride inside the rate controller's floor): the ECS bridge and
    // session adapter (ADR-8032) and lockstep seat inputs (ADR-8487).
    pub(crate) fn send_essential_message(&mut self, message: &G::Message) -> Result<(), SendError> {
        let header_bytes = self.header_bytes();
        let conn = self.conn.as_mut().ok_or(SendError::NotConnected)?;
        let mut w = WireWriter::new();
        w.u8(LANE_GAME);
        message.encode(&mut w);
        conn.send_essential(w.finish(), header_bytes)
    }

    /// Queues a game message.
    pub fn send(&mut self, channel: Channel, message: &G::Message) -> Result<(), SendError> {
        let header_bytes = self.header_bytes();
        let conn = self.conn.as_mut().ok_or(SendError::NotConnected)?;
        let mut w = WireWriter::new();
        w.u8(LANE_GAME);
        message.encode(&mut w);
        conn.send(channel, w.finish(), header_bytes)
    }

    /// Records the input for `tick` and queues a bundle of the recent inputs on the unreliable
    /// channel. Stamp inputs a little ahead of [`NetClient::estimated_server_tick`] so they arrive
    /// before the server simulates that tick.
    ///
    /// Size-aware (ADR-7896): the bundle holds the newest consecutive inputs that fit one
    /// datagram ([`NetClient::input_bundle_budget`]), always including this one; when the
    /// remembered history does not fit, the older inputs follow in more bundles, at most
    /// [`crate::tick::MAX_INPUT_BUNDLES_PER_SEND`] in all, so large inputs keep their redundancy
    /// instead of failing. An input too large for a datagram on its own goes on the reliable
    /// channel instead (`tick u64 | input`, at most [`crate::tick::MAX_RELIABLE_INPUT_BYTES`]
    /// encoded bytes): on loss it arrives a resend later, but intact and on its tick. A send that
    /// fails leaves the history as it was, so one refused input never blocks the next.
    pub fn send_input(&mut self, tick: u64, input: G::Input) -> Result<(), SendError> {
        let header_bytes = self.header_bytes();
        if self.capture.is_some()
            && let Ok(bytes) = input.to_bytes()
        {
            self.record(|| CaptureRecord::Input { tick, bytes });
        }
        let conn = self.conn.as_mut().ok_or(SendError::NotConnected)?;
        let budget = input_bundle_budget(conn, header_bytes);
        // Loss-aware redundancy (spec 120 § 6.1 item 3, ADR-8312): each input rides in enough
        // datagrams for a residual loss of 0.1 % at the worse of the loss this end measured and
        // the upstream loss the server reported. The measured count also raises a configured
        // redundancy below it; older bundles of large inputs count a loss under 1 % as 1 %.
        let loss = f64::from(
            conn.stats()
                .loss_fraction
                .max(self.feedback.map_or(0.0, |f| f.upstream_loss)),
        );
        self.inputs.set_copies_floor(crate::tick::loss_aware_copies(loss));
        let copies = crate::tick::loss_aware_copies(loss.max(0.01));
        self.input_copies = copies;
        let undo = self.inputs.push_undoable(tick, input);
        let bundles = self
            .inputs
            .bundles_for_send(budget, MAX_INPUT_BUNDLES_PER_SEND, copies);
        // The bundle that carries this tick (the first, unless it was older than the newest).
        let carrier = bundles.iter().position(|b| b.iter().any(|(t, _)| t == tick));
        // This input goes first, on whichever lane holds it; only its refusal fails the send.
        let sent = if let Some(carrier) = carrier {
            let trimmed = bundles
                .first()
                .is_some_and(|b| b.inputs.len() < self.inputs.len());
            let bytes = bundles.get(carrier).map(encode_bundle).unwrap_or_default();
            conn.send_essential(bytes, header_bytes)
                .map(|()| self.totals.input_bundles_trimmed += u64::from(trimmed))
        } else {
            match self.inputs.get(tick) {
                Some(newest) => encode_reliable_input(LANE_INPUT_RELIABLE, tick, newest)
                    .and_then(|bytes| conn.send(RELIABLE_INPUT_CHANNEL, bytes, header_bytes))
                    .map(|()| self.totals.reliable_inputs += 1),
                None => Ok(()),
            }
        };
        match sent {
            Ok(()) => {
                // The stamp offset (tick minus the local clock) corrects the lead (ADR-8077).
                let offset =
                    tick as f64 - self.now_ms as f64 / (1000.0 / f64::from(self.tick_rate_hz.max(1)));
                self.lead.note_stamp(tick, offset);
                // The older inputs are redundancy only. Each bundle fits the budget, so the
                // channel cannot refuse it, and if it did the newest input is already queued.
                for (i, bundle) in bundles.iter().enumerate() {
                    if Some(i) != carrier {
                        let _ = conn.send_essential(encode_bundle(bundle), header_bytes);
                    }
                }
                Ok(())
            }
            Err(e) => {
                self.inputs.undo(undo);
                Err(e)
            }
        }
    }

    /// Bytes an input bundle may take in one datagram on the current link (bundle header
    /// included; the lane byte and every endpoint, packet and record header already taken out).
    /// `None` before the client is connected. An input whose encoding is larger than this minus
    /// [`crate::tick::BUNDLE_HEADER_BYTES`] travels on the reliable input lane (ADR-7896).
    pub fn input_bundle_budget(&self) -> Option<usize> {
        self.conn
            .as_ref()
            .map(|conn| input_bundle_budget(conn, self.header_bytes()))
    }

    /// Closes the connection and tells the server. Call [`NetClient::flush`] afterwards.
    pub fn disconnect(&mut self) {
        self.send_goodbye();
        self.end(ClientEnd::Closed);
    }

    /// Sends the goodbye notice (three copies; they are not acknowledged) while connected.
    fn send_goodbye(&mut self) {
        if self.is_connected() {
            for _ in 0..3 {
                // A sealed goodbye gets a fresh counter each time, or the copies would be replays.
                let bytes = match &mut self.keys {
                    ClientKeys::Sealed(link) => link
                        .seal(SealedKind::Disconnect, &[DisconnectReason::Left.to_u8()])
                        .ok(),
                    _ => {
                        let mut w = WireWriter::new();
                        w.raw(&connected_header(PacketKind::Disconnect, self.token));
                        w.u8(DisconnectReason::Left.to_u8());
                        Some(w.finish())
                    }
                };
                if let Some(bytes) = bytes {
                    self.totals.sealed_out += u64::from(self.is_sealed());
                    self.emit(bytes);
                }
            }
        }
    }

    fn end(&mut self, why: ClientEnd) {
        if matches!(self.state, ClientState::Ended(_)) {
            return;
        }
        tracing::info!(target: "regia_net", server = %self.server, reason = why.as_str(), "client ended");
        self.record(|| CaptureRecord::Ended {
            code: why.as_str().to_owned(),
        });
        self.state = ClientState::Ended(why.clone());
        self.conn = None;
        self.keys = ClientKeys::Plain;
        self.first_exchange = None;
        self.resume = ResumeStage::Off;
        self.events.push_back(ClientEvent::Ended(why));
    }

    fn emit(&mut self, bytes: Vec<u8>) {
        if self.replay.is_some() {
            // A replay sends nothing: the capture already holds what the server answered.
            self.totals.datagrams_out += 1;
            return;
        }
        for (bytes, dest) in self.conditioner.process(bytes, self.server, self.now_ms) {
            self.write_socket(&bytes, dest);
        }
    }

    fn write_socket(&mut self, bytes: &[u8], dest: SocketAddr) {
        if self.replay.is_some() {
            self.totals.datagrams_out += 1;
            return;
        }
        match self.socket.send_to(bytes, dest) {
            Ok(()) => self.totals.datagrams_out += 1,
            Err(e) => {
                self.totals.socket_errors += 1;
                self.totals.socket_drops += 1;
                tracing::debug!(error = %e, "regia_net client send error");
            }
        }
    }

    /// Reads every waiting datagram from the server and checks the timeouts.
    pub fn receive(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("regia_net::client_receive").entered();
        self.now_ms = self.now_ms.max(now_ms);
        self.prev_poll_ms = self.last_poll_ms;
        self.credit_connect_stall();
        let started = *self.started_ms.get_or_insert(self.now_ms);
        self.unreliable_this_receive = 0;
        self.reliable_this_receive = 0;
        let now = self.now_ms;
        self.record(|| CaptureRecord::Receive { at_ms: now });
        if self.replay.is_some() {
            self.replay_receive();
            if matches!(self.state, ClientState::Connected { .. })
                && self.now_ms.saturating_sub(self.last_heard_ms) >= self.config.timeout_ms
            {
                self.end(ClientEnd::Disconnected(DisconnectReason::Timeout));
            }
            return;
        }
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        for _ in 0..1024 {
            match self.socket.recv_from(&mut buf) {
                Ok(Some((n, from))) => {
                    self.totals.datagrams_in += 1;
                    if from != self.server {
                        if !self.on_candidate_datagram(&buf[..n], from) {
                            self.totals.foreign += 1;
                        }
                        continue;
                    }
                    self.handle_datagram(&buf[..n]);
                }
                Ok(None) => break,
                Err(e) => {
                    self.totals.socket_errors += 1;
                    tracing::debug!(error = %e, "regia_net client receive error");
                    break;
                }
            }
        }
        if self.is_connected()
            && !self.interrupted
            && self.now_ms.saturating_sub(self.last_heard_ms) >= self.config.timeouts().interrupt_ms
        {
            self.interrupted = true;
            if let Some(conn) = self.conn.as_mut() {
                conn.set_interrupted(true, self.now_ms);
            }
            self.events.push_back(ClientEvent::PeerInterrupted);
            if let Some(change) = self.quality.set_interrupted(true, self.now_ms) {
                self.push_quality(change);
            }
        }
        if self.is_connected()
            && let Some(conn) = self.conn.as_ref()
        {
            let fb = self.feedback;
            let inputs = fb
                .filter(|f| f.margin_samples > 0 || f.inputs_missing > 0)
                .map(|f| InputTotals {
                    server_tick: f.server_tick,
                    late: f.inputs_late,
                    missing: f.inputs_missing,
                    margin_p5: f.margin_p5,
                });
            if let Some(change) = self
                .quality
                .sample(self.now_ms, conn, inputs, fb.map(|f| f.upstream_loss))
            {
                self.push_quality(change);
            }
        }
        // A probe nobody answered: the server predates it (spec 120 § 12.2).
        if self
            .probe_wait
            .is_some_and(|w| self.now_ms.saturating_sub(w.started_ms) >= PROBE_WAIT_MS)
        {
            self.resolve_mismatch(None);
        }
        // A handshake step handled above restarts the connect timeout (`handshake_advanced`).
        let started = self.started_ms.unwrap_or(started);
        match self.state {
            ClientState::Connecting
                if self.now_ms.saturating_sub(started) >= self.config.connect_timeout_ms =>
            {
                self.end(ClientEnd::ConnectTimeout);
            }
            ClientState::Connected { .. }
                if self.config.timeouts().expired(
                    self.now_ms,
                    self.joined_ms,
                    self.last_heard_ms,
                    self.started,
                ) =>
            {
                self.end(ClientEnd::Disconnected(DisconnectReason::Timeout));
            }
            _ => {}
        }
    }

    fn handle_datagram(&mut self, datagram: &[u8]) {
        if is_probe_answer(datagram) {
            self.on_probe_answer(datagram);
            return;
        }
        let Some((kind, body)) = split_datagram(datagram) else {
            self.totals.foreign += 1;
            return;
        };
        let connecting = matches!(self.state, ClientState::Connecting);
        let connected = self.is_connected();
        match (kind, connecting, connected) {
            (PacketKind::Challenge, true, _) => match Challenge::decode(body) {
                Ok(ch) if ch.client_nonce == self.client_nonce => self.on_challenge(ch),
                _ => self.totals.malformed += 1,
            },
            (PacketKind::KeyReply, true, _) => match KeyReply::decode(body) {
                Ok(reply) if reply.client_nonce == self.client_nonce => self.on_key_reply(&reply),
                _ => self.totals.malformed += 1,
            },
            (PacketKind::ResumeReply, true, _) => match ResumeReply::decode(body) {
                Ok(reply) if reply.client_nonce == self.client_nonce => self.on_resume_reply(&reply),
                _ => self.totals.malformed += 1,
            },
            (PacketKind::ResumeRetry, true, _) => match ResumeRetry::decode(body) {
                Ok(retry) if retry.client_nonce == self.client_nonce => self.on_resume_retry(retry),
                _ => self.totals.malformed += 1,
            },
            (PacketKind::Sealed, _, _) => self.on_sealed(body),
            // Once a key exchange started, or on a link that must be sealed, a plain `Accept`
            // proves nothing: anyone who read the request nonce can write one. A plain `Deny`
            // still counts until the keys exist: the server may refuse before any curve work.
            (PacketKind::Accept, true, _)
                if !matches!(self.keys, ClientKeys::Plain) || self.crypto_required() =>
            {
                self.totals.auth_failed += 1;
            }
            (PacketKind::Deny, true, _) if self.is_sealed() => self.totals.auth_failed += 1,
            (PacketKind::Payload | PacketKind::Disconnect, _, true) if self.is_sealed() => {
                self.totals.auth_failed += 1;
            }
            // The server accepts a challenge response, so an accept before any challenge did not
            // come from it.
            (PacketKind::Accept, true, _) => match Accept::decode(body) {
                Ok(acc) if acc.client_nonce == self.client_nonce && self.challenge.is_some() => {
                    self.on_accept(acc);
                }
                _ => self.totals.malformed += 1,
            },
            (PacketKind::Deny, true, _) => match Deny::decode_with_stamp(body) {
                // A link that must be sealed takes a plain refusal only for a reason the server
                // sends before the keys exist; the rest come sealed (audit wave 5, netplug L2).
                Ok((deny, _))
                    if deny.client_nonce == self.client_nonce
                        && self.crypto_required()
                        && !deny.reason.sent_before_keys() =>
                {
                    self.totals.auth_failed += 1;
                }
                Ok((deny, stamp)) if deny.client_nonce == self.client_nonce => self.on_deny(deny, stamp),
                _ => self.totals.malformed += 1,
            },
            (PacketKind::Payload, _, true) => self.on_payload(body),
            (PacketKind::Disconnect, _, true) if body.get(..4) == Some(&self.token.to_le_bytes()[..]) => {
                self.on_goodbye(body.get(4).copied());
            }
            // Late handshake replies after the accept, or traffic after the end: ignore.
            _ => {}
        }
    }

    /// A datagram from an address that is not the server's, once connected: the server's address
    /// may have changed (its NAT moved it, a multi-homed host answered from another interface), or
    /// someone copied a server record and raced it from elsewhere. A record that is the server's
    /// newest and authentic (on a plain link: the session token and a newer packet sequence) makes
    /// the address a candidate (ADR-8299): its content is delivered, the client keeps sending to
    /// the validated address and probes the candidate alone, and moves when a record from the
    /// candidate acknowledges the probe. `false` for anything else (counted as foreign).
    fn on_candidate_datagram(&mut self, datagram: &[u8], from: SocketAddr) -> bool {
        if !self.is_connected() {
            return false;
        }
        let Some((kind, body)) = split_datagram(datagram) else {
            return false;
        };
        let now = self.now_ms;
        // An address that already authenticated a record is past the trial budget (ADR-8422).
        let known = self.path.is_candidate(from, now);
        let unsealed = matches!(self.keys, ClientKeys::Plain);
        let plain = match (kind, &mut self.keys) {
            (PacketKind::Sealed, ClientKeys::Sealed(link)) => {
                let Some(header) = SealedHeader::parse(body) else {
                    return false;
                };
                if header.token != self.token {
                    return false;
                }
                if !(known || self.path_limiter.admit(self.token, from, now))
                    || link
                        .newest_counter()
                        .is_some_and(|n| header.record_counter() <= n)
                {
                    self.totals.rebinds_refused += 1;
                    return true;
                }
                let opened = link.open(header);
                if opened.is_err() {
                    self.totals.rebinds_refused += 1;
                }
                match self.totals.note_open(opened) {
                    Some((SealedKind::Payload, plain)) => plain,
                    // An authentic goodbye is the server's, whichever address carried it.
                    Some((SealedKind::Disconnect, plain)) => {
                        self.on_goodbye(plain.first().copied());
                        return true;
                    }
                    Some(_) | None => return true,
                }
            }
            (PacketKind::Payload, ClientKeys::Plain) => {
                if body.get(..4) != Some(&self.token.to_le_bytes()[..]) {
                    return false;
                }
                let packet = &body[4..];
                let newer = self.conn.as_ref().is_some_and(|c| c.is_newer_packet(packet));
                if !(known || self.path_limiter.admit(self.token, from, now)) || !newer {
                    self.totals.rebinds_refused += 1;
                    return true;
                }
                packet.to_vec()
            }
            _ => return false,
        };
        if self.path.validates(from, &plain) {
            // A plain link's packets held from the candidate (KI-NT-018, ADR-10077); none on a
            // sealed link.
            let held = self.path.take_held(from);
            self.path.moved(now);
            // A port-only change keeps the round-trip estimate (RFC 9000 § 9.4; ADR-8422).
            if self.server.ip() != from.ip()
                && let Some(conn) = self.conn.as_mut()
            {
                conn.path_changed();
            }
            let old = std::mem::replace(&mut self.server, from);
            self.totals.rebinds += 1;
            tracing::info!(target: "regia_net", %old, new = %from, "server moved to a validated address");
            // Held packets in arrival order, then the one that validated the candidate.
            for p in &held {
                self.deliver(p);
            }
            self.deliver(&plain);
            return true;
        }
        self.path.note(from, datagram.len(), now);
        if unsealed {
            // A plain packet has no tag: nothing proves it is the server's until a packet from
            // `from` answers the probe, so it is held, not applied (KI-NT-018, ADR-10077); past
            // the hold's bound it is dropped.
            if !self.path.hold(from, &plain) {
                self.totals.rebinds_refused += 1;
            }
        } else {
            self.deliver(&plain);
        }
        self.probe_server_path(from);
        true
    }

    /// Sends one probe to candidate server address `to` when due and within three times the bytes
    /// it sent (RFC 9000 § 8.1).
    fn probe_server_path(&mut self, to: SocketAddr) {
        let now = self.now_ms;
        let len = self.header_bytes() + crate::connection::PACKET_HEADER_BYTES;
        if !self.path.get(to).is_some_and(|c| c.may_probe(len, now)) {
            return;
        }
        let Some(conn) = self.conn.as_mut() else {
            return;
        };
        let probe = match &mut self.keys {
            ClientKeys::Sealed(link) => {
                let reserved = [0u8; SEALED_RESERVED_HEADER_BYTES];
                let (seq, packet) = conn.path_probe(now, &reserved);
                link.seal(SealedKind::Payload, &packet[SEALED_RESERVED_HEADER_BYTES..])
                    .ok()
                    .map(|bytes| (seq, bytes))
            }
            _ => Some(conn.path_probe(now, &connected_header(PacketKind::Payload, self.token))),
        };
        let Some((seq, bytes)) = probe else {
            return;
        };
        if matches!(self.keys, ClientKeys::Sealed(_)) {
            self.totals.sealed_out += 1;
        }
        self.path.probed(to, seq, bytes.len(), now);
        self.totals.path_probes += 1;
        for (bytes, dest) in self.conditioner.process(bytes, to, now) {
            self.write_socket(&bytes, dest);
        }
    }

    /// Replay: the records of the next captured receive (see [`NetClient::replay`]).
    fn replay_receive(&mut self) {
        let now = self.now_ms;
        let Some(feed) = self.replay.as_mut() else {
            return;
        };
        // Skip what the client and game wrote after the last receive (inputs, flushes, frames,
        // ends): the replay writes its own, and the comparison reads both.
        loop {
            match feed.records.front() {
                Some(CaptureRecord::Receive { .. }) | None => break,
                Some(_) => {
                    feed.records.pop_front();
                }
            }
        }
        match feed.records.pop_front() {
            Some(CaptureRecord::Receive { at_ms }) => {
                if at_ms != now {
                    feed.time_mismatches += 1;
                }
            }
            _ => {
                feed.overruns += 1;
                return;
            }
        }
        loop {
            let Some(feed) = self.replay.as_mut() else {
                return;
            };
            let next = match feed.records.front() {
                Some(
                    CaptureRecord::Accept { .. }
                    | CaptureRecord::Deny { .. }
                    | CaptureRecord::Packet { .. }
                    | CaptureRecord::Goodbye { .. }
                    | CaptureRecord::Ended { .. }
                    | CaptureRecord::Handshake { .. },
                ) => feed.records.pop_front(),
                _ => None,
            };
            let Some(record) = next else {
                return;
            };
            // The handshake's round trip is no datagram: it is what the run's handshake
            // measured, for the accept behind it (ADR-10231).
            if let CaptureRecord::Handshake { rtt_ms } = record {
                self.handshake_rtt_ms = Some(rtt_ms);
                continue;
            }
            self.totals.datagrams_in += 1;
            let connecting = matches!(self.state, ClientState::Connecting);
            match record {
                CaptureRecord::Accept { sealed, datagram } if connecting => {
                    match split_datagram(&datagram).map(|(_, body)| Accept::decode(body)) {
                        Some(Ok(acc)) => {
                            self.replay_sealed = sealed;
                            self.on_accept(acc);
                        }
                        _ => self.totals.malformed += 1,
                    }
                }
                CaptureRecord::Deny { datagram } if connecting => {
                    match split_datagram(&datagram).map(|(_, body)| Deny::decode_with_stamp(body)) {
                        Some(Ok((deny, stamp))) => self.on_deny(deny, stamp),
                        _ => self.totals.malformed += 1,
                    }
                }
                CaptureRecord::Packet { bytes } if self.is_connected() => self.deliver(&bytes),
                CaptureRecord::Goodbye { reason } if self.is_connected() => self.on_goodbye(reason),
                // A captured end while connecting came from the handshake, which a replay does not
                // run; an end after the accept is the replay's own to reach.
                CaptureRecord::Ended { code } if connecting => self.end(replayed_end(&code)),
                _ => {}
            }
        }
    }

    fn on_deny(&mut self, deny: Deny, server: Option<VersionStamp>) {
        self.record(|| CaptureRecord::Deny {
            datagram: server
                .as_ref()
                .map_or_else(|| deny.to_datagram(), |stamp| deny.to_datagram_with_stamp(stamp)),
        });
        let mismatch = deny.reason == DenyReason::ProtocolMismatch;
        self.end(ClientEnd::Denied {
            reason: deny.reason,
            detail: deny.detail,
        });
        if !mismatch {
            return;
        }
        match server {
            // The server stamped its deny: both versions are known now.
            Some(stamp) => self.resolve_mismatch(Some(stamp)),
            // A server that predates stamps; ask it with the version probe (spec 120 § 12.2).
            None => {
                self.probe_wait = Some(ProbeWait {
                    nonce: fresh_nonce(),
                    started_ms: self.now_ms,
                    last_sent_ms: None,
                });
            }
        }
    }

    /// A probe answer from the server (plaintext, before any other parsing).
    fn on_probe_answer(&mut self, datagram: &[u8]) {
        let Some(wait) = self.probe_wait else {
            self.totals.foreign += 1;
            return;
        };
        match ProbeAnswer::decode(datagram) {
            Ok(answer) if answer.nonce == wait.nonce => self.resolve_mismatch(Some(answer.stamp)),
            _ => self.totals.malformed += 1,
        }
    }

    /// Records the refusal's versions and raises [`ClientEvent::VersionMismatch`]. `None`: the
    /// server did not answer the probe, so it predates it and is older than this build.
    fn resolve_mismatch(&mut self, server: Option<VersionStamp>) {
        self.probe_wait = None;
        let report = VersionMismatchReport::new(Some(self.version_stamp()), server);
        tracing::info!(
            target: "regia_net",
            server = %self.server,
            side = report.side.as_str(),
            message = report.message.as_str(),
            "version mismatch"
        );
        self.events.push_back(ClientEvent::VersionMismatch {
            server_game_version: report.server.as_ref().map(VersionStamp::display_version),
            ours: self.version_stamp().display_version(),
            server_is_older: report.server_is_older,
            side: report.side,
            message: report.message.clone(),
        });
        self.version_mismatch = Some(report);
    }

    /// Sends the version probe while it waits; gives up after `PROBE_WAIT_MS`.
    fn drive_probe(&mut self, now: u64) {
        let Some(mut wait) = self.probe_wait else {
            return;
        };
        if now.saturating_sub(wait.started_ms) >= PROBE_WAIT_MS {
            self.resolve_mismatch(None);
            return;
        }
        if wait
            .last_sent_ms
            .is_none_or(|t| now.saturating_sub(t) >= PROBE_RESEND_MS)
        {
            wait.last_sent_ms = Some(now);
            self.probe_wait = Some(wait);
            let request = ProbeRequest { nonce: wait.nonce }.to_datagram();
            self.emit(request);
        }
    }

    fn on_goodbye(&mut self, reason: Option<u8>) {
        self.record(|| CaptureRecord::Goodbye { reason });
        let reason = reason.map_or(DisconnectReason::Left, DisconnectReason::from_u8);
        self.end(ClientEnd::Disconnected(reason));
    }

    /// The server's choice. A plain `Challenge` is cleartext: the `ConnectRequest` nonce is
    /// readable, so anyone who saw it can write one. It is taken only when this client does not
    /// require a seal. On a link that must be sealed it is counted and ignored, and the attempt
    /// waits for a `CRYPTO_VERSION` challenge. Ending here would let the forgery abort the
    /// handshake before that challenge arrives.
    fn on_challenge(&mut self, ch: Challenge) {
        if matches!(self.keys, ClientKeys::Sealed(_)) {
            return; // Plaintext cannot replace authenticated state.
        }
        // A challenge for this attempt's nonce while a resume flight is out: the server refused
        // the resume, or another build answered the `ConnectRequest` beside it. The full
        // handshake runs from here; the flight stays for a `ResumeReply` that still opens, until
        // the full handshake has keys of its own (ADR-8858 A6, A8).
        if let ResumeStage::Flying(attempt) = &mut self.resume
            && !attempt.refused
        {
            attempt.refused = true;
            self.totals.resume_fallbacks += 1;
        }
        if self.challenge == Some(ch) {
            if self.challenge_replaced
                && self.key_init_sent_ms.is_some()
                && self.restarts < MAX_HANDSHAKE_RESTARTS
            {
                // The server may retain the previous share on this cookie. Its refresh asks
                // for a new attempt, not more curve work on that existing pending row.
                self.restart_handshake();
                self.preserve_challenge_deadline = true;
            }
            // Ordinary duplicates retain the share, so a lost KeyReply can be retransmitted.
            return;
        }
        if self
            .first_exchange
            .as_ref()
            .is_some_and(|(first, _)| *first == ch)
        {
            // The server names the first challenge again: its share is the one it may hold.
            // Go back to that exchange instead of starting a third one.
            if let Some((first, handshake)) = self.first_exchange.take() {
                self.challenge_replaced |= self.key_init_sent_ms.is_some();
                self.keys = ClientKeys::Exchanging(handshake);
                self.challenge = Some(first);
                self.key_init_sent_ms = None;
                self.last_handshake_ms = None;
            }
            return;
        }
        if ch.crypto_version == PLAIN_VERSION {
            if self.crypto_required() {
                self.totals.auth_failed += 1;
                return;
            }
        } else if ch.crypto_version != CRYPTO_VERSION {
            self.end(ClientEnd::Denied {
                reason: DenyReason::CryptoVersionMismatch,
                detail: format!(
                    "server chose suite {}, client runs {CRYPTO_VERSION}",
                    ch.crypto_version
                ),
            });
            return;
        } else if !self.config.crypto.can_encrypt() {
            self.totals.malformed += 1; // We offered no suite, so no server may choose one.
            return;
        } else {
            let prologue = crypto::prologue(
                protocol_id::<G>(),
                self.client_nonce,
                ch.crypto_version,
                &ch.cookie,
                ch.password.as_ref(),
            );
            match ClientHandshake::start(&prologue, self.config.server_key) {
                Ok(handshake) => {
                    let previous =
                        std::mem::replace(&mut self.keys, ClientKeys::Exchanging(Box::new(handshake)));
                    // A later, equally valid challenge (the cookie's time bucket turned over
                    // between two requests) must not cost the exchange already under way: the
                    // server stores the first share it sees and answers only that one. Keep the
                    // first exchange whose KeyInit left; a second one that left is the risk the
                    // refresh rule below handles.
                    if let (ClientKeys::Exchanging(old), Some(challenge), true) =
                        (previous, self.challenge, self.key_init_sent_ms.is_some())
                    {
                        if self.first_exchange.is_none() {
                            self.first_exchange = Some((challenge, old));
                        } else {
                            self.challenge_replaced = true;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "regia_net", error = %e, "key exchange could not start");
                    self.end(ClientEnd::EncryptionUnavailable);
                    return;
                }
            }
        }
        if ch.crypto_version == PLAIN_VERSION {
            self.keys = ClientKeys::Plain;
            self.first_exchange = None;
        }
        if self.challenge.is_none() {
            if !self.preserve_challenge_deadline {
                self.handshake_advanced();
            }
            self.preserve_challenge_deadline = false;
            // What answers this challenge is a new flight. A replacement is not: the server
            // may still answer what left before it (it keeps the first share it saw), so the
            // flight keeps its first send and a leg is never read shorter than the path
            // (ADR-10096).
            self.flight_sent_ms = None;
        }
        // A replacement is not authenticated progress and must not extend the timeout.
        self.key_init_sent_ms = None;
        self.challenge = Some(ch);
        // Answer at once rather than waiting for the resend timer.
        self.last_handshake_ms = None;
    }

    /// While connecting, a gap between two polls longer than `CONNECT_STALL_MS` counts only that
    /// long against the connect timeout (ADR-3075, found by the two-machine match: a windowed client
    /// sent its challenge response, the conditioner dropped it, and its first frame then stalled
    /// 4.6 s compiling pipelines on a software renderer, so the attempt ended before the resend).
    /// The credit is bounded per gap, so a client that polls only every few seconds still times out.
    fn credit_connect_stall(&mut self) {
        let now = self.now_ms;
        if matches!(self.state, ClientState::Connecting)
            && let (Some(last), Some(started)) = (self.last_poll_ms, self.started_ms)
        {
            let gap = now.saturating_sub(last);
            if gap > CONNECT_STALL_MS {
                self.started_ms = Some(started.saturating_add(gap - CONNECT_STALL_MS).min(now));
            }
        }
        self.last_poll_ms = Some(now);
    }

    /// The handshake reached its next step (the first challenge, the key reply): the connect
    /// timeout counts again from here. A client whose frames stalled (a first frame that compiles
    /// shaders on a software renderer, ADR-3075) handles a reply that waited in the socket and
    /// then gets a full timeout for the next step, instead of giving up in the same poll. Each
    /// step happens once per attempt, so an attempt still ends within three timeouts.
    fn handshake_advanced(&mut self) {
        self.started_ms = Some(self.now_ms);
        self.note_handshake_leg();
    }

    /// A reply moved the handshake on: one round trip of the path has been seen (ADR-10096).
    /// The flight it answers first left at `flight_sent_ms`, and the reply was not in the socket
    /// at the poll before this one, so that stretch is a lower bound of the round trip however
    /// long this client's frames are (a client that stalls for seconds with the reply waiting
    /// reads no delay into it). Only a lost first copy makes a leg longer than the path, by whole
    /// resend intervals, so the connect keeps its shortest leg.
    fn note_handshake_leg(&mut self) {
        let (Some(sent), Some(prev)) = (self.flight_sent_ms.take(), self.prev_poll_ms) else {
            return;
        };
        let leg = prev.saturating_sub(sent);
        self.handshake_rtt_ms = Some(self.handshake_rtt_ms.map_or(leg, |best| best.min(leg)));
    }

    fn on_key_reply(&mut self, reply: &KeyReply) {
        let ClientKeys::Exchanging(handshake) = &mut self.keys else {
            return; // A late copy.
        };
        let mut result = handshake.read_reply(&reply.noise);
        // The server answers the share it stored. When that is the first exchange of this
        // attempt, finish on it, with the challenge it was started from (the password proof and
        // the prologue are bound to that cookie).
        if !matches!(result, Ok(_) | Err(CryptoError::KeyMismatch))
            && let Some((_, first)) = self.first_exchange.as_mut()
        {
            match first.read_reply(&reply.noise) {
                Ok(key) => {
                    if let Some((challenge, first)) = self.first_exchange.take() {
                        self.challenge = Some(challenge);
                        self.keys = ClientKeys::Exchanging(first);
                        result = Ok(key);
                    }
                }
                Err(CryptoError::KeyMismatch) => result = Err(CryptoError::KeyMismatch),
                Err(_) => {}
            }
        }
        match result {
            Ok(key) => {
                self.first_exchange = None;
                // The full handshake has its own keys: a late `ResumeReply` no longer counts.
                self.resume = ResumeStage::Off;
                self.server_key = Some(key);
                self.totals.handshakes += 1;
                self.handshake_advanced();
                let exchanging = std::mem::replace(&mut self.keys, ClientKeys::Plain);
                if let ClientKeys::Exchanging(handshake) = exchanging {
                    match handshake.into_link() {
                        Ok(mut link) => {
                            link.set_rekey_policy(self.config.rekey);
                            self.keys = ClientKeys::Sealed(Box::new(link));
                        }
                        Err(_) => self.end(ClientEnd::EncryptionUnavailable),
                    }
                }
                self.last_handshake_ms = None;
                self.flight_sent_ms = None;
            }
            Err(CryptoError::KeyMismatch) => {
                tracing::warn!(target: "regia_net", server = %self.server, "the server's key is not the pinned key");
                self.end(ClientEnd::ServerKeyMismatch);
            }
            Err(_) => self.totals.auth_failed += 1,
        }
    }

    /// The server's `ResumeRetry`: this source is outside the ticket's address block. The same
    /// flight goes again with the cookie (ADR-8858 A6); a later retry (the cookie's time bucket
    /// turned over) replaces the cookie.
    fn on_resume_retry(&mut self, retry: ResumeRetry) {
        let ResumeStage::Flying(attempt) = &mut self.resume else {
            return; // A late copy.
        };
        if attempt.refused || attempt.cookie == Some(retry.cookie) {
            return;
        }
        let first_answer = !attempt.answered;
        attempt.answered = true;
        attempt.cookie = Some(retry.cookie);
        if first_answer {
            self.started_ms = Some(self.now_ms);
            // The flight with the cookie is a new one; a second cookie is not (the server may
            // still answer the flight that carried the first).
            self.note_handshake_leg();
            self.flight_sent_ms = None;
        }
        // Answer at once rather than waiting for the resend timer.
        self.last_handshake_ms = None;
    }

    /// The server's `ResumeReply`: the second Noise message, then the sealed accept under the
    /// link it finishes. Only the server that opened the ticket knows the PSK, so a reply that
    /// opens is the server of the credential's link.
    fn on_resume_reply(&mut self, reply: &ResumeReply) {
        if matches!(self.keys, ClientKeys::Sealed(_)) {
            return; // The full handshake already has keys.
        }
        let ResumeStage::Flying(attempt) = &mut self.resume else {
            return; // A late copy.
        };
        if attempt.flight.read_reply(&reply.noise).is_err() {
            self.totals.auth_failed += 1;
            return;
        }
        let ResumeStage::Flying(attempt) = std::mem::replace(&mut self.resume, ResumeStage::Off) else {
            return;
        };
        let ResumeAttempt {
            flight, credential, ..
        } = *attempt;
        let mut link = match flight.into_link() {
            Ok(link) => link,
            Err(_) => {
                self.end(ClientEnd::EncryptionUnavailable);
                return;
            }
        };
        link.set_rekey_policy(self.config.rekey);
        self.server_key = credential.server_key();
        self.totals.handshakes += 1;
        self.first_exchange = None;
        self.keys = ClientKeys::Sealed(Box::new(link));
        self.handshake_advanced();
        match split_datagram(&reply.accept) {
            Some((PacketKind::Sealed, body)) => self.on_sealed(body),
            _ => self.totals.malformed += 1,
        }
        if self.is_connected() {
            self.totals.resumed_one_rtt += 1;
        }
        if matches!(self.state, ClientState::Connecting) {
            // The noise opened and the record after it did not yield an accept: the tail was
            // changed on the path. This link has no accept and never will; start over without
            // the resume.
            self.totals.resume_fallbacks += 1;
            self.restart_handshake();
        }
    }

    /// Whether this attempt starts with a resume flight: a credential, a link that may be sealed,
    /// a pinned key that is the credential's, a path that carries the padded flight, and no
    /// restart yet (a restart is a fresh nonce, and the flight is bound to its nonce).
    fn start_resume(&mut self) -> Option<Box<ResumeAttempt>> {
        let credential = self.config.resumption.clone()?;
        if !self.config.crypto.can_encrypt()
            || self.restarts > 0
            || self
                .config
                .server_key
                .is_some_and(|pin| credential.server_key() != Some(pin))
            || self.socket.max_datagram_size(self.server) < RESUME_INIT_BYTES
        {
            return None;
        }
        let hello = ResumeHello {
            flight: 0,
            name: self.config.name.clone(),
            role: self.config.role,
            build: self.config.build.clone(),
            join_data: self.join_data.clone(),
        };
        let flight = ResumeFlight::start(
            &credential,
            protocol_id::<G>(),
            self.client_nonce,
            CRYPTO_VERSION,
            hello,
        )
        .ok()?;
        self.totals.resume_inits += 1;
        Some(Box::new(ResumeAttempt {
            flight,
            credential,
            cookie: None,
            sends: 0,
            answered: false,
            refused: false,
        }))
    }

    /// The resume flight's datagram for this resend, and whether the full handshake's datagram
    /// goes out too. Before any answer the flight goes alone; after
    /// [`RESUME_INIT_UNANSWERED_SENDS`] sends without one the `ConnectRequest` goes with it, so a
    /// server that drops the resume kind unread (a build before wire 6) still answers
    /// (ADR-8858 A8). After a `Challenge` only the full handshake is sent.
    fn resume_datagram(&mut self) -> (Option<Vec<u8>>, bool) {
        if matches!(self.resume, ResumeStage::Idle) {
            self.resume = match self.challenge {
                None => self.start_resume().map_or(ResumeStage::Off, ResumeStage::Flying),
                Some(_) => ResumeStage::Off,
            };
        }
        let stamp = self.version_stamp();
        let nonce = self.client_nonce;
        let ResumeStage::Flying(attempt) = &mut self.resume else {
            return (None, true);
        };
        if attempt.refused || self.challenge.is_some() {
            return (None, true);
        }
        let datagram = ResumeInit {
            protocol_id: protocol_id::<G>(),
            client_nonce: nonce,
            crypto_version: CRYPTO_VERSION,
            cookie: attempt.cookie,
            ticket: *attempt.credential.ticket(),
            noise: attempt.flight.message().to_vec(),
        }
        .to_datagram_with_stamp(&stamp)
        .ok();
        let with_request = !attempt.answered && attempt.sends >= RESUME_INIT_UNANSWERED_SENDS;
        attempt.sends += 1;
        (datagram, with_request)
    }

    /// A sealed record. Nothing here acts on a byte before the tag verified.
    fn on_sealed(&mut self, body: &[u8]) {
        let Some(header) = SealedHeader::parse(body) else {
            self.totals.malformed += 1;
            return;
        };
        let ClientKeys::Sealed(link) = &mut self.keys else {
            return; // No keys yet (or any more): nothing can authenticate this.
        };
        let Some((kind, plain)) = self.totals.note_open(link.open(header)) else {
            return;
        };
        let connecting = matches!(self.state, ClientState::Connecting);
        match kind {
            SealedKind::Accept if connecting => match Accept::decode(&plain) {
                Ok(acc) if acc.client_nonce == self.client_nonce => self.on_accept(acc),
                _ => self.totals.malformed += 1,
            },
            SealedKind::Deny if connecting => match Deny::decode(&plain) {
                Ok(deny) if deny.client_nonce == self.client_nonce => self.on_deny(deny, None),
                _ => self.totals.malformed += 1,
            },
            SealedKind::Payload if self.is_connected() => {
                self.path.heard_primary(self.now_ms);
                self.deliver(&plain);
            }
            SealedKind::Disconnect if self.is_connected() => self.on_goodbye(plain.first().copied()),
            // A second `Accept` after a resend, or a packet that overtook the first one.
            SealedKind::Accept | SealedKind::Payload => {}
            _ => self.totals.malformed += 1,
        }
    }

    fn on_accept(&mut self, acc: Accept) {
        let sealed = self.is_sealed() || self.replay_sealed;
        // The accept is one more answered flight. The handshake's shortest round trip anchors
        // the clock (ADR-10096): the server reads its socket once a tick, so up to one tick of
        // each leg is its wait, not the path. Without it the estimate is the whole one-way delay
        // behind until the connection's first round trip comes back. A capture holds it in front
        // of the accept (ADR-10231): a replay runs no handshake, and without the round trip it
        // anchored a one-way delay of 0 and stamped its first inputs a tick lower than the run.
        self.note_handshake_leg();
        if let Some(rtt_ms) = self.handshake_rtt_ms {
            self.record(|| CaptureRecord::Handshake { rtt_ms });
        }
        self.record(|| CaptureRecord::Accept {
            sealed,
            datagram: acc.to_datagram(),
        });
        self.token = acc.session_token;
        self.ticket = acc.resume_ticket;
        // The ticket is opaque here; the PSK is this link's own (never sent, ADR-8858 A7).
        self.resumption = match (&self.keys, acc.resumption) {
            (ClientKeys::Sealed(link), Some(ticket)) => {
                Some(ResumeCredential::new(ticket, *link.resume_psk(), self.server_key))
            }
            _ => None,
        };
        self.resume = ResumeStage::Off;
        self.tick_rate_hz = u32::from(acc.tick_rate_hz);
        self.conn = None;
        self.handshake_one_way_ms = self
            .handshake_rtt_ms
            .map_or(0.0, |rtt| ((rtt as f64 - self.tick_ms()) / 2.0).max(0.0));
        let mut clock = ClockFilter::new(self.tick_rate_hz);
        clock.on_sync(acc.server_tick, self.now_ms, self.clock_one_way_ms());
        self.clock = Some(clock);
        self.lead.reset();
        self.feedback = None;
        // A new session numbers its ticks afresh: nothing of the last one is resent.
        self.inputs = InputHistory::new(self.config.input_redundancy);
        self.last_due_tick = None;
        self.last_heard_ms = self.now_ms;
        self.joined_ms = self.now_ms;
        self.started = false;
        self.interrupted = false;
        self.quality = QualityTracker::default();
        self.next_clock_ms = None;
        self.pending_clock = None;
        self.server_stamps = false;
        let mut conn = Connection::new(self.config.link.clone());
        conn.set_tick_rate(self.tick_rate_hz);
        conn.limit_packet_bytes(self.socket.max_datagram_size(self.server), self.header_bytes());
        self.conn = Some(conn);
        let peer = PeerId(u32::from(acc.peer_id));
        self.state = ClientState::Connected { peer };
        tracing::info!(target: "regia_net", server = %self.server, %peer, "client connected");
        self.events.push_back(ClientEvent::Connected {
            peer,
            tick_rate_hz: self.tick_rate_hz,
        });
    }

    fn on_payload(&mut self, body: &[u8]) {
        if body.get(..4) != Some(&self.token.to_le_bytes()[..]) {
            self.totals.malformed += 1;
            return;
        }
        self.path.heard_primary(self.now_ms);
        self.deliver(&body[4..]);
    }

    /// Hands one authenticated (or plain, on a plain link) packet body to the connection.
    fn deliver(&mut self, packet: &[u8]) {
        self.record(|| CaptureRecord::Packet {
            bytes: packet.to_vec(),
        });
        let now = self.now_ms;
        let received = {
            let Some(conn) = self.conn.as_mut() else {
                return;
            };
            conn.receive(packet, now).map(|()| {
                let mut messages = Vec::new();
                while let Some(m) = conn.next_message() {
                    messages.push(m);
                }
                messages
            })
        };
        let messages = match received {
            Ok(messages) => messages,
            Err(e) => {
                self.totals.malformed += 1;
                // A failed link (an over-long field, or a reassembly cap or stall: spec 120
                // § 5.2) can never deliver again.
                let failed = self.conn.as_ref().and_then(|c| c.failure());
                if failed.is_some() || matches!(e, WireError::TooLong { .. }) {
                    if let Some(failure) = failed {
                        tracing::info!(target: "regia_net", failure = failure.as_str(), error = %e, "server link failed");
                    }
                    self.fail_protocol();
                }
                return;
            }
        };
        self.last_heard_ms = now;
        // Restored only on proof that the server hears this client too: a server whose packets
        // arrive while the client's do not is still an interrupted link (ADR-7995).
        if self.interrupted && self.conn.as_ref().is_some_and(|c| c.peer_hears_us(now)) {
            self.interrupted = false;
            if let Some(conn) = self.conn.as_mut() {
                conn.set_interrupted(false, now);
            }
            self.events.push_back(ClientEvent::PeerRestored);
            if let Some(change) = self.quality.set_interrupted(false, now) {
                self.push_quality(change);
            }
        }
        for (channel, bytes) in messages {
            let count = if channel.is_reliable() {
                &mut self.reliable_this_receive
            } else {
                &mut self.unreliable_this_receive
            };
            *count += 1;
            if *count > MAX_MESSAGES_PER_RECEIVE {
                self.totals.inbound_dropped += 1;
                if channel.is_reliable() {
                    // A reliable message cannot be dropped.
                    self.fail_protocol();
                    return;
                }
                continue;
            }
            let mut r = WireReader::new(&bytes);
            let failed = match r.u8() {
                Ok(LANE_GAME) => match G::Message::decode(&mut r).and_then(|m| r.finish().map(|()| m)) {
                    Ok(message) => {
                        self.started = true;
                        self.events.push_back(ClientEvent::Message { channel, message });
                        None
                    }
                    Err(e) => Some(e),
                },
                Ok(LANE_TICK_SYNC) => match r.u64() {
                    Ok(tick) => {
                        // The server's clock stamp after the tick (ADR-8182). A server that sends
                        // none (an older build) gets no clock stamps back (ADR-8889).
                        if let Ok(sent_ms) = r.u32() {
                            self.quality.on_stamp(sent_ms, now);
                            self.server_stamps = true;
                        }
                        // Never far ahead of the clock: one forged sync of `u64::MAX` would freeze
                        // the estimate, and the server would drop every later input as
                        // far-future. A reordered (older) sync is only a more delayed sample,
                        // which the filter passes over; the estimate never moves backward.
                        if sync_too_far(tick, self.estimated_server_tick(now), self.tick_rate_hz) {
                            self.totals.malformed += 1;
                        } else {
                            let one_way = self.clock_one_way_ms();
                            if let Some(clock) = self.clock.as_mut() {
                                clock.on_sync(tick, now, one_way);
                            }
                        }
                        None
                    }
                    Err(e) => Some(e),
                },
                Ok(LANE_LINK_FEEDBACK) => {
                    match LinkFeedback::decode(&mut r).and_then(|f| r.finish().map(|()| f)) {
                        Ok(fb) => {
                            self.on_feedback(fb, now);
                            None
                        }
                        Err(e) => Some(e),
                    }
                }
                Ok(_) => Some(WireError::Invalid("unknown lane")),
                Err(e) => Some(e),
            };
            let Some(error) = failed else {
                continue;
            };
            self.totals.malformed += 1;
            if channel.is_reliable() {
                // The reliable stream is ordered and every message on it counts: skipping one would
                // leave this client silently out of step with the server, so the link ends with a
                // protocol error instead (spec 120 § 11.2, ADR-7984). An unreliable message is
                // allowed to go missing: it is dropped and counted.
                tracing::warn!(target: "regia_net", server = %self.server, %error, "reliable message did not decode");
                self.fail_protocol();
                return;
            }
            tracing::debug!(target: "regia_net", server = %self.server, %error, "unreliable message dropped");
        }
    }

    /// Ends the link with [`DisconnectReason::ProtocolError`] and tells the server, so its slot is
    /// freed at once rather than at the timeout.
    fn fail_protocol(&mut self) {
        self.send_goodbye();
        self.end(ClientEnd::Disconnected(DisconnectReason::ProtocolError));
    }

    fn push_quality(&mut self, change: GradeChange) {
        tracing::debug!(target: "regia_net", from = %change.from, to = %change.to, "link quality changed");
        self.events.push_back(ClientEvent::QualityChanged {
            from: change.from,
            to: change.to,
        });
    }

    /// The quality of the link to the server while connected (spec 120 § 13.1, ADR-8182):
    /// round-trip percentiles, one-way jitter, loss on both directions (the upstream loss as the
    /// server measured it in `loss_in`), input lateness and the grade.
    pub fn quality(&self) -> Option<PeerQuality> {
        self.is_connected().then(|| self.quality.quality())
    }

    /// Takes the server's input feedback: adopts its input buffer, steers the lead and reports a
    /// late spell (ADR-8077).
    fn on_feedback(&mut self, fb: LinkFeedback, now: u64) {
        self.server_input_buffer_ticks = fb.input_buffer_ticks as usize;
        let fallback = self.initial_input_lead();
        let max_lead = self.max_input_lead();
        let tick_ms = self.tick_ms();
        let estimate = self.estimated_server_tick(now) as f64;
        let local = now as f64 / tick_ms;
        let window_ticks = (FEEDBACK_WINDOW_MS as f64 / tick_ms).ceil() as u64;
        let late = self.lead.on_feedback(
            &fb,
            now,
            fallback,
            |lead| estimate + f64::from(lead) - local,
            window_ticks,
            max_lead,
        );
        // Unacked-only redundancy (spec 120 § 6.1 item 1, ADR-8312): inputs up to the newest
        // contiguous tick arrived or no longer matter, and are never sent again.
        if let Some(tick) = fb.newest_contiguous_tick {
            self.inputs.ack_through(tick);
        }
        self.feedback = Some(fb);
        if let Some(margin_p5) = late {
            self.late_spells += 1;
            tracing::debug!(target: "regia_net", margin_p5, lead = self.input_lead_ticks(), "inputs late");
            self.events.push_back(ClientEvent::InputsLate { margin_p5 });
        }
    }

    /// Sends handshake packets while connecting, or every due packet once connected, and
    /// releases conditioner-held datagrams.
    pub fn flush(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("regia_net::client_flush").entered();
        self.now_ms = self.now_ms.max(now_ms);
        let now = self.now_ms;
        self.record(|| CaptureRecord::Flush { at_ms: now });
        match self.state {
            ClientState::Connecting if self.replay.is_some() => {}
            ClientState::Connecting => {
                if self.handshake_is_stale(now) {
                    self.restart_handshake();
                }
                let due = self
                    .last_handshake_ms
                    .is_none_or(|t| now.saturating_sub(t) >= self.config.handshake_resend_ms);
                if due {
                    self.last_handshake_ms = Some(now);
                    self.flight_sent_ms.get_or_insert(now);
                    if self.challenge.is_none() {
                        self.attempt_sent_ms.get_or_insert(now);
                    } else if matches!(self.keys, ClientKeys::Exchanging(_)) {
                        self.key_init_sent_ms.get_or_insert(now);
                    }
                    let (resume, full) = self.resume_datagram();
                    if let Some(datagram) = resume {
                        self.emit(datagram);
                    }
                    if full && let Some(datagram) = self.handshake_datagram() {
                        self.emit(datagram);
                    }
                    // The server answers whichever share it stored; keep asking for both.
                    if matches!(self.keys, ClientKeys::Exchanging(_))
                        && let Some((first, handshake)) = self.first_exchange.as_ref()
                    {
                        let datagram = KeyInit {
                            client_nonce: self.client_nonce,
                            cookie: first.cookie,
                            version: first.crypto_version,
                            noise: *handshake.message(),
                        }
                        .to_datagram();
                        self.emit(datagram);
                    }
                }
            }
            ClientState::Connected { .. } => {
                let header_bytes = self.header_bytes();
                if let Some(conn) = self.conn.as_mut() {
                    conn.limit_packet_bytes(self.socket.max_datagram_size(self.server), header_bytes);
                    // The clock stamp for the server's upstream jitter (ADR-8182), essential so it
                    // rides the next packet; an unsent one is replaced. Only to a server that
                    // stamps its tick syncs (ADR-8889). A due stamp is queued in a flush that sends
                    // a datagram anyway (the tick's input or acknowledgment, ADR-10200): queued in
                    // a flush between ticks it left in a datagram of its own, about three a second
                    // more at 300 fps than at 30 (spec 120 § 4.5 A1). After `CLOCK_RIDE_WAIT_MS`
                    // it leaves on its own.
                    let rides = conn.sends_anyway(now)
                        || self
                            .next_clock_ms
                            .is_some_and(|t| now >= t.saturating_add(CLOCK_RIDE_WAIT_MS));
                    if self.server_stamps && rides && self.next_clock_ms.is_none_or(|t| now >= t) {
                        self.next_clock_ms = Some(now + QUALITY_SAMPLE_MS);
                        if let Some(id) = self.pending_clock.take() {
                            conn.cancel_essential(id);
                        }
                        let mut w = WireWriter::with_capacity(5);
                        w.u8(LANE_CLOCK);
                        w.u32(now as u32);
                        self.pending_clock = conn.queue_unreliable(w.finish(), header_bytes, true).ok();
                    }
                    let reserved = [0; SEALED_RESERVED_HEADER_BYTES];
                    let plain_header = connected_header(PacketKind::Payload, self.token);
                    // A replay of a sealed link cuts its packets at the sealed header size, as the
                    // capture's were (ADR-8221).
                    let header: &[u8] = if matches!(self.keys, ClientKeys::Sealed(_)) || self.replay_sealed {
                        &reserved
                    } else {
                        &plain_header
                    };
                    let replaying = self.replay.is_some();
                    let replay_sealed = self.replay_sealed;
                    let path = &mut self.path;
                    conn.poll_transmit(now, header, |packet| {
                        if replaying {
                            // A replay sends nothing: the same packets as the capture's, counted.
                            if replay_sealed {
                                self.totals.sealed_out += 1;
                            }
                            self.totals.datagrams_out += 1;
                            return Ok(());
                        }
                        let bytes = if let ClientKeys::Sealed(link) = &mut self.keys {
                            match link.seal(SealedKind::Payload, &packet[SEALED_RESERVED_HEADER_BYTES..]) {
                                Ok(bytes) => {
                                    self.totals.sealed_out += 1;
                                    bytes
                                }
                                Err(error) => {
                                    tracing::warn!(target: "regia_net", %error, "seal failed");
                                    // No transmission. Keep queued work and the packet sequence.
                                    return Err(crate::socket::TxRefused::TooLarge(
                                        self.socket.max_datagram_size(self.server),
                                    ));
                                }
                            }
                        } else {
                            packet
                        };
                        // The validated path went silent while a candidate sends: a copy goes
                        // there too, within three times what it sent (ADR-8422).
                        let mirror = path.mirror_target(bytes.len()).map(|to| (to, bytes.clone()));
                        crate::socket::transmit_packet(
                            &mut self.socket,
                            &mut self.conditioner,
                            &mut self.totals,
                            bytes,
                            self.server,
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
                    if self.pending_clock.is_some_and(|id| conn.was_packed(id)) {
                        self.pending_clock = None;
                    }
                }
            }
            ClientState::Ended(_) => self.drive_probe(now),
        }
        for (bytes, dest) in self.conditioner.release_due(now) {
            self.write_socket(&bytes, dest);
        }
    }

    fn join_token(&self, password_proof: [u8; 32]) -> JoinToken {
        JoinToken {
            name: self.config.name.clone(),
            role: self.config.role,
            build: self.config.build.clone(),
            password_proof,
            join_data: self.join_data.clone(),
            resume: self.config.resume,
        }
    }

    /// The next handshake datagram: where the exchange stands decides which one.
    /// Whether the server may have forgotten the state the next handshake packet relies on: the
    /// cookie (minted after this attempt's first request) or the half-open key exchange (created
    /// after the first key init). A stall longer than the stall credit lets a reply wait in the
    /// socket past that age. True only while a restart is left (ADR-4929).
    fn handshake_is_stale(&self, now: u64) -> bool {
        let anchor = match (&self.keys, self.challenge.is_some()) {
            (_, false) => None,
            (ClientKeys::Sealed(_), true) => self.key_init_sent_ms,
            (_, true) => self.attempt_sent_ms,
        };
        self.restarts < MAX_HANDSHAKE_RESTARTS
            && anchor.is_some_and(|sent| now.saturating_sub(sent) >= HANDSHAKE_REFRESH_MS)
    }

    /// Starts the handshake over as a new attempt (ADR-4929): a fresh nonce (the server refuses a
    /// second key share on one nonce), no challenge, no keys. Late answers to the old attempt no
    /// longer match its nonce. A stale-state restart treats its next challenge as first progress;
    /// a challenge-race caller preserves the existing deadline (ADR-7946).
    fn restart_handshake(&mut self) {
        self.restarts += 1;
        tracing::info!(
            target: "regia_net",
            server = %self.server,
            restart = self.restarts,
            "starting a fresh handshake attempt"
        );
        self.client_nonce = fresh_nonce();
        // A flight is bound to its nonce: the fresh attempt runs the full handshake.
        self.resume = ResumeStage::Off;
        self.challenge = None;
        self.challenge_replaced = false;
        self.first_exchange = None;
        self.preserve_challenge_deadline = false;
        self.keys = ClientKeys::Plain;
        self.server_key = None;
        self.attempt_sent_ms = None;
        self.key_init_sent_ms = None;
        self.last_handshake_ms = None;
        self.flight_sent_ms = None;
    }

    fn handshake_datagram(&mut self) -> Option<Vec<u8>> {
        let Some(ch) = self.challenge else {
            return Some(
                ConnectRequest {
                    protocol_id: protocol_id::<G>(),
                    client_nonce: self.client_nonce,
                    crypto_version: if self.config.crypto.can_encrypt() {
                        CRYPTO_VERSION
                    } else {
                        PLAIN_VERSION
                    },
                    crypto_required: self.crypto_required(),
                }
                .to_datagram_with_stamp(&self.version_stamp()),
            );
        };
        let password = self.config.password.as_deref();
        match &self.keys {
            ClientKeys::Plain => {
                let proof = match (password, ch.password) {
                    (Some(pw), Some(p)) => prove(pw, &p.salt, &p.nonce),
                    _ => [0; 32],
                };
                ChallengeResponse {
                    client_nonce: self.client_nonce,
                    cookie: ch.cookie,
                    token: self.join_token(proof),
                }
                .to_datagram()
                .ok()
            }
            ClientKeys::Exchanging(handshake) => Some(
                KeyInit {
                    client_nonce: self.client_nonce,
                    cookie: ch.cookie,
                    version: ch.crypto_version,
                    noise: *handshake.message(),
                }
                .to_datagram(),
            ),
            ClientKeys::Sealed(link) => {
                let proof = match (password, ch.password) {
                    (Some(pw), Some(p)) => {
                        let nonce = crypto::bind_password_nonce(&p.nonce, link.handshake_hash());
                        prove(pw, &p.salt, &nonce)
                    }
                    _ => [0; 32],
                };
                let token = self.join_token(proof);
                let mut w = WireWriter::new();
                token.encode(&mut w).ok()?;
                let body = w.finish();
                let ClientKeys::Sealed(link) = &mut self.keys else {
                    return None;
                };
                let bytes = link.seal(SealedKind::Join, &body).ok()?;
                self.totals.sealed_out += 1;
                Some(bytes)
            }
        }
    }

    /// [`NetClient::receive`] then [`NetClient::flush`].
    pub fn update(&mut self, now_ms: u64) {
        self.receive(now_ms);
        self.flush(now_ms);
    }

    // Session envelopes are not game traffic until their owning lifecycle reaches InGame.
    // Only the ECS session driver (`NetSession`) calls it.
    #[cfg(feature = "ecs")]
    pub(crate) fn set_started(&mut self, started: bool) {
        self.started = started;
    }

    /// Timestamp of the last validated connection datagram on this client's clock.
    pub fn last_heard_ms(&self) -> u64 {
        self.last_heard_ms
    }

    /// The `net.status` view of this client.
    pub fn status(&self) -> NetStatus {
        let state = match &self.state {
            ClientState::Connecting => "connecting".to_owned(),
            ClientState::Connected { .. } => "connected".to_owned(),
            ClientState::Ended(end) => format!("disconnected:{}", end.as_str()),
        };
        NetStatus {
            role: "client".into(),
            interrupted: self.is_connected() && self.interrupted,
            state,
            local_addr: self.socket.local_addr().ok().map(|a| a.to_string()),
            remote_addr: Some(self.server.to_string()),
            peer_id: self.peer_id().map(|p| p.0),
            tick: self.estimated_server_tick(self.now_ms),
            tick_rate_hz: self.tick_rate_hz,
            peer_count: usize::from(self.is_connected()),
            max_peers: 1,
            conditioner: self.conditioner.config().clone(),
            conditioner_stats: self.conditioner.stats(),
            totals: self.totals,
            link: self.conn.as_ref().map(Connection::stats),
            crypto: CryptoStatus {
                mode: self.config.crypto.as_str().into(),
                suite: NOISE_PATTERN.into(),
                server_key: self.server_key.map(|k| k.to_string()),
                pinned: self.config.server_key.is_some(),
                sealed: self.is_sealed(),
                sealed_peers: usize::from(self.is_sealed() && self.is_connected()),
                rekeys: match &self.keys {
                    ClientKeys::Sealed(link) => link.rekeys(),
                    _ => 0,
                },
            },
            version_mismatch: self.version_mismatch.clone(),
            input_timing: self.input_timing(),
            reconnect: None,
            streams: self
                .conn
                .as_ref()
                .map_or_else(Vec::new, Connection::stream_statuses),
            socket: self.socket.socket_report(),
            resume_refusals: std::collections::BTreeMap::new(),
            quality: self.quality(),
        }
    }

    /// Publishes this client into the agent bridge under `label` and applies a pending
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
                peers: Vec::new(),
                rejections: Vec::new(),
            },
        );
    }

    /// Seals `body` as one session record whose kind byte is `kind`.
    ///
    /// Tests only. `kind` may be a byte [`SealedKind`] does not name, so a review can send an
    /// authenticated record the receiver cannot classify. Production sends go through
    /// [`SecureLink::seal`], which takes a [`SealedKind`].
    #[cfg(test)]
    pub(crate) fn testing_seal_kind(&mut self, kind: u8, body: &[u8]) -> Result<Vec<u8>, CryptoError> {
        match &mut self.keys {
            ClientKeys::Sealed(link) => link.seal_raw_kind(kind, body),
            _ => Err(CryptoError::Unavailable),
        }
    }
}

/// Bytes an input bundle may take in one datagram of `conn` behind `header_bytes`: the largest
/// unreliable message less the lane byte (ADR-7896).
fn input_bundle_budget(conn: &Connection, header_bytes: usize) -> usize {
    conn.max_unreliable_bytes(header_bytes).saturating_sub(1)
}

/// `LANE_INPUT | bundle`.
fn encode_bundle<I: NetCodec>(bundle: &InputBundle<I>) -> Vec<u8> {
    let mut w = WireWriter::new();
    w.u8(LANE_INPUT);
    bundle.encode(&mut w);
    w.finish()
}

/// A random handshake nonce.
/// The end a replay takes for a captured end code while connecting (the handshake's local ends;
/// a refusal travels as a captured deny instead). An unknown code ends as a connect timeout.
fn replayed_end(code: &str) -> ClientEnd {
    match code {
        "closed" => ClientEnd::Closed,
        "encryption_unavailable" => ClientEnd::EncryptionUnavailable,
        "server_key_mismatch" => ClientEnd::ServerKeyMismatch,
        "crypto_version_mismatch" => ClientEnd::Denied {
            reason: DenyReason::CryptoVersionMismatch,
            detail: String::new(),
        },
        _ => ClientEnd::ConnectTimeout,
    }
}

fn fresh_nonce() -> u64 {
    let secret = fresh_secret();
    let mut nonce = [0u8; 8];
    nonce.copy_from_slice(&secret[..8]);
    u64::from_le_bytes(nonce)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ClockGame;
    impl NetGame for ClockGame {
        type Message = u8;
        type Input = u8;
        type JoinData = ();
        const GAME_ID: &'static str = "regia.clock_test";
        const PROTOCOL_VERSION: u32 = 1;
    }

    fn clock_client(conn: Connection, rate: u32) -> NetClient<ClockGame, crate::socket::LoopbackSocket> {
        let net = crate::socket::LoopbackNet::new();
        let addr = SocketAddr::from(([127, 0, 0, 1], 7000));
        let mut client =
            NetClient::connect(net.bind(addr).expect("bind"), addr, ClientConfig::default(), &())
                .expect("join");
        client.conn = Some(conn);
        client.tick_rate_hz = rate;
        let mut clock = ClockFilter::new(rate);
        let one_way = client.clock_one_way_ms();
        clock.on_sync(100, 20_000, one_way);
        client.clock = Some(clock);
        client
    }

    #[test]
    fn the_rtt_lead_leaves_room_for_two_input_ticks() {
        for rate in [60, 240] {
            let mut conn = Connection::new(LinkConfig::default());
            let mut remote = Connection::new(LinkConfig::default());
            remote.receive(&conn.poll(0, &[])[0], 0).expect("packet");
            conn.receive(&remote.poll(1900, &[])[0], 1900).expect("ack");
            let client = clock_client(conn, rate);
            let input_tick = client.estimated_server_tick(20_000) + 2;
            let mut driver = crate::tick::TickDriver::<u8>::new(64);
            for _ in 0..100 {
                driver.advance(|_, _| {});
            }
            driver.add_peer(PeerId(1));
            assert!(
                driver.push_input(PeerId(1), input_tick, 7),
                "estimated input {input_tick} at rate {rate} must fit the server horizon"
            );
        }
    }

    #[test]
    fn a_late_ack_keeps_60hz_estimated_inputs_storable() {
        let mut conn = Connection::new(LinkConfig::default());
        let mut remote = Connection::new(LinkConfig::default());
        remote.receive(&conn.poll(0, &[])[0], 0).expect("packet");
        conn.receive(&remote.poll(40, &[])[0], 40).expect("ack");
        conn.send(Channel::GAME_RPC, vec![1], 0).expect("queued");
        remote.receive(&conn.poll(50, &[])[0], 50).expect("packet");
        let late = remote.poll(60, &[]);
        conn.poll(2051, &[]);
        conn.receive(&late[0], 20_000).expect("late ack");
        let client = clock_client(conn, 60);
        let input_tick = client.estimated_server_tick(20_000) + 2;
        let mut driver = crate::tick::TickDriver::<u8>::new(64);
        for _ in 0..100 {
            driver.advance(|_, _| {});
        }
        driver.add_peer(PeerId(1));
        assert!(
            driver.push_input(PeerId(1), input_tick, 7),
            "post-stall estimated input {input_tick} must fit"
        );
        assert_eq!(client.conn.as_ref().expect("connection").stats().rtt_ms, 40.0);
    }

    #[test]
    fn a_tick_sync_cannot_jump_past_the_clock() {
        assert!(sync_too_far(u64::MAX, 100, 60));
        assert!(sync_too_far(100 + 601, 100, 60));
        assert!(!sync_too_far(100 + 600, 100, 60));
        // A server behind the estimate is the anchor's rule, not this one's.
        assert!(!sync_too_far(5, 100, 60));
        // Before the accept the rate is 0; the guard still divides nothing.
        assert!(!sync_too_far(10, 5, 0));
    }
}
