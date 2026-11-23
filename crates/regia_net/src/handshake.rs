// SPDX-License-Identifier: MPL-2.0
//! Connection handshake: request, stateless cookie challenge, join token, accept or deny.
//!
//! ```text
//! client                                   server
//!   ConnectRequest(protocol_id, nonce) ──▶   (no state kept; padded so the reply is never larger)
//!   ◀── Challenge(nonce, cookie, password salt + nonce?)
//!   ChallengeResponse(nonce, cookie, JoinToken) ──▶   cookie checked, then the peer is created
//!   ◀── Accept(token, peer id, tick rate, tick) | Deny(reason)
//! ```
//!
//! The cookie is a keyed hash of the client's address, its request nonce and a ten-second time
//! bucket, so the server allocates nothing for a spoofed source address and a cookie dies after at
//! most two buckets (the HelloVerifyRequest idea of RFC 6347 § 4.2.1 and the stateless retry of
//! RFC 9000 § 8.1). Every datagram starts with [`MAGIC`] and a [`PacketKind`] byte; connected
//! traffic adds the 32-bit session token the server handed out in `Accept`.
//!
//! On an encrypted link a key exchange follows the challenge and the join token, `Accept` and
//! `Deny` travel sealed: see [`crate::crypto`] (ADR-1901).

use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::version_probe::VersionStamp;
use crate::wire::{WireError, WireReader, WireWriter, constant_time_eq};

/// First four bytes of every `regia_net` datagram.
pub const MAGIC: [u8; 4] = *b"RGN1";
/// Bytes of the header in front of a connected packet: magic, kind, session token.
pub const CONNECTED_HEADER_BYTES: usize = 9;
/// A connect request is padded to this size so the challenge reply is never larger (no
/// amplification through a spoofed source address).
pub const CONNECT_REQUEST_BYTES: usize = 256;
/// Bytes of a `ConnectRequest`'s fields after the kind byte; the version stamp follows them.
pub const CONNECT_REQUEST_FIELD_BYTES: usize = 18;
/// The longest deny detail sent with a version stamp, so the stamped deny stays smaller than the
/// padded request it answers.
pub const MAX_STAMPED_DENY_DETAIL_BYTES: usize = 160;
/// Longest player name in a join token.
pub const MAX_NAME_BYTES: usize = 32;
/// Longest build id in a join token.
pub const MAX_BUILD_BYTES: usize = 64;
/// Largest encoded game join data in a join token.
pub const MAX_JOIN_DATA_BYTES: usize = 512;
/// Bytes of a resume ticket (spec 120 § 10.3): an opaque value the server minted in its
/// [`Accept`], presented once in a later [`JoinToken`] to bind the new connection to the old
/// `PeerId` (the shape of a TLS 1.3 session ticket, RFC 8446 § 2.2).
pub const RESUME_TICKET_BYTES: usize = 16;
/// Bytes of a resumption ticket (wire 6, ADR-8858): the opaque, sealed value in a sealed
/// [`Accept`] that a later `crate::crypto::ResumeInit` presents to resume in one round trip.
/// Only the server that sealed it can read it (`crate::crypto::TicketKeys`).
pub const RESUMPTION_TICKET_BYTES: usize = 142;
/// The largest encoded [`Accept`] body: its fixed fields, the flag and a resumption ticket.
pub const MAX_ACCEPT_BODY_BYTES: usize =
    8 + 4 + 2 + 2 + 8 + RESUME_TICKET_BYTES + 1 + RESUMPTION_TICKET_BYTES;
/// Longest deny detail text.
pub const MAX_DENY_DETAIL_BYTES: usize = 200;
/// Width of one cookie time bucket.
pub const COOKIE_BUCKET_MS: u64 = 10_000;

/// Datagram kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketKind {
    /// Client asks to connect.
    ConnectRequest = 1,
    /// Server answers with a cookie (and a password challenge).
    Challenge = 2,
    /// Client echoes the cookie with its join token.
    ChallengeResponse = 3,
    /// Server admits the client.
    Accept = 4,
    /// Server refuses the client.
    Deny = 5,
    /// Connected traffic (a `crate::connection::Connection` packet body).
    Payload = 6,
    /// Either side closes the connection.
    Disconnect = 7,
    /// Client's first Noise message, with the cookie echo.
    KeyInit = 8,
    /// Server's Noise reply.
    KeyReply = 9,
    /// An encrypted record (`crate::crypto::SecureLink`).
    Sealed = 10,
    /// Client's one-round-trip resume: a resumption ticket and the first `Noise_NNpsk0` message
    /// (`crate::crypto::ResumeInit`; wire 6, ADR-8858).
    ResumeInit = 11,
    /// Server's answer to a `ResumeInit`: the second Noise message and the sealed `Accept`
    /// (`crate::crypto::ResumeReply`).
    ResumeReply = 12,
    /// Server asks a `ResumeInit` from outside its ticket's address block to come again with a
    /// cookie (`crate::crypto::ResumeRetry`).
    ResumeRetry = 13,
}

impl PacketKind {
    /// The kind for a wire byte.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::ConnectRequest,
            2 => Self::Challenge,
            3 => Self::ChallengeResponse,
            4 => Self::Accept,
            5 => Self::Deny,
            6 => Self::Payload,
            7 => Self::Disconnect,
            8 => Self::KeyInit,
            9 => Self::KeyReply,
            10 => Self::Sealed,
            11 => Self::ResumeInit,
            12 => Self::ResumeReply,
            13 => Self::ResumeRetry,
            _ => return None,
        })
    }
}

/// Splits a datagram into its kind and the bytes after the kind byte. `None` for foreign traffic.
pub fn split_datagram(datagram: &[u8]) -> Option<(PacketKind, &[u8])> {
    if datagram.len() < 5 || datagram[..4] != MAGIC {
        return None;
    }
    Some((PacketKind::from_u8(datagram[4])?, &datagram[5..]))
}

/// The header of a connected datagram (`Payload` or `Disconnect`).
pub fn connected_header(kind: PacketKind, token: u32) -> [u8; CONNECTED_HEADER_BYTES] {
    let mut out = [0u8; CONNECTED_HEADER_BYTES];
    out[..4].copy_from_slice(&MAGIC);
    out[4] = kind as u8;
    out[5..].copy_from_slice(&token.to_le_bytes());
    out
}

pub(crate) fn begin(kind: PacketKind) -> WireWriter {
    let mut w = WireWriter::with_capacity(64);
    w.raw(&MAGIC);
    w.u8(kind as u8);
    w
}

/// What a peer joins as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PeerRole {
    /// Sends input and plays.
    Player,
    /// Watches; the server ignores its input.
    Spectator,
}

impl PeerRole {
    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::Player => 0,
            Self::Spectator => 1,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Result<Self, WireError> {
        match v {
            0 => Ok(Self::Player),
            1 => Ok(Self::Spectator),
            _ => Err(WireError::Invalid("peer role")),
        }
    }

    /// Lowercase name for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Player => "player",
            Self::Spectator => "spectator",
        }
    }
}

/// `ConnectRequest`: the protocol id and a client nonce that names this attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectRequest {
    /// The client's protocol id (`crate::payload::protocol_id`).
    pub protocol_id: u64,
    /// Random per attempt.
    pub client_nonce: u64,
    /// The suite version this client can run (`crate::crypto::PLAIN_VERSION` = none).
    pub crypto_version: u8,
    /// This client refuses a plain link to this server.
    pub crypto_required: bool,
}

impl ConnectRequest {
    /// The padded datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::ConnectRequest);
        w.u64(self.protocol_id);
        w.u64(self.client_nonce);
        w.u8(self.crypto_version);
        w.u8(u8::from(self.crypto_required));
        let mut out = w.finish();
        out.resize(CONNECT_REQUEST_BYTES, 0);
        out
    }

    /// The padded datagram with this build's version stamp in the padding (ADR-7985). A server
    /// that predates stamps reads only the fields above and ignores it.
    pub fn to_datagram_with_stamp(&self, stamp: &VersionStamp) -> Vec<u8> {
        let mut w = begin(PacketKind::ConnectRequest);
        w.u64(self.protocol_id);
        w.u64(self.client_nonce);
        w.u8(self.crypto_version);
        w.u8(u8::from(self.crypto_required));
        stamp.encode_tagged(&mut w);
        let mut out = w.finish();
        out.resize(CONNECT_REQUEST_BYTES, 0);
        out
    }

    /// The version stamp in a request body's padding (the bytes after the kind byte), if the
    /// client sent one. A malformed stamp counts as none.
    pub fn stamp_of(body: &[u8]) -> Option<VersionStamp> {
        VersionStamp::from_tagged(body.get(CONNECT_REQUEST_FIELD_BYTES..)?)
            .ok()
            .flatten()
    }

    /// Reads the bytes after the kind byte. Short (unpadded) requests are refused.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        if body.len() + 5 < CONNECT_REQUEST_BYTES {
            return Err(WireError::Invalid("connect request is not padded"));
        }
        let mut r = WireReader::new(body);
        Ok(Self {
            protocol_id: r.u64()?,
            client_nonce: r.u64()?,
            crypto_version: r.u8()?,
            crypto_required: r.u8()? != 0,
        })
    }
}

/// Password part of a challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordChallenge {
    /// The host's public salt.
    pub salt: [u8; 16],
    /// The nonce to answer.
    pub nonce: [u8; 16],
}

/// `Challenge`: the cookie to echo and, for a private host, a password challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Challenge {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Keyed hash the client must echo.
    pub cookie: [u8; 16],
    /// Present when the host has a password.
    pub password: Option<PasswordChallenge>,
    /// The suite version the link will use (`crate::crypto::PLAIN_VERSION` = plain).
    pub crypto_version: u8,
}

impl Challenge {
    /// The datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::Challenge);
        w.u64(self.client_nonce);
        w.raw(&self.cookie);
        w.u8(self.crypto_version);
        match &self.password {
            None => w.u8(0),
            Some(p) => {
                w.u8(1);
                w.raw(&p.salt);
                w.raw(&p.nonce);
            }
        }
        w.finish()
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let client_nonce = r.u64()?;
        let cookie = r.array()?;
        let crypto_version = r.u8()?;
        let password = match r.u8()? {
            0 => None,
            1 => Some(PasswordChallenge {
                salt: r.array()?,
                nonce: r.array()?,
            }),
            _ => return Err(WireError::Invalid("password flag")),
        };
        r.finish()?;
        Ok(Self {
            client_nonce,
            cookie,
            password,
            crypto_version,
        })
    }
}

/// The join token a client presents once it holds a cookie: who it is and what it brings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinToken {
    /// Display name, at most [`MAX_NAME_BYTES`].
    pub name: String,
    /// Player or spectator.
    pub role: PeerRole,
    /// The game's build id (empty when the game does not check builds).
    pub build: String,
    /// Password proof (zeros for an open host).
    pub password_proof: [u8; 32],
    /// The game's encoded `NetGame::JoinData`.
    pub join_data: Vec<u8>,
    /// The resume ticket of an earlier connection to this server (spec 120 § 10.3), or `None`
    /// for a fresh join. Single use: the server mints a new one in every [`Accept`].
    pub resume: Option<[u8; RESUME_TICKET_BYTES]>,
}

impl JoinToken {
    /// Appends the token (the plaintext of a sealed `Join` record is exactly one token).
    pub fn encode(&self, w: &mut WireWriter) -> Result<(), WireError> {
        w.str_u8(&self.name, MAX_NAME_BYTES)?;
        w.u8(self.role.to_u8());
        w.str_u8(&self.build, MAX_BUILD_BYTES)?;
        w.raw(&self.password_proof);
        w.bytes_u16(&self.join_data, MAX_JOIN_DATA_BYTES)?;
        match &self.resume {
            Some(ticket) => {
                w.u8(1);
                w.raw(ticket);
            }
            None => w.u8(0),
        }
        Ok(())
    }

    /// Reads one token.
    pub fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            name: r.str_u8(MAX_NAME_BYTES)?.to_owned(),
            role: PeerRole::from_u8(r.u8()?)?,
            build: r.str_u8(MAX_BUILD_BYTES)?.to_owned(),
            password_proof: r.array()?,
            join_data: r.bytes_u16(MAX_JOIN_DATA_BYTES)?.to_vec(),
            resume: match r.u8()? {
                0 => None,
                1 => Some(r.array()?),
                _ => return Err(WireError::Invalid("resume ticket flag")),
            },
        })
    }
}

/// `ChallengeResponse`: cookie echo plus the join token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChallengeResponse {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Echo of the cookie.
    pub cookie: [u8; 16],
    /// Who joins.
    pub token: JoinToken,
}

impl ChallengeResponse {
    /// The datagram; fails when a token field exceeds its limit.
    pub fn to_datagram(&self) -> Result<Vec<u8>, WireError> {
        let mut w = begin(PacketKind::ChallengeResponse);
        w.u64(self.client_nonce);
        w.raw(&self.cookie);
        self.token.encode(&mut w)?;
        Ok(w.finish())
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let client_nonce = r.u64()?;
        let cookie = r.array()?;
        let token = JoinToken::decode(&mut r)?;
        r.finish()?;
        Ok(Self {
            client_nonce,
            cookie,
            token,
        })
    }
}

/// `Accept`: the session token and the facts a client needs to start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Accept {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Token every connected datagram must carry.
    pub session_token: u32,
    /// The client's peer id.
    pub peer_id: u16,
    /// Authoritative tick rate.
    pub tick_rate_hz: u16,
    /// Server tick when the accept was sent.
    pub server_tick: u64,
    /// The ticket a later [`JoinToken::resume`] presents to come back as the same peer (spec 120
    /// § 10.3); all zeros when the server keeps no dormant peers (`resume_grace_ms` 0).
    pub resume_ticket: [u8; RESUME_TICKET_BYTES],
    /// The resumption ticket for a one-round-trip resume (wire 6, ADR-8858): opaque to the
    /// client, which presents it with the link's resumption PSK
    /// (`crate::crypto::ResumeCredential`). `None` on a plain link (no PSK exists) and when the
    /// server keeps no dormant peers.
    pub resumption: Option<[u8; RESUMPTION_TICKET_BYTES]>,
}

impl Accept {
    /// The datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::Accept);
        self.encode(&mut w);
        w.finish()
    }

    /// The body alone, for a sealed record.
    pub fn to_body(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(192);
        self.encode(&mut w);
        w.finish()
    }

    fn encode(&self, w: &mut WireWriter) {
        w.u64(self.client_nonce);
        w.u32(self.session_token);
        w.u16(self.peer_id);
        w.u16(self.tick_rate_hz);
        w.u64(self.server_tick);
        w.raw(&self.resume_ticket);
        match &self.resumption {
            Some(ticket) => {
                w.u8(1);
                w.raw(ticket);
            }
            None => w.u8(0),
        }
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let out = Self {
            client_nonce: r.u64()?,
            session_token: r.u32()?,
            peer_id: r.u16()?,
            tick_rate_hz: r.u16()?,
            server_tick: r.u64()?,
            resume_ticket: r.array()?,
            resumption: match r.u8()? {
                0 => None,
                1 => Some(r.array()?),
                _ => return Err(WireError::Invalid("resumption ticket flag")),
            },
        };
        r.finish()?;
        Ok(out)
    }
}

/// Why a join was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DenyReason {
    /// No free peer slot.
    Full,
    /// The address is banned.
    Banned,
    /// Missing or wrong password.
    BadPassword,
    /// Different `regia_net` framing, game or game protocol version.
    ProtocolMismatch,
    /// Different game build id.
    BuildMismatch,
    /// The game refused the join data, or the token was malformed.
    InvalidJoin,
    /// The server is closing.
    ShuttingDown,
    /// The server requires encryption and the client cannot or will not run it.
    EncryptionRequired,
    /// The two sides run different versions of the crypto suite.
    CryptoVersionMismatch,
}

impl DenyReason {
    /// Whether the server can send this refusal in plain text before any keys exist: on the
    /// `ConnectRequest` or the `KeyInit`, or to a client that answered a challenge in plain. Every
    /// other refusal (`full`, `bad_password`, `build_mismatch`, `invalid_join`) travels sealed on
    /// an encrypted link, so a client that requires a seal ignores a plain copy of one: a forgery
    /// by anyone who read the request nonce (audit wave 5, netplug L2).
    pub fn sent_before_keys(self) -> bool {
        matches!(
            self,
            Self::ShuttingDown
                | Self::ProtocolMismatch
                | Self::Banned
                | Self::EncryptionRequired
                | Self::CryptoVersionMismatch
        )
    }

    fn to_u8(self) -> u8 {
        match self {
            Self::Full => 1,
            Self::Banned => 2,
            Self::BadPassword => 3,
            Self::ProtocolMismatch => 4,
            Self::BuildMismatch => 5,
            Self::InvalidJoin => 6,
            Self::ShuttingDown => 7,
            Self::EncryptionRequired => 8,
            Self::CryptoVersionMismatch => 9,
        }
    }

    fn from_u8(v: u8) -> Result<Self, WireError> {
        Ok(match v {
            1 => Self::Full,
            2 => Self::Banned,
            3 => Self::BadPassword,
            4 => Self::ProtocolMismatch,
            5 => Self::BuildMismatch,
            6 => Self::InvalidJoin,
            7 => Self::ShuttingDown,
            8 => Self::EncryptionRequired,
            9 => Self::CryptoVersionMismatch,
            _ => return Err(WireError::Invalid("deny reason")),
        })
    }

    /// Lowercase code for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Banned => "banned",
            Self::BadPassword => "bad_password",
            Self::ProtocolMismatch => "protocol_mismatch",
            Self::BuildMismatch => "build_mismatch",
            Self::InvalidJoin => "invalid_join",
            Self::ShuttingDown => "shutting_down",
            Self::EncryptionRequired => "encryption_required",
            Self::CryptoVersionMismatch => "crypto_version_mismatch",
        }
    }
}

/// `Deny`: the reason, a short detail text and, for [`DenyReason::BadPassword`], a fresh
/// challenge so the client can retry without a new request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deny {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Why.
    pub reason: DenyReason,
    /// Human-readable detail (at most [`MAX_DENY_DETAIL_BYTES`]; longer text is cut).
    pub detail: String,
}

impl Deny {
    /// The datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::Deny);
        self.encode(&mut w);
        w.finish()
    }

    /// The body alone, for a sealed record.
    pub fn to_body(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(32);
        self.encode(&mut w);
        w.finish()
    }

    /// The datagram with the server's version stamp after the detail (ADR-7985). Only for a
    /// client whose request carried a stamp: a build that predates stamps refuses trailing bytes.
    /// The detail is cut to [`MAX_STAMPED_DENY_DETAIL_BYTES`], so the answer stays smaller than the
    /// padded request.
    pub fn to_datagram_with_stamp(&self, stamp: &VersionStamp) -> Vec<u8> {
        let mut w = begin(PacketKind::Deny);
        self.encode_cut(&mut w, MAX_STAMPED_DENY_DETAIL_BYTES);
        stamp.encode_tagged(&mut w);
        w.finish()
    }

    fn encode(&self, w: &mut WireWriter) {
        self.encode_cut(w, MAX_DENY_DETAIL_BYTES);
    }

    fn encode_cut(&self, w: &mut WireWriter, max: usize) {
        w.u64(self.client_nonce);
        w.u8(self.reason.to_u8());
        let mut cut = self.detail.len().min(max);
        while !self.detail.is_char_boundary(cut) {
            cut -= 1;
        }
        // Cannot fail: the text was cut to the limit.
        let _ = w.str_u8(&self.detail[..cut], MAX_DENY_DETAIL_BYTES);
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let out = Self::decode_fields(&mut r)?;
        r.finish()?;
        Ok(out)
    }

    /// Reads the bytes after the kind byte, with the server's version stamp when one follows the
    /// detail ([`Deny::to_datagram_with_stamp`]). Any other trailing bytes are an error.
    pub fn decode_with_stamp(body: &[u8]) -> Result<(Self, Option<VersionStamp>), WireError> {
        let mut r = WireReader::new(body);
        let out = Self::decode_fields(&mut r)?;
        if r.is_empty() {
            return Ok((out, None));
        }
        let rest = r.take(r.remaining())?;
        match VersionStamp::from_tagged(rest)? {
            Some(stamp) => Ok((out, Some(stamp))),
            None => Err(WireError::Trailing(rest.len())),
        }
    }

    fn decode_fields(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            client_nonce: r.u64()?,
            reason: DenyReason::from_u8(r.u8()?)?,
            detail: r.str_u8(MAX_DENY_DETAIL_BYTES)?.to_owned(),
        })
    }
}

/// Why a connected peer went away.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DisconnectReason {
    /// The other side said goodbye.
    Left,
    /// Silent for longer than the timeout.
    Timeout,
    /// Removed by the host.
    Kicked,
    /// Removed and banned by the host.
    Banned,
    /// The server is closing.
    ServerShutdown,
    /// The peer broke the protocol (for example an oversized reliable message).
    ProtocolError,
    /// A net message failed validation with `Verdict::Kick` (spec 70 § 6.4).
    InvalidMessage,
    /// A net message could not be decoded (spec 70 § 6.4).
    MalformedPacket,
    /// The reliable send queue passed its bound (spec 70 § 6.5).
    ReliableOverflow,
    /// The peer kept flooding while slowed (spec 70 § 6.6).
    Flood,
    /// The session has no peer id left (spec 70 § 4).
    ServerFull,
    /// The join declared more local players than allowed (spec 70 § 4).
    TooManyLocalPlayers,
}

impl DisconnectReason {
    /// Wire byte.
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Timeout => 2,
            Self::Kicked => 3,
            Self::Banned => 4,
            Self::ServerShutdown => 5,
            Self::ProtocolError => 6,
            Self::InvalidMessage => 7,
            Self::MalformedPacket => 8,
            Self::ReliableOverflow => 9,
            Self::Flood => 10,
            Self::ServerFull => 11,
            Self::TooManyLocalPlayers => 12,
        }
    }

    /// From a wire byte; unknown bytes read as [`DisconnectReason::Left`].
    pub fn from_u8(v: u8) -> Self {
        match v {
            2 => Self::Timeout,
            3 => Self::Kicked,
            4 => Self::Banned,
            5 => Self::ServerShutdown,
            6 => Self::ProtocolError,
            7 => Self::InvalidMessage,
            8 => Self::MalformedPacket,
            9 => Self::ReliableOverflow,
            10 => Self::Flood,
            11 => Self::ServerFull,
            12 => Self::TooManyLocalPlayers,
            _ => Self::Left,
        }
    }

    /// Lowercase code for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Timeout => "timeout",
            Self::Kicked => "kicked",
            Self::Banned => "banned",
            Self::ServerShutdown => "server_shutdown",
            Self::ProtocolError => "protocol_error",
            Self::InvalidMessage => "invalid_message",
            Self::MalformedPacket => "malformed_packet",
            Self::ReliableOverflow => "reliable_overflow",
            Self::Flood => "flood",
            Self::ServerFull => "server_full",
            Self::TooManyLocalPlayers => "too_many_local_players",
        }
    }
}

/// Mints and checks stateless cookies and password nonces from a server secret.
#[derive(Clone)]
pub struct CookieMinter {
    secret: [u8; 32],
}

impl std::fmt::Debug for CookieMinter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CookieMinter { secret: <hidden> }")
    }
}

/// MAC domain of the IP-bound game handshake cookie.
const DOMAIN_COOKIE: u8 = 1;
/// MAC domain of the IP-bound password nonce.
const DOMAIN_PASSWORD_NONCE: u8 = 2;
// Domain 3 is `CookieMinter::derive_u32`.
/// MAC domain of the address-bound (IP and port) cookie, so a cookie of one binding never
/// verifies as the other.
const DOMAIN_ADDR_COOKIE: u8 = 4;

/// What part of the source address a MAC binds.
#[derive(Clone, Copy)]
enum Binding {
    /// The canonical IP only: the game handshake survives a NAT port change.
    Ip,
    /// The IP and the port, as [`encode_addr`] writes them: the relay keys lobbies by the exact
    /// address, so its proof must not cover the other ports of a shared IP.
    Addr,
}

impl CookieMinter {
    /// A minter with `secret`.
    pub fn new(secret: [u8; 32]) -> Self {
        Self { secret }
    }

    fn mac(
        &self,
        domain: u8,
        binding: Binding,
        addr: SocketAddr,
        client_nonce: u64,
        bucket: u64,
    ) -> [u8; 32] {
        let mut w = WireWriter::with_capacity(48);
        w.u8(domain);
        match binding {
            // NAT may reassign a port between Challenge and KeyInit. The proof binds the exact
            // canonical IP, nonce and time bucket; it does not authorize a neighboring IP.
            Binding::Ip => match addr.ip().to_canonical() {
                IpAddr::V4(ip) => {
                    w.u8(4);
                    w.raw(&ip.octets());
                }
                IpAddr::V6(ip) => {
                    w.u8(6);
                    w.raw(&ip.octets());
                }
            },
            Binding::Addr => encode_addr(&mut w, addr),
        }
        w.u64(client_nonce);
        w.u64(bucket);
        *blake3::keyed_hash(&self.secret, w.as_slice()).as_bytes()
    }

    fn bucket(now_ms: u64) -> u64 {
        now_ms / COOKIE_BUCKET_MS
    }

    /// The cookie of `domain` and `binding` valid now or one bucket ago, as the `now_ms` it was
    /// minted for.
    fn verify_with(
        &self,
        domain: u8,
        binding: Binding,
        addr: SocketAddr,
        client_nonce: u64,
        cookie: &[u8; 16],
        now_ms: u64,
    ) -> Option<u64> {
        let current = Self::bucket(now_ms);
        [current, current.saturating_sub(1)]
            .into_iter()
            .find_map(|bucket| {
                let expect = first16(&self.mac(domain, binding, addr, client_nonce, bucket));
                constant_time_eq(&expect, cookie).then_some(bucket * COOKIE_BUCKET_MS)
            })
    }

    /// The cookie for the canonical IP of `addr` and `client_nonce` at `now_ms`.
    /// Source ports are excluded so NAT port changes do not invalidate an outstanding proof.
    pub fn cookie(&self, addr: SocketAddr, client_nonce: u64, now_ms: u64) -> [u8; 16] {
        first16(&self.mac(
            DOMAIN_COOKIE,
            Binding::Ip,
            addr,
            client_nonce,
            Self::bucket(now_ms),
        ))
    }

    /// The password nonce paired with that cookie.
    pub fn password_nonce(&self, addr: SocketAddr, client_nonce: u64, now_ms: u64) -> [u8; 16] {
        first16(&self.mac(
            DOMAIN_PASSWORD_NONCE,
            Binding::Ip,
            addr,
            client_nonce,
            Self::bucket(now_ms),
        ))
    }

    /// If `cookie` is valid for this or the previous bucket, the `now_ms` it was minted for (so the
    /// matching password nonce can be recomputed).
    pub fn verify(&self, addr: SocketAddr, client_nonce: u64, cookie: &[u8; 16], now_ms: u64) -> Option<u64> {
        self.verify_with(DOMAIN_COOKIE, Binding::Ip, addr, client_nonce, cookie, now_ms)
    }

    /// The cookie for the exact `addr` (IP and port, as [`encode_addr`] writes them) and
    /// `client_nonce` at `now_ms`. For proofs that key state by the full source address (the relay's
    /// `Register`): another port of the same IP cannot reuse it. It is minted under its own MAC
    /// domain, so it never verifies as a [`CookieMinter::cookie`] and the reverse.
    pub fn cookie_for_addr(&self, addr: SocketAddr, client_nonce: u64, now_ms: u64) -> [u8; 16] {
        first16(&self.mac(
            DOMAIN_ADDR_COOKIE,
            Binding::Addr,
            addr,
            client_nonce,
            Self::bucket(now_ms),
        ))
    }

    /// [`CookieMinter::verify`] for a [`CookieMinter::cookie_for_addr`] cookie: valid only from the
    /// exact address it was minted for, in this or the previous bucket.
    pub fn verify_addr(
        &self,
        addr: SocketAddr,
        client_nonce: u64,
        cookie: &[u8; 16],
        now_ms: u64,
    ) -> Option<u64> {
        self.verify_with(
            DOMAIN_ADDR_COOKIE,
            Binding::Addr,
            addr,
            client_nonce,
            cookie,
            now_ms,
        )
    }

    /// A resume ticket derived from the secret (spec 120 § 10.3): unguessable without it, distinct
    /// per `(label, counter)`.
    pub fn derive_ticket(&self, label: u64, counter: u64) -> [u8; RESUME_TICKET_BYTES] {
        let mut w = WireWriter::with_capacity(17);
        w.u8(4);
        w.u64(label);
        w.u64(counter);
        first16(blake3::keyed_hash(&self.secret, w.as_slice()).as_bytes())
    }

    /// A 32-bit value derived from the secret, for session tokens.
    pub fn derive_u32(&self, label: u64, counter: u64) -> u32 {
        let mut w = WireWriter::with_capacity(17);
        w.u8(3);
        w.u64(label);
        w.u64(counter);
        let mac = blake3::keyed_hash(&self.secret, w.as_slice());
        let b = mac.as_bytes();
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }
}

fn first16(mac: &[u8; 32]) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&mac[..16]);
    out
}

/// Writes an address as `family u8 | ip (4 or 16 bytes) | port u16`.
pub fn encode_addr(w: &mut WireWriter, addr: SocketAddr) {
    match addr.ip() {
        IpAddr::V4(ip) => {
            w.u8(4);
            w.raw(&ip.octets());
        }
        IpAddr::V6(ip) => {
            w.u8(6);
            w.raw(&ip.octets());
        }
    }
    w.u16(addr.port());
}

/// Reads an address written by [`encode_addr`].
pub fn decode_addr(r: &mut WireReader<'_>) -> Result<SocketAddr, WireError> {
    let ip = match r.u8()? {
        4 => IpAddr::from(r.array::<4>()?),
        6 => IpAddr::from(r.array::<16>()?),
        _ => return Err(WireError::Invalid("address family")),
    };
    Ok(SocketAddr::new(ip, r.u16()?))
}

/// 32 bytes of secret material for a server: the operating system's random source plus OS-seeded
/// hasher keys, the process id, the clock and a counter, mixed through BLAKE3, so a failed random
/// source degrades instead of repeating. A server without a configured identity derives its key
/// from this; tests pass a fixed secret instead.
pub fn fresh_secret() -> [u8; 32] {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = blake3::Hasher::new_derive_key("regia_net server secret v1");
    match crate::crypto::os_random::<32>() {
        Some(bytes) => {
            h.update(&bytes);
        }
        None => tracing::warn!(target: "regia_net", "the system random source failed; using weaker seeds"),
    }
    for i in 0..4u64 {
        let mut s = std::collections::hash_map::RandomState::new().build_hasher();
        s.write_u64(i);
        h.update(&s.finish().to_le_bytes());
    }
    h.update(&u64::from(std::process::id()).to_le_bytes());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    h.update(&nanos.to_le_bytes());
    h.update(&COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    *h.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The version stamps stay invisible to builds that predate them (ADR-7985): an old server's
    /// decoder reads the same fields from a stamped request, an unstamped request carries no
    /// stamp, a stamped deny is smaller than the request it answers, and an old client's strict
    /// decoder refuses the trailer (so a server only stamps a deny when the request was stamped).
    #[test]
    fn version_stamps_are_invisible_to_older_builds() {
        let request = ConnectRequest {
            protocol_id: 0x1122_3344_5566_7788,
            client_nonce: 9,
            crypto_version: 1,
            crypto_required: true,
        };
        let stamp = VersionStamp::new(3, &"x".repeat(40), 7, u64::MAX);
        let stamped = request.to_datagram_with_stamp(&stamp);
        assert_eq!(stamped.len(), CONNECT_REQUEST_BYTES);
        let (kind, body) = split_datagram(&stamped).expect("regia traffic");
        assert_eq!(kind, PacketKind::ConnectRequest);
        assert_eq!(ConnectRequest::decode(body), Ok(request));
        assert_eq!(ConnectRequest::stamp_of(body), Some(stamp.clone()));
        let plain = request.to_datagram();
        let (_, plain_body) = split_datagram(&plain).expect("regia traffic");
        assert_eq!(ConnectRequest::stamp_of(plain_body), None);

        let deny = Deny {
            client_nonce: 9,
            reason: DenyReason::ProtocolMismatch,
            detail: "d".repeat(400),
        };
        let stamped_deny = deny.to_datagram_with_stamp(&stamp);
        assert!(stamped_deny.len() < stamped.len(), "{} bytes", stamped_deny.len());
        let (_, deny_body) = split_datagram(&stamped_deny).expect("regia traffic");
        assert!(
            Deny::decode(deny_body).is_err(),
            "an old client refuses the trailer"
        );
        let (back, back_stamp) = Deny::decode_with_stamp(deny_body).expect("stamped deny");
        assert_eq!(back.detail.len(), MAX_STAMPED_DENY_DETAIL_BYTES);
        assert_eq!(back_stamp, Some(stamp));
        let (_, plain_deny) = split_datagram(&deny.to_datagram())
            .map(|(k, b)| (k, b.to_vec()))
            .expect("deny");
        assert_eq!(Deny::decode_with_stamp(&plain_deny).map(|(_, s)| s), Ok(None));
    }

    fn addr() -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 2], 40000))
    }

    #[test]
    fn padding_reads_as_a_plain_offer() {
        // A request whose crypto bytes are padding offers no suite.
        let mut dg = ConnectRequest {
            protocol_id: 1,
            client_nonce: 2,
            crypto_version: 0,
            crypto_required: false,
        }
        .to_datagram();
        dg[5 + 16..].fill(0);
        let req = ConnectRequest::decode(&dg[5..]).expect("padded");
        assert_eq!((req.crypto_version, req.crypto_required), (0, false));
    }

    #[test]
    fn messages_round_trip() {
        let req = ConnectRequest {
            protocol_id: 0xAB,
            client_nonce: 7,
            crypto_version: 1,
            crypto_required: true,
        };
        let dg = req.to_datagram();
        assert_eq!(dg.len(), CONNECT_REQUEST_BYTES);
        let (kind, body) = split_datagram(&dg).expect("ours");
        assert_eq!(kind, PacketKind::ConnectRequest);
        assert_eq!(ConnectRequest::decode(body), Ok(req));

        let ch = Challenge {
            client_nonce: 7,
            cookie: [1; 16],
            password: Some(PasswordChallenge {
                salt: [2; 16],
                nonce: [3; 16],
            }),
            crypto_version: 1,
        };
        let dg = ch.to_datagram();
        assert!(dg.len() <= CONNECT_REQUEST_BYTES, "no amplification");
        assert_eq!(Challenge::decode(split_datagram(&dg).expect("ours").1), Ok(ch));

        let resp = ChallengeResponse {
            client_nonce: 7,
            cookie: [1; 16],
            token: JoinToken {
                name: "ada".into(),
                role: PeerRole::Spectator,
                build: "1.0-abc".into(),
                password_proof: [5; 32],
                join_data: vec![9, 9],
                resume: Some([4; RESUME_TICKET_BYTES]),
            },
        };
        let dg = resp.to_datagram().expect("fits");
        assert_eq!(
            ChallengeResponse::decode(split_datagram(&dg).expect("ours").1),
            Ok(resp)
        );

        let acc = Accept {
            client_nonce: 7,
            session_token: 0xDEAD,
            peer_id: 3,
            tick_rate_hz: 30,
            server_tick: 99,
            resume_ticket: [8; RESUME_TICKET_BYTES],
            resumption: None,
        };
        assert_eq!(
            Accept::decode(split_datagram(&acc.to_datagram()).expect("ours").1),
            Ok(acc)
        );
        // Wire 6 (ADR-8858): the resumption ticket rides after a flag byte; no other flag value is read.
        let with_ticket = Accept {
            resumption: Some([9; RESUMPTION_TICKET_BYTES]),
            ..acc
        };
        let body = with_ticket.to_body();
        assert_eq!(body.len(), MAX_ACCEPT_BODY_BYTES);
        assert_eq!(
            acc.to_body().len(),
            MAX_ACCEPT_BODY_BYTES - RESUMPTION_TICKET_BYTES
        );
        assert_eq!(Accept::decode(&body), Ok(with_ticket));
        assert!(Accept::decode(&body[..body.len() - 1]).is_err(), "a cut ticket");
        let mut bad_flag = acc.to_body();
        let flag = bad_flag.len() - 1;
        bad_flag[flag] = 2;
        assert!(Accept::decode(&bad_flag).is_err());
        for (byte, kind) in [
            (11, PacketKind::ResumeInit),
            (12, PacketKind::ResumeReply),
            (13, PacketKind::ResumeRetry),
        ] {
            assert_eq!(PacketKind::from_u8(byte), Some(kind));
            assert_eq!(kind as u8, byte);
        }
        assert_eq!(PacketKind::from_u8(14), None);

        let deny = Deny {
            client_nonce: 7,
            reason: DenyReason::BuildMismatch,
            detail: "x".repeat(500),
        };
        let back = Deny::decode(split_datagram(&deny.to_datagram()).expect("ours").1).expect("valid");
        assert_eq!(back.reason, DenyReason::BuildMismatch);
        assert_eq!(
            back.detail.len(),
            MAX_DENY_DETAIL_BYTES,
            "long detail is cut, not refused"
        );
    }

    #[test]
    fn foreign_and_short_datagrams_are_rejected() {
        assert!(split_datagram(b"HTTP/1.1 200").is_none());
        assert!(split_datagram(&[]).is_none());
        let mut short = ConnectRequest {
            protocol_id: 1,
            client_nonce: 1,
            crypto_version: 0,
            crypto_required: false,
        }
        .to_datagram();
        short.truncate(40);
        assert!(
            ConnectRequest::decode(&short[5..]).is_err(),
            "unpadded request refused"
        );
        let resp = ChallengeResponse {
            client_nonce: 1,
            cookie: [0; 16],
            token: JoinToken {
                name: "n".repeat(MAX_NAME_BYTES + 1),
                role: PeerRole::Player,
                build: String::new(),
                password_proof: [0; 32],
                join_data: Vec::new(),
                resume: None,
            },
        };
        assert!(resp.to_datagram().is_err(), "over-long name refused at encode");
    }

    /// Spec 120 § 10.3: the join token carries an optional resume ticket behind a strict flag,
    /// and tickets derived from the secret differ per peer and per mint.
    #[test]
    fn the_resume_ticket_round_trips_and_a_bad_flag_is_refused() {
        let token = JoinToken {
            name: "p".into(),
            role: PeerRole::Player,
            build: String::new(),
            password_proof: [0; 32],
            join_data: Vec::new(),
            resume: None,
        };
        let mut w = WireWriter::new();
        token.encode(&mut w).expect("fits");
        let mut bytes = w.finish();
        assert_eq!(bytes.last(), Some(&0), "no ticket: one flag byte");
        assert_eq!(JoinToken::decode(&mut WireReader::new(&bytes)), Ok(token));
        if let Some(flag) = bytes.last_mut() {
            *flag = 2;
        }
        assert!(
            JoinToken::decode(&mut WireReader::new(&bytes)).is_err(),
            "flag 2 refused"
        );
        if let Some(flag) = bytes.last_mut() {
            *flag = 1;
        }
        assert!(
            JoinToken::decode(&mut WireReader::new(&bytes)).is_err(),
            "flag 1 without 16 bytes refused"
        );
        let m = CookieMinter::new([7; 32]);
        let a = m.derive_ticket(1, 1);
        assert_ne!(a, m.derive_ticket(1, 2), "a new ticket per mint");
        assert_ne!(a, m.derive_ticket(2, 1), "a different ticket per peer");
        assert_ne!(a, CookieMinter::new([8; 32]).derive_ticket(1, 1), "secret-bound");
        assert_ne!(a, [0; RESUME_TICKET_BYTES]);
    }

    #[test]
    fn cookies_bind_address_nonce_and_time() {
        let m = CookieMinter::new([7; 32]);
        let c = m.cookie(addr(), 1, 25_000);
        assert_eq!(m.verify(addr(), 1, &c, 25_000), Some(20_000));
        assert_eq!(
            m.verify(addr(), 1, &c, 34_999),
            Some(20_000),
            "previous bucket still valid"
        );
        assert_eq!(
            m.verify(addr(), 1, &c, 40_000),
            None,
            "two buckets later it is dead"
        );
        assert_eq!(m.verify(addr(), 2, &c, 25_000), None, "other nonce");
        let other = SocketAddr::from(([10, 0, 0, 3], 40000));
        assert_eq!(m.verify(other, 1, &c, 25_000), None, "other address");
        assert_eq!(
            CookieMinter::new([8; 32]).verify(addr(), 1, &c, 25_000),
            None,
            "other secret"
        );
        assert_ne!(fresh_secret(), fresh_secret());
    }

    #[test]
    fn cookies_and_password_nonces_bind_ip_but_not_port() {
        let minter = CookieMinter::new([9; 32]);
        for ip in ["10.0.0.1", "2001:db8::1"] {
            let ip: IpAddr = ip.parse().expect("IP");
            let a = SocketAddr::new(ip, 1000);
            let b = SocketAddr::new(ip, 2000);
            let cookie = minter.cookie(a, 17, 100);
            assert_eq!(minter.verify(b, 17, &cookie, 100), Some(0));
            assert_eq!(
                minter.password_nonce(a, 17, 100),
                minter.password_nonce(b, 17, 100)
            );
            let other: SocketAddr = if ip.is_ipv4() {
                "10.0.0.2:2000"
            } else {
                "[2001:db8::2]:2000"
            }
            .parse()
            .expect("other IP");
            assert_eq!(minter.verify(other, 17, &cookie, 100), None);
        }
        let a = "10.0.0.1:1000".parse().expect("v4");
        let mapped = "[::ffff:10.0.0.1]:2000".parse().expect("mapped");
        assert_eq!(minter.cookie(a, 17, 100), minter.cookie(mapped, 17, 100));
    }

    /// Review row 19: the relay's address-bound cookie binds the port too, and neither kind of
    /// cookie verifies as the other.
    #[test]
    fn address_bound_cookies_bind_the_port_and_never_pass_as_ip_cookies() {
        let minter = CookieMinter::new([9; 32]);
        for ip in ["10.0.0.1", "2001:db8::1"] {
            let ip: IpAddr = ip.parse().expect("IP");
            let a = SocketAddr::new(ip, 1000);
            let b = SocketAddr::new(ip, 2000);
            let bound = minter.cookie_for_addr(a, 0, 100);
            assert_ne!(bound, minter.cookie_for_addr(b, 0, 100), "the port is bound");
            assert_eq!(minter.verify_addr(a, 0, &bound, 100), Some(0));
            assert_eq!(
                minter.verify_addr(a, 0, &bound, 19_999),
                Some(0),
                "previous bucket"
            );
            assert_eq!(minter.verify_addr(a, 0, &bound, 20_000), None, "expired");
            assert_eq!(minter.verify_addr(b, 0, &bound, 100), None, "another port");
            assert_eq!(minter.verify_addr(a, 1, &bound, 100), None, "another nonce");
            // The IP-bound cookie stays the same across ports.
            assert_eq!(minter.cookie(a, 0, 100), minter.cookie(b, 0, 100));
            // Separate MAC domains: no cross-use in either direction.
            let ip_cookie = minter.cookie(a, 0, 100);
            assert_ne!(bound, ip_cookie);
            assert_eq!(minter.verify(a, 0, &bound, 100), None);
            assert_eq!(minter.verify_addr(a, 0, &ip_cookie, 100), None);
            assert_eq!(
                minter.verify_addr(a, 0, &minter.password_nonce(a, 0, 100), 100),
                None
            );
        }
    }

    #[test]
    fn addresses_round_trip_v4_and_v6() {
        for a in [addr(), SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 9))] {
            let mut w = WireWriter::new();
            encode_addr(&mut w, a);
            let bytes = w.finish();
            assert_eq!(decode_addr(&mut WireReader::new(&bytes)), Ok(a));
        }
    }
}
