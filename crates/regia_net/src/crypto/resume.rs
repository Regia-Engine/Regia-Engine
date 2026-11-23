// SPDX-License-Identifier: MPL-2.0
//! One-round-trip ticket resume (spec 120 § 10.3; ADR-8733 (3) as amended by ADR-8858), wire 6.
//!
//! A sealed `Accept` carries a **resumption ticket**: an opaque value only the server that
//! sealed it can read ([`TicketKeys`], the stateless ticket of RFC 5077 § 4). It names the peer,
//! the peer's epoch and the address block the server proved, and it holds the resumption PSK both
//! ends derived from the link's handshake ([`derive_resume_psk`], label [`RESUME_PSK_LABEL`]). The
//! client keeps the ticket and the PSK as a [`ResumeCredential`].
//!
//! ```text
//! client                                         server
//!   ResumeInit(ticket, psk, e, hello)      ──▶     ticket opened, message 1 opened: no curve work
//!   ◀── ResumeReply(e, ee, token; sealed Accept)   the link keys exist; nothing is admitted yet
//!   Sealed(first record)                   ──▶     key confirmation: now the peer is resumed
//! ```
//!
//! The handshake is [`RESUME_NOISE_PATTERN`] through `snow` (Noise revision 34 § 9, the PSK
//! modifier): under `psk0` the AEAD tag of message 1 is the proof of identity, so the first
//! flight carries the credential, the proof and the key share. Message 1 carries a
//! [`ResumeHello`] only: no input and no reliable message, so a replay delivers nothing (the
//! 0-RTT replay hazard of RFC 8446 § 8 does not arise). The link keys are the Noise split of
//! `psk` and `ee`: forward secret.
//!
//! A server that refuses the resume answers a plain `Challenge` and the client carries on with
//! the full handshake from it ([`ResumeRefusal`] names why, on the server's side). A source
//! outside the ticket's address block gets a [`ResumeRetry`] with a cookie instead.
//!
//! Like the rest of `crypto`, this holds no primitive of its own: the handshake and the ticket's
//! AEAD are `snow`'s, the key derivations are `blake3`'s.

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use snow::params::NoiseParams;
use snow::{Builder, HandshakeState};

use super::link::SecureLink;
use super::rekey::Key;
use super::{CryptoError, SEALED_HEADER_BYTES, ServerKey};
use crate::addr_block::AddrBlock;
use crate::handshake::{
    ConnectRequest, JoinToken, MAX_ACCEPT_BODY_BYTES, MAX_BUILD_BYTES, MAX_JOIN_DATA_BYTES, MAX_NAME_BYTES,
    PacketKind, PeerRole, RESUME_TICKET_BYTES, RESUMPTION_TICKET_BYTES, begin,
};
use crate::version_probe::{MAX_STAMP_BYTES, VersionStamp};
use crate::wire::{WireError, WireReader, WireWriter, constant_time_eq, fnv1a_64};

/// The Noise protocol name of a resume handshake.
pub const RESUME_NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// The `blake3` derive-key context of the resumption PSK.
pub const RESUME_PSK_LABEL: &str = "regia resume psk";
/// A `ResumeInit` is padded to this size: no answer to it is larger, and it is as large as the
/// first flight of a path that must carry 1,200-byte datagrams (RFC 9000 § 8.1, § 14.1).
pub const RESUME_INIT_BYTES: usize = 1200;
/// The version byte of a resumption ticket.
pub const RESUMPTION_TICKET_VERSION: u8 = 1;
/// `ResumeInit` sends without any answer after which the client also sends its `ConnectRequest`
/// with every resend (ADR-8858 A8): a server that predates wire 6 drops the resume kind unread.
pub const RESUME_INIT_UNANSWERED_SENDS: u32 = 3;
/// Bytes of the second Noise message: `e` (32), the session token and its tag (4 + 16).
pub const RESUME_REPLY_NOISE_BYTES: usize = 32 + 4 + TAG_BYTES;
/// Bytes of a `ResumeInit`'s fields after the kind byte, up to the Noise message.
pub const RESUME_INIT_FIELD_BYTES: usize = 8 + 8 + 1 + 1 + 16 + RESUMPTION_TICKET_BYTES + 2;
/// The largest encoded [`ResumeHello`].
pub const MAX_RESUME_HELLO_BYTES: usize =
    4 + (1 + MAX_NAME_BYTES) + 1 + (1 + MAX_BUILD_BYTES) + (2 + MAX_JOIN_DATA_BYTES);
/// The smallest encoded [`ResumeHello`] (empty name, build and join data).
const MIN_RESUME_HELLO_BYTES: usize = 4 + 1 + 1 + 1 + 2;
/// The largest first Noise message: `e`, the sealed hello.
pub const MAX_RESUME_NOISE_BYTES: usize = 32 + MAX_RESUME_HELLO_BYTES + TAG_BYTES;
const MIN_RESUME_NOISE_BYTES: usize = 32 + MIN_RESUME_HELLO_BYTES + TAG_BYTES;
/// The largest sealed `Accept` record a `ResumeReply` carries.
pub const MAX_RESUME_ACCEPT_RECORD_BYTES: usize = SEALED_HEADER_BYTES + 1 + MAX_ACCEPT_BODY_BYTES + TAG_BYTES;
const MIN_RESUME_ACCEPT_RECORD_BYTES: usize = SEALED_HEADER_BYTES + 1 + TAG_BYTES;

const TAG_BYTES: usize = 16;
const TICKET_NONCE_BYTES: usize = 16;
const TICKET_HEADER_BYTES: usize = 1 + 4 + TICKET_NONCE_BYTES;
const CLAIMS_BYTES: usize = 4 + 4 + 32 + 16 + 8 + 8 + 8 + 8 + 1 + RESUME_TICKET_BYTES;
const FLAG_PASSWORD_VERIFIED: u8 = 1;

const _: () = assert!(TICKET_HEADER_BYTES + CLAIMS_BYTES + TAG_BYTES == RESUMPTION_TICKET_BYTES);
// The largest `ResumeInit`, version stamp included, fits its padded size.
const _: () =
    assert!(5 + RESUME_INIT_FIELD_BYTES + MAX_RESUME_NOISE_BYTES + MAX_STAMP_BYTES <= RESUME_INIT_BYTES);

/// Why a server did not resume a `ResumeInit` (ADR-8858). Every refusal but
/// [`ResumeRefusal::OtherBlock`] (answered [`ResumeRetry`]), [`ResumeRefusal::Psk`] and
/// [`ResumeRefusal::StaleFlight`] (no answer) sends the client to the full handshake with a
/// plain `Challenge`; the code is the server's own record of why (logs, tests, the agent
/// protocol's `net.status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ResumeRefusal {
    /// The ticket or the hello has the wrong size, version or shape.
    Malformed,
    /// The ticket was sealed under a key this server no longer keeps (older than the previous
    /// one).
    KeyRetired,
    /// The ticket names a key id this server has not reached: another server's ticket.
    KeyUnknown,
    /// The ticket did not open under its key: another server's ticket, or a changed byte.
    TicketAuth,
    /// The ticket's lifetime (one resume grace) is over.
    Expired,
    /// The ticket was sealed for another protocol id or build.
    Protocol,
    /// The ticket's epoch is outside its peer's window: an accept the client confirmed since
    /// made it single use (ADR-8858 A2).
    Epoch,
    /// The source is outside the ticket's address block and brought no valid cookie.
    OtherBlock,
    /// The first Noise message did not open under the ticket's PSK.
    Psk,
    /// The ticket names no dormant or live peer and no held slot.
    NoPeer,
    /// The server's resume register is full, or the flight is over its budget.
    Busy,
    /// This server does not admit this resume: it keeps no dormant peers, does not seal, is
    /// closing, bans the source, or asks for a password the ticket does not vouch for.
    Unavailable,
    /// The flight's number is at or below the highest this server has seen with the ticket and
    /// matches no pending resume: a replay of an older flight (ADR-8858 A3). No answer.
    StaleFlight,
}

impl ResumeRefusal {
    /// Every refusal, in declaration order.
    pub const ALL: [Self; 13] = [
        Self::Malformed,
        Self::KeyRetired,
        Self::KeyUnknown,
        Self::TicketAuth,
        Self::Expired,
        Self::Protocol,
        Self::Epoch,
        Self::OtherBlock,
        Self::Psk,
        Self::NoPeer,
        Self::Busy,
        Self::Unavailable,
        Self::StaleFlight,
    ];

    /// This refusal's place in [`ResumeRefusal::ALL`].
    pub fn index(self) -> usize {
        self as usize
    }

    /// Lowercase code for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Malformed => "resume_malformed",
            Self::KeyRetired => "resume_key_retired",
            Self::KeyUnknown => "resume_key_unknown",
            Self::TicketAuth => "resume_ticket_auth",
            Self::Expired => "resume_expired",
            Self::Protocol => "resume_protocol",
            Self::Epoch => "resume_epoch",
            Self::OtherBlock => "resume_other_block",
            Self::Psk => "resume_psk",
            Self::NoPeer => "resume_no_peer",
            Self::Busy => "resume_busy",
            Self::Unavailable => "resume_unavailable",
            Self::StaleFlight => "resume_stale_flight",
        }
    }
}

impl std::fmt::Display for ResumeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::error::Error for ResumeRefusal {}

/// The resumption PSK of a finished handshake (ADR-8858 A7): a one-way function of its handshake
/// hash and its two transport keys (`snow` does not expose the chaining key; the split keys are
/// its only outputs). Both ends compute it; it is never sent.
pub fn derive_resume_psk(
    handshake_hash: &[u8; 32],
    to_responder: &[u8; 32],
    to_initiator: &[u8; 32],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key(RESUME_PSK_LABEL);
    h.update(handshake_hash);
    h.update(to_responder);
    h.update(to_initiator);
    *h.finalize().as_bytes()
}

/// The hash of a build id a ticket carries instead of the text.
pub fn build_hash(build: &str) -> u64 {
    fnv1a_64(build.as_bytes())
}

/// What a resumption ticket says, readable only by the server that sealed it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct TicketClaims {
    /// The peer the ticket resumes (`PeerId`).
    pub peer: u32,
    /// The peer's epoch when the ticket was minted ([`ResumeEpochs`]).
    pub epoch: u32,
    /// The resumption PSK of the link whose accept carried the ticket.
    pub psk: [u8; 32],
    /// The address block the server had proved for the client (`AddrBlock::key_bytes`).
    pub block: [u8; 16],
    /// When the ticket was minted, on the server's clock, in milliseconds.
    pub issued_ms: u64,
    /// When it stops opening, on the same clock (one resume grace after `issued_ms`).
    pub expires_ms: u64,
    /// The protocol id of the link.
    pub protocol_id: u64,
    /// [`build_hash`] of the build id the join presented.
    pub build_hash: u64,
    /// The join passed the password gate (a resume presents no proof).
    pub password_verified: bool,
    /// The 16-byte resume ticket minted in the same accept: it names the dormant or live peer's
    /// ticket chain, or the address hold of a player that left on purpose.
    pub resume_id: [u8; RESUME_TICKET_BYTES],
}

impl std::fmt::Debug for TicketClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketClaims")
            .field("peer", &self.peer)
            .field("epoch", &self.epoch)
            .field("issued_ms", &self.issued_ms)
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

impl TicketClaims {
    /// True when `ip` lies in the address block the ticket was minted for (one IPv4 address, one
    /// IPv6 /64): such a source needs no cookie (the address token of RFC 9000 § 8.1.3).
    pub fn in_block(&self, ip: IpAddr) -> bool {
        AddrBlock::of(ip).key_bytes() == self.block
    }

    /// Refuses a ticket sealed for another protocol id or build.
    pub fn check_protocol(&self, protocol_id: u64, build: &str) -> Result<(), ResumeRefusal> {
        if self.protocol_id == protocol_id && self.build_hash == build_hash(build) {
            Ok(())
        } else {
            Err(ResumeRefusal::Protocol)
        }
    }

    fn encode(&self) -> [u8; CLAIMS_BYTES] {
        let mut w = WireWriter::with_capacity(CLAIMS_BYTES);
        w.u32(self.peer);
        w.u32(self.epoch);
        w.raw(&self.psk);
        w.raw(&self.block);
        w.u64(self.issued_ms);
        w.u64(self.expires_ms);
        w.u64(self.protocol_id);
        w.u64(self.build_hash);
        w.u8(if self.password_verified {
            FLAG_PASSWORD_VERIFIED
        } else {
            0
        });
        w.raw(&self.resume_id);
        let mut out = [0u8; CLAIMS_BYTES];
        out.copy_from_slice(w.as_slice());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(bytes);
        let out = Self {
            peer: r.u32()?,
            epoch: r.u32()?,
            psk: r.array()?,
            block: r.array()?,
            issued_ms: r.u64()?,
            expires_ms: r.u64()?,
            protocol_id: r.u64()?,
            build_hash: r.u64()?,
            password_verified: match r.u8()? {
                0 => false,
                FLAG_PASSWORD_VERIFIED => true,
                _ => return Err(WireError::Invalid("ticket flags")),
            },
            resume_id: r.array()?,
        };
        r.finish()?;
        Ok(out)
    }
}

/// The server's ticket keys: two live at any time, the current one and the one before it,
/// rotated every period (the resume grace), with no state per ticket and none per key.
///
/// The key id is the period's number on the server's clock, plus one; a key is a keyed hash of
/// the server secret and its id. A ticket lives one period, so the key that sealed it is still
/// the current or the previous key for the ticket's whole life. Each ticket is sealed under its
/// own key (a keyed hash of the ticket key over the ticket's clear header), so a changed header
/// byte reaches another key and fails the tag, and no nonce is used twice under one key.
#[derive(Clone)]
pub struct TicketKeys {
    root: [u8; 32],
    period_ms: u64,
    sealed: u64,
}

impl std::fmt::Debug for TicketKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TicketKeys")
            .field("period_ms", &self.period_ms)
            .finish_non_exhaustive()
    }
}

impl TicketKeys {
    /// The keys of the server with `secret`, rotated every `period_ms` (at least 1).
    pub fn new(secret: &[u8; 32], period_ms: u64) -> Self {
        Self {
            root: *blake3::keyed_hash(secret, b"regia_net resumption ticket keys v1").as_bytes(),
            period_ms: period_ms.max(1),
            sealed: 0,
        }
    }

    /// The id of the key that seals at `now_ms`.
    pub fn current_id(&self, now_ms: u64) -> u32 {
        u32::try_from(now_ms / self.period_ms)
            .unwrap_or(u32::MAX)
            .min(u32::MAX - 1)
            + 1
    }

    fn ticket_key(&self, id: u32, header: &[u8]) -> Result<Key, CryptoError> {
        let mut input = [0u8; 5];
        input[0] = b'k';
        input[1..].copy_from_slice(&id.to_le_bytes());
        let key = blake3::keyed_hash(&self.root, &input);
        Key::new(*blake3::keyed_hash(key.as_bytes(), header).as_bytes())
    }

    /// Seals `claims` under the key current at `now_ms`.
    pub fn seal(
        &mut self,
        claims: &TicketClaims,
        now_ms: u64,
    ) -> Result<[u8; RESUMPTION_TICKET_BYTES], CryptoError> {
        let id = self.current_id(now_ms);
        let mut input = [0u8; 13];
        input[0] = b'n';
        input[1..5].copy_from_slice(&id.to_le_bytes());
        input[5..].copy_from_slice(&self.sealed.to_le_bytes());
        self.sealed = self.sealed.wrapping_add(1);
        let nonce = blake3::keyed_hash(&self.root, &input);
        let mut out = [0u8; RESUMPTION_TICKET_BYTES];
        let (header, sealed) = out.split_at_mut(TICKET_HEADER_BYTES);
        header[0] = RESUMPTION_TICKET_VERSION;
        header[1..5].copy_from_slice(&id.to_le_bytes());
        header[5..].copy_from_slice(&nonce.as_bytes()[..TICKET_NONCE_BYTES]);
        self.ticket_key(id, header)?.seal(0, &claims.encode(), sealed)?;
        Ok(out)
    }

    /// Opens a ticket at `now_ms`: the size and version, the key id, the tag, then the expiry.
    /// One AEAD, no curve work and no state.
    pub fn open(&self, ticket: &[u8], now_ms: u64) -> Result<TicketClaims, ResumeRefusal> {
        if ticket.len() != RESUMPTION_TICKET_BYTES || ticket[0] != RESUMPTION_TICKET_VERSION {
            return Err(ResumeRefusal::Malformed);
        }
        let (header, sealed) = ticket.split_at(TICKET_HEADER_BYTES);
        let id = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
        let current = self.current_id(now_ms);
        if id > current {
            return Err(ResumeRefusal::KeyUnknown);
        }
        if current - id > 1 || id == 0 {
            return Err(ResumeRefusal::KeyRetired);
        }
        let key = self
            .ticket_key(id, header)
            .map_err(|_| ResumeRefusal::Unavailable)?;
        let mut plain = [0u8; CLAIMS_BYTES + TAG_BYTES];
        let n = key
            .open(0, sealed, &mut plain)
            .map_err(|_| ResumeRefusal::TicketAuth)?;
        let claims = TicketClaims::decode(&plain[..n]).map_err(|_| ResumeRefusal::Malformed)?;
        if now_ms >= claims.expires_ms {
            return Err(ResumeRefusal::Expired);
        }
        Ok(claims)
    }
}

/// A peer's two resume epochs (ADR-8858 A2): what makes a stateless ticket single use.
///
/// Every accept the server sends a peer mints the next epoch into its resumption ticket. The
/// client's first authenticated packet on a connection confirms that connection's accept; from
/// then on every ticket of an older epoch is refused from every address ([`ResumeRefusal::Epoch`]).
/// Until then the older ticket still names the peer (ADR-8535: a ticket is spent when the client
/// proves it holds the newer one).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResumeEpochs {
    confirmed: u32,
    minted: u32,
}

impl ResumeEpochs {
    /// The epoch of the newest accept the client proved it holds (0: none yet).
    pub fn confirmed(&self) -> u32 {
        self.confirmed
    }

    /// The epoch of the newest ticket minted for the peer (0: none yet).
    pub fn minted(&self) -> u32 {
        self.minted
    }

    /// The epoch for the next ticket. `None` when the counter is used up: the accept then carries
    /// no resumption ticket, and the peer resumes through the full handshake.
    pub fn mint(&mut self) -> Option<u32> {
        let next = self.minted.checked_add(1)?;
        self.minted = next;
        Some(next)
    }

    /// The client proved it holds the accept whose ticket has `epoch`. An epoch never minted, or
    /// one older than the confirmed one, changes nothing.
    pub fn confirm(&mut self, epoch: u32) {
        if epoch <= self.minted && epoch > self.confirmed {
            self.confirmed = epoch;
        }
    }

    /// Whether a ticket of `epoch` still names the peer.
    pub fn check(&self, epoch: u32) -> Result<(), ResumeRefusal> {
        if epoch != 0 && epoch >= self.confirmed && epoch <= self.minted {
            Ok(())
        } else {
            Err(ResumeRefusal::Epoch)
        }
    }
}

/// What a client keeps to resume in one round trip: the resumption ticket of its last accept, the
/// PSK of that link, the server key that link proved, and a count of the flights made with it.
///
/// Clones share the flight count, so every flight made with one credential has a higher number
/// than the last, whoever made it (ADR-8858 A3). The PSK is a secret: this type is never
/// serialized and never printed.
#[derive(Clone)]
pub struct ResumeCredential {
    ticket: [u8; RESUMPTION_TICKET_BYTES],
    psk: [u8; 32],
    server_key: Option<ServerKey>,
    flights: Arc<AtomicU32>,
}

impl ResumeCredential {
    /// The credential of an accept: its ticket, the link's PSK ([`SecureLink::resume_psk`]) and
    /// the key the server proved on that link.
    pub fn new(ticket: [u8; RESUMPTION_TICKET_BYTES], psk: [u8; 32], server_key: Option<ServerKey>) -> Self {
        Self {
            ticket,
            psk,
            server_key,
            flights: Arc::new(AtomicU32::new(0)),
        }
    }

    /// The opaque ticket.
    pub fn ticket(&self) -> &[u8; RESUMPTION_TICKET_BYTES] {
        &self.ticket
    }

    /// The key the server proved on the link this credential came from.
    pub fn server_key(&self) -> Option<ServerKey> {
        self.server_key
    }

    /// Flights made with this credential so far.
    pub fn flights(&self) -> u32 {
        self.flights.load(Ordering::Relaxed)
    }

    fn next_flight(&self) -> u32 {
        self.flights.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }
}

impl PartialEq for ResumeCredential {
    fn eq(&self, other: &Self) -> bool {
        self.ticket == other.ticket
            && constant_time_eq(&self.psk, &other.psk)
            && self.server_key == other.server_key
    }
}

impl Eq for ResumeCredential {}

impl std::fmt::Debug for ResumeCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeCredential")
            .field("server_key", &self.server_key)
            .field("flights", &self.flights())
            .finish_non_exhaustive()
    }
}

/// The payload of the first Noise message: who comes back. No password proof (the ticket says the
/// password was verified), no input, no reliable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeHello {
    /// The flight's number: higher for every flight a client makes with one credential. The
    /// server orders flights by it, never by arrival (ADR-8858 A3).
    pub flight: u32,
    /// Display name, at most [`MAX_NAME_BYTES`].
    pub name: String,
    /// Player or spectator.
    pub role: PeerRole,
    /// The game's build id.
    pub build: String,
    /// The game's encoded `NetGame::JoinData`.
    pub join_data: Vec<u8>,
}

impl ResumeHello {
    /// Appends the hello; fails when a field exceeds its limit.
    pub fn encode(&self, w: &mut WireWriter) -> Result<(), WireError> {
        w.u32(self.flight);
        w.str_u8(&self.name, MAX_NAME_BYTES)?;
        w.u8(self.role.to_u8());
        w.str_u8(&self.build, MAX_BUILD_BYTES)?;
        w.bytes_u16(&self.join_data, MAX_JOIN_DATA_BYTES)
    }

    /// Reads a whole hello; trailing bytes are an error.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(bytes);
        let out = Self {
            flight: r.u32()?,
            name: r.str_u8(MAX_NAME_BYTES)?.to_owned(),
            role: PeerRole::from_u8(r.u8()?)?,
            build: r.str_u8(MAX_BUILD_BYTES)?.to_owned(),
            join_data: r.bytes_u16(MAX_JOIN_DATA_BYTES)?.to_vec(),
        };
        r.finish()?;
        Ok(out)
    }

    /// The join token the server's admission checks read: this hello, no password proof, and the
    /// resume ticket id the resumption ticket named.
    pub fn into_join_token(self, resume_id: [u8; RESUME_TICKET_BYTES]) -> JoinToken {
        JoinToken {
            name: self.name,
            role: self.role,
            build: self.build,
            password_proof: [0; 32],
            join_data: self.join_data,
            resume: Some(resume_id),
        }
    }
}

/// The prologue of a resume handshake: the plain fields both sides saw, so a change to any of
/// them fails message 1 (Noise revision 34 § 6). The cookie is not in it: a `ResumeInit` resent
/// with a cookie after a [`ResumeRetry`] is the same Noise message and the same flight.
pub fn resume_prologue(
    protocol_id: u64,
    client_nonce: u64,
    version: u8,
    ticket: &[u8; RESUMPTION_TICKET_BYTES],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new_derive_key("regia_net resume prologue v1");
    h.update(&protocol_id.to_le_bytes());
    h.update(&client_nonce.to_le_bytes());
    h.update(&[version]);
    h.update(ticket);
    *h.finalize().as_bytes()
}

fn params() -> Result<NoiseParams, CryptoError> {
    RESUME_NOISE_PATTERN.parse().map_err(|_| CryptoError::Unavailable)
}

/// The client's side of a resume: one flight.
pub struct ResumeFlight {
    state: HandshakeState,
    message: Vec<u8>,
    flight: u32,
    token: Option<u32>,
}

impl std::fmt::Debug for ResumeFlight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeFlight")
            .field("flight", &self.flight)
            .finish_non_exhaustive()
    }
}

impl ResumeFlight {
    /// Starts a flight and writes the first Noise message (`psk, e`, the sealed hello).
    /// `hello.flight` is replaced by the credential's next flight number.
    pub fn start(
        credential: &ResumeCredential,
        protocol_id: u64,
        client_nonce: u64,
        version: u8,
        mut hello: ResumeHello,
    ) -> Result<Self, CryptoError> {
        hello.flight = credential.next_flight();
        let mut w = WireWriter::with_capacity(MAX_RESUME_HELLO_BYTES);
        hello.encode(&mut w).map_err(|_| CryptoError::TooLarge)?;
        let payload = w.finish();
        let prologue = resume_prologue(protocol_id, client_nonce, version, &credential.ticket);
        let mut state = Builder::new(params()?)
            .prologue(&prologue)
            .and_then(|b| b.psk(0, &credential.psk))
            .and_then(Builder::build_initiator)
            .map_err(|_| CryptoError::Unavailable)?;
        let mut message = vec![0u8; MAX_RESUME_NOISE_BYTES];
        let n = state
            .write_message(&payload, &mut message)
            .map_err(|_| CryptoError::Unavailable)?;
        message.truncate(n);
        Ok(Self {
            state,
            message,
            flight: hello.flight,
            token: None,
        })
    }

    /// The first Noise message (kept: a resend, with or without a cookie, is the same flight).
    pub fn message(&self) -> &[u8] {
        &self.message
    }

    /// This flight's number.
    pub fn flight(&self) -> u32 {
        self.flight
    }

    /// Reads the server's second Noise message. [`CryptoError::Auth`] leaves the flight usable,
    /// so a forged reply cannot end a resume.
    pub fn read_reply(&mut self, noise: &[u8; RESUME_REPLY_NOISE_BYTES]) -> Result<(), CryptoError> {
        let mut payload = [0u8; RESUME_REPLY_NOISE_BYTES];
        let n = self
            .state
            .read_message(noise, &mut payload)
            .map_err(|_| CryptoError::Auth)?;
        let token: [u8; 4] = payload[..n].try_into().map_err(|_| CryptoError::Malformed)?;
        self.token = Some(u32::from_le_bytes(token));
        Ok(())
    }

    /// The finished link, under the token the reply carried. Call after
    /// [`ResumeFlight::read_reply`] succeeded.
    pub fn into_link(self) -> Result<SecureLink, CryptoError> {
        let token = self.token.ok_or(CryptoError::Unavailable)?;
        SecureLink::new(self.state, token)
    }
}

/// The server's side of a resume, after the ticket opened: message 1 read, message 2 not yet
/// written. Reading costs one AEAD and no curve work; [`ResumeResponder::reply`] is the curve
/// work, so a server calls it only for a flight it will keep (ADR-8858 A3, A4).
pub struct ResumeResponder {
    state: HandshakeState,
    hello: ResumeHello,
}

impl std::fmt::Debug for ResumeResponder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeResponder")
            .field("hello", &self.hello)
            .finish_non_exhaustive()
    }
}

impl ResumeResponder {
    /// Opens the first Noise message under the ticket's PSK. [`ResumeRefusal::Psk`] when it does
    /// not open (the sender does not hold the PSK, or a plain field or the ticket was changed);
    /// [`ResumeRefusal::Malformed`] when it opens and is not a hello.
    pub fn open(
        psk: &[u8; 32],
        protocol_id: u64,
        client_nonce: u64,
        version: u8,
        ticket: &[u8; RESUMPTION_TICKET_BYTES],
        noise: &[u8],
    ) -> Result<Self, ResumeRefusal> {
        if !(MIN_RESUME_NOISE_BYTES..=MAX_RESUME_NOISE_BYTES).contains(&noise.len()) {
            return Err(ResumeRefusal::Malformed);
        }
        let prologue = resume_prologue(protocol_id, client_nonce, version, ticket);
        let mut state = params()
            .and_then(|p| {
                Builder::new(p)
                    .prologue(&prologue)
                    .and_then(|b| b.psk(0, psk))
                    .and_then(Builder::build_responder)
                    .map_err(|_| CryptoError::Unavailable)
            })
            .map_err(|_| ResumeRefusal::Unavailable)?;
        let mut payload = [0u8; MAX_RESUME_NOISE_BYTES];
        let n = state
            .read_message(noise, &mut payload)
            .map_err(|_| ResumeRefusal::Psk)?;
        let hello = ResumeHello::decode(&payload[..n]).map_err(|_| ResumeRefusal::Malformed)?;
        Ok(Self { state, hello })
    }

    /// The hello the client sealed.
    pub fn hello(&self) -> &ResumeHello {
        &self.hello
    }

    /// Writes the second Noise message (`e, ee`: one key generation and one curve
    /// multiplication) with `token` as its payload, and finishes the link.
    pub fn reply(mut self, token: u32) -> Result<(SecureLink, [u8; RESUME_REPLY_NOISE_BYTES]), CryptoError> {
        let mut noise = [0u8; RESUME_REPLY_NOISE_BYTES];
        let n = self
            .state
            .write_message(&token.to_le_bytes(), &mut noise)
            .map_err(|_| CryptoError::Unavailable)?;
        if n != noise.len() {
            return Err(CryptoError::Unavailable);
        }
        Ok((SecureLink::new(self.state, token)?, noise))
    }

    /// The hello, when the caller is done with the handshake state.
    pub fn into_hello(self) -> ResumeHello {
        self.hello
    }
}

/// `ResumeInit`: the client's first flight of a one-round-trip resume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeInit {
    /// The client's protocol id (`crate::payload::protocol_id`).
    pub protocol_id: u64,
    /// Random per attempt.
    pub client_nonce: u64,
    /// The suite version of the link the client resumes into.
    pub crypto_version: u8,
    /// The cookie of a [`ResumeRetry`], when the server asked for one.
    pub cookie: Option<[u8; 16]>,
    /// The resumption ticket.
    pub ticket: [u8; RESUMPTION_TICKET_BYTES],
    /// Noise `-> psk, e` and the sealed [`ResumeHello`].
    pub noise: Vec<u8>,
}

impl ResumeInit {
    /// The padded datagram. Fails when the Noise message is outside its size range.
    pub fn to_datagram(&self) -> Result<Vec<u8>, WireError> {
        self.write(None)
    }

    /// The padded datagram with this build's version stamp after the Noise message, so a server
    /// of another version can say which side is older (ADR-7985), as a `ConnectRequest`'s does.
    pub fn to_datagram_with_stamp(&self, stamp: &VersionStamp) -> Result<Vec<u8>, WireError> {
        self.write(Some(stamp))
    }

    fn write(&self, stamp: Option<&VersionStamp>) -> Result<Vec<u8>, WireError> {
        if self.noise.len() < MIN_RESUME_NOISE_BYTES {
            return Err(WireError::Invalid("resume noise message is too short"));
        }
        let mut w = begin(PacketKind::ResumeInit);
        w.u64(self.protocol_id);
        w.u64(self.client_nonce);
        w.u8(self.crypto_version);
        match &self.cookie {
            Some(cookie) => {
                w.u8(1);
                w.raw(cookie);
            }
            None => {
                w.u8(0);
                w.raw(&[0; 16]);
            }
        }
        w.raw(&self.ticket);
        w.bytes_u16(&self.noise, MAX_RESUME_NOISE_BYTES)?;
        if let Some(stamp) = stamp {
            stamp.encode_tagged(&mut w);
        }
        let mut out = w.finish();
        out.resize(RESUME_INIT_BYTES, 0);
        Ok(out)
    }

    /// Reads the bytes after the kind byte. A datagram shorter than [`RESUME_INIT_BYTES`] is
    /// refused before any field is read.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        if body.len() + 5 < RESUME_INIT_BYTES {
            return Err(WireError::Invalid("resume init is not padded"));
        }
        let mut r = WireReader::new(body);
        let protocol_id = r.u64()?;
        let client_nonce = r.u64()?;
        let crypto_version = r.u8()?;
        let flag = r.u8()?;
        let cookie: [u8; 16] = r.array()?;
        let cookie = match flag {
            0 => None,
            1 => Some(cookie),
            _ => return Err(WireError::Invalid("resume cookie flag")),
        };
        let ticket = r.array()?;
        let noise = r.bytes_u16(MAX_RESUME_NOISE_BYTES)?;
        if noise.len() < MIN_RESUME_NOISE_BYTES {
            return Err(WireError::Invalid("resume noise message is too short"));
        }
        Ok(Self {
            protocol_id,
            client_nonce,
            crypto_version,
            cookie,
            ticket,
            noise: noise.to_vec(),
        })
    }

    /// The version stamp after the Noise message of a request body (the bytes after the kind
    /// byte), if the client sent one. A malformed stamp counts as none.
    pub fn stamp_of(body: &[u8]) -> Option<VersionStamp> {
        let len = body.get(RESUME_INIT_FIELD_BYTES - 2..RESUME_INIT_FIELD_BYTES)?;
        let noise = usize::from(u16::from_le_bytes([len[0], len[1]]));
        VersionStamp::from_tagged(body.get(RESUME_INIT_FIELD_BYTES + noise..)?)
            .ok()
            .flatten()
    }

    /// The `ConnectRequest` this flight stands in for: a server that refuses the resume answers
    /// it as it would answer that request (a `Challenge`, or a deny).
    pub fn as_connect_request(&self) -> ConnectRequest {
        ConnectRequest {
            protocol_id: self.protocol_id,
            client_nonce: self.client_nonce,
            crypto_version: self.crypto_version,
            // A resume is a sealed link's: the client takes no plain one in its place.
            crypto_required: true,
        }
    }
}

/// `ResumeReply`: the second Noise message and, after it, the `Accept` as a sealed record under
/// the new link's keys (ADR-8858 A7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeReply {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// Noise `<- e, ee`, with the session token as its payload.
    pub noise: [u8; RESUME_REPLY_NOISE_BYTES],
    /// A whole sealed datagram ([`SecureLink::seal`] of an `Accept` body).
    pub accept: Vec<u8>,
}

impl ResumeReply {
    /// The datagram. Fails when the sealed record is outside its size range.
    pub fn to_datagram(&self) -> Result<Vec<u8>, WireError> {
        check_accept_record(self.accept.len())?;
        let mut w = begin(PacketKind::ResumeReply);
        w.u64(self.client_nonce);
        w.raw(&self.noise);
        w.raw(&self.accept);
        Ok(w.finish())
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let client_nonce = r.u64()?;
        let noise = r.array()?;
        let accept = r.take(r.remaining())?;
        check_accept_record(accept.len())?;
        Ok(Self {
            client_nonce,
            noise,
            accept: accept.to_vec(),
        })
    }
}

fn check_accept_record(len: usize) -> Result<(), WireError> {
    if len < MIN_RESUME_ACCEPT_RECORD_BYTES {
        return Err(WireError::Invalid("resume reply has no sealed accept"));
    }
    if len > MAX_RESUME_ACCEPT_RECORD_BYTES {
        return Err(WireError::TooLong {
            len,
            max: MAX_RESUME_ACCEPT_RECORD_BYTES,
        });
    }
    Ok(())
}

/// `ResumeRetry`: the server asks a `ResumeInit` from outside its ticket's address block to come
/// again with this cookie (the stateless retry of RFC 9000 § 8.1.2). 29 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResumeRetry {
    /// Echo of the request nonce.
    pub client_nonce: u64,
    /// The cookie to bring back in [`ResumeInit::cookie`].
    pub cookie: [u8; 16],
}

impl ResumeRetry {
    /// The datagram.
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = begin(PacketKind::ResumeRetry);
        w.u64(self.client_nonce);
        w.raw(&self.cookie);
        w.finish()
    }

    /// Reads the bytes after the kind byte.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(body);
        let out = Self {
            client_nonce: r.u64()?,
            cookie: r.array()?,
        };
        r.finish()?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, Ipv6Addr};

    use super::*;
    use crate::crypto::{ClientHandshake, SealedHeader, SealedKind, ServerIdentity, accept_handshake};
    use crate::handshake::{Accept, split_datagram};

    const GRACE_MS: u64 = 60_000;
    const PROTOCOL: u64 = 0x1122_3344_5566_7788;

    fn claims(now_ms: u64) -> TicketClaims {
        TicketClaims {
            peer: 7,
            epoch: 3,
            psk: [0x5A; 32],
            block: AddrBlock::of(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9))).key_bytes(),
            issued_ms: now_ms,
            expires_ms: now_ms + GRACE_MS,
            protocol_id: PROTOCOL,
            build_hash: build_hash("build-1"),
            password_verified: true,
            resume_id: [0xA7; RESUME_TICKET_BYTES],
        }
    }

    fn hello() -> ResumeHello {
        ResumeHello {
            flight: 0,
            name: "Ada".into(),
            role: PeerRole::Player,
            build: "build-1".into(),
            join_data: vec![1, 2, 3],
        }
    }

    #[test]
    fn a_ticket_seals_and_opens_to_the_same_claims() {
        let mut keys = TicketKeys::new(&[1; 32], GRACE_MS);
        let now = 5 * GRACE_MS + 1_234;
        let want = claims(now);
        let ticket = keys.seal(&want, now).expect("seal");
        assert_eq!(ticket.len(), RESUMPTION_TICKET_BYTES);
        assert_eq!(ticket[0], RESUMPTION_TICKET_VERSION);
        let got = keys.open(&ticket, now + 10).expect("open");
        assert_eq!(got, want);
        // Until its last millisecond, across the key rotation in between.
        assert_eq!(keys.open(&ticket, now + GRACE_MS - 1), Ok(want));
        assert!(got.in_block(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 9))));
        assert!(!got.in_block(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))));
        assert_eq!(got.check_protocol(PROTOCOL, "build-1"), Ok(()));
        // The claims are not readable in the ticket.
        assert!(!ticket.windows(8).any(|w| w == [0x5A; 8]), "the PSK is sealed");
        // Two tickets for the same claims differ (a nonce each).
        let again = keys.seal(&want, now).expect("seal");
        assert_ne!(ticket[..], again[..]);
        assert_eq!(keys.open(&again, now), Ok(want));
    }

    #[test]
    fn an_ipv6_block_is_its_slash_64() {
        let mut c = claims(0);
        let home: Ipv6Addr = "2001:db8:1:2::9".parse().expect("addr");
        c.block = AddrBlock::of(IpAddr::V6(home)).key_bytes();
        assert!(c.in_block("2001:db8:1:2:ffff::1".parse().expect("addr")));
        assert!(!c.in_block("2001:db8:1:3::9".parse().expect("addr")));
    }

    #[test]
    fn a_ticket_from_a_retired_key_id_is_refused_with_its_own_code() {
        let mut keys = TicketKeys::new(&[1; 32], GRACE_MS);
        let now = 10 * GRACE_MS;
        let mut long_lived = claims(now);
        // Not expired by its own clock, so only the key's age refuses it.
        long_lived.expires_ms = u64::MAX;
        let ticket = keys.seal(&long_lived, now).expect("seal");
        assert_eq!(
            keys.open(&ticket, now + GRACE_MS),
            Ok(long_lived),
            "the previous key opens"
        );
        let refusal = keys.open(&ticket, now + 2 * GRACE_MS).expect_err("retired");
        assert_eq!(refusal, ResumeRefusal::KeyRetired);
        assert_eq!(refusal.as_str(), "resume_key_retired");
        // A key id this server has not reached is another server's.
        let mut ahead = TicketKeys::new(&[1; 32], GRACE_MS);
        let future = ahead.seal(&long_lived, now + 5 * GRACE_MS).expect("seal");
        let refusal = keys.open(&future, now).expect_err("unknown");
        assert_eq!(refusal.as_str(), "resume_key_unknown");
    }

    #[test]
    fn a_ticket_from_another_server_is_refused_with_its_own_code() {
        let mut ours = TicketKeys::new(&[1; 32], GRACE_MS);
        let mut theirs = TicketKeys::new(&[2; 32], GRACE_MS);
        let now = 3 * GRACE_MS;
        let foreign = theirs.seal(&claims(now), now).expect("seal");
        let refusal = ours.open(&foreign, now).expect_err("another key");
        assert_eq!(refusal, ResumeRefusal::TicketAuth);
        assert_eq!(refusal.as_str(), "resume_ticket_auth");
        // Every changed byte fails closed: the header reaches another key, the rest fails the tag.
        let good = ours.seal(&claims(now), now).expect("seal");
        for at in 0..RESUMPTION_TICKET_BYTES {
            let mut bad = good;
            bad[at] ^= 0x01;
            assert!(ours.open(&bad, now).is_err(), "byte {at}");
        }
        assert_eq!(ours.open(&good[..141], now), Err(ResumeRefusal::Malformed));
    }

    #[test]
    fn an_expired_ticket_is_refused_with_its_own_code() {
        let mut keys = TicketKeys::new(&[1; 32], GRACE_MS);
        let now = 4 * GRACE_MS + 500;
        let ticket = keys.seal(&claims(now), now).expect("seal");
        assert!(keys.open(&ticket, now + GRACE_MS - 1).is_ok());
        let refusal = keys.open(&ticket, now + GRACE_MS).expect_err("expired");
        assert_eq!(refusal, ResumeRefusal::Expired);
        assert_eq!(refusal.as_str(), "resume_expired");
        // A short-lived ticket expires while its key is still the current one.
        let mut short = claims(now);
        short.expires_ms = now + 10;
        let ticket = keys.seal(&short, now).expect("seal");
        assert_eq!(keys.open(&ticket, now + 10), Err(ResumeRefusal::Expired));
    }

    #[test]
    fn a_ticket_with_a_wrong_epoch_is_refused_with_its_own_code() {
        let mut epochs = ResumeEpochs::default();
        assert_eq!(epochs.check(0), Err(ResumeRefusal::Epoch), "never minted");
        let first = epochs.mint().expect("mint");
        assert_eq!(first, 1);
        assert_eq!(epochs.check(first), Ok(()), "good before its accept is confirmed");
        epochs.confirm(first);
        // A resume mints the next; until the client confirms it, both name the peer (ADR-8535).
        let second = epochs.mint().expect("mint");
        assert_eq!(epochs.check(first), Ok(()));
        assert_eq!(epochs.check(second), Ok(()));
        assert_eq!(
            epochs.check(second + 1),
            Err(ResumeRefusal::Epoch),
            "never minted"
        );
        // The confirmation spends every older ticket, for good.
        epochs.confirm(second);
        let refusal = epochs.check(first).expect_err("spent");
        assert_eq!(refusal.as_str(), "resume_epoch");
        assert_eq!(epochs.check(second), Ok(()));
        // An epoch never minted, or an older one, confirms nothing.
        epochs.confirm(99);
        epochs.confirm(first);
        assert_eq!((epochs.confirmed(), epochs.minted()), (second, second));
        // The counter fails closed at its end.
        let mut last = ResumeEpochs {
            confirmed: u32::MAX,
            minted: u32::MAX,
        };
        assert_eq!(last.mint(), None);
    }

    #[test]
    fn a_ticket_for_another_protocol_or_build_is_refused_with_its_own_code() {
        let c = claims(0);
        assert_eq!(
            c.check_protocol(PROTOCOL + 1, "build-1"),
            Err(ResumeRefusal::Protocol)
        );
        assert_eq!(
            c.check_protocol(PROTOCOL, "build-2"),
            Err(ResumeRefusal::Protocol)
        );
        assert_eq!(ResumeRefusal::Protocol.as_str(), "resume_protocol");
    }

    #[test]
    fn every_refusal_has_its_own_lowercase_code() {
        let mut seen = std::collections::BTreeSet::new();
        for refusal in ResumeRefusal::ALL {
            let code = refusal.as_str();
            assert!(code.starts_with("resume_"), "{code}");
            assert!(
                code.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{code}"
            );
            assert!(seen.insert(code), "{code} twice");
            assert_eq!(refusal.to_string(), code);
        }
    }

    fn credential() -> ResumeCredential {
        ResumeCredential::new([0x77; RESUMPTION_TICKET_BYTES], [0x5A; 32], None)
    }

    fn init_of(flight: &ResumeFlight, credential: &ResumeCredential) -> ResumeInit {
        ResumeInit {
            protocol_id: PROTOCOL,
            client_nonce: 0xC0FFEE,
            crypto_version: crate::crypto::CRYPTO_VERSION,
            cookie: None,
            ticket: *credential.ticket(),
            noise: flight.message().to_vec(),
        }
    }

    #[test]
    fn a_resume_init_is_padded_to_1200_bytes_and_a_shorter_one_is_refused() {
        let credential = credential();
        let flight = ResumeFlight::start(&credential, PROTOCOL, 0xC0FFEE, 2, hello()).expect("start");
        let init = init_of(&flight, &credential);
        let bytes = init.to_datagram().expect("datagram");
        assert_eq!(bytes.len(), RESUME_INIT_BYTES);
        assert_eq!(RESUME_INIT_BYTES, 1200);
        let (kind, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(kind, PacketKind::ResumeInit);
        assert_eq!(ResumeInit::decode(body).expect("decode"), init);
        assert!(
            ResumeInit::decode(&body[..body.len() - 1]).is_err(),
            "one byte short of the padding"
        );
        // No answer is larger than the request.
        let reply = ResumeReply {
            client_nonce: 0xC0FFEE,
            noise: [0; RESUME_REPLY_NOISE_BYTES],
            accept: vec![0; MAX_RESUME_ACCEPT_RECORD_BYTES],
        }
        .to_datagram()
        .expect("reply");
        assert!(reply.len() < bytes.len(), "{} bytes", reply.len());
        // The largest hello and a version stamp still fit the padded size.
        let largest = ResumeHello {
            flight: 0,
            name: "n".repeat(MAX_NAME_BYTES),
            role: PeerRole::Spectator,
            build: "b".repeat(MAX_BUILD_BYTES),
            join_data: vec![9; MAX_JOIN_DATA_BYTES],
        };
        let flight = ResumeFlight::start(&credential, PROTOCOL, 1, 2, largest).expect("start");
        assert_eq!(flight.message().len(), MAX_RESUME_NOISE_BYTES);
        let stamp = VersionStamp::new(6, &"v".repeat(crate::version_probe::MAX_GAME_VERSION_BYTES), 3, 4);
        let mut init = init_of(&flight, &credential);
        init.cookie = Some([4; 16]);
        let bytes = init.to_datagram_with_stamp(&stamp).expect("datagram");
        assert_eq!(bytes.len(), RESUME_INIT_BYTES);
        let (_, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(ResumeInit::decode(body).expect("decode"), init);
        assert_eq!(ResumeInit::stamp_of(body), Some(stamp));
        // No stamp: the padding reads as none.
        let bytes = init.to_datagram().expect("datagram");
        let (_, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(ResumeInit::stamp_of(body), None);
        // A cookie flag that is neither 0 nor 1 is refused.
        let mut bad = bytes.clone();
        bad[5 + 17] = 2;
        let (_, body) = split_datagram(&bad).expect("framing");
        assert!(ResumeInit::decode(body).is_err());
    }

    #[test]
    fn the_first_flight_carries_no_readable_join_and_stands_in_for_a_connect_request() {
        let credential = credential();
        let named = ResumeHello {
            name: "a-long-player-name".into(),
            ..hello()
        };
        let flight = ResumeFlight::start(&credential, PROTOCOL, 0xC0FFEE, 2, named).expect("start");
        let init = init_of(&flight, &credential);
        let bytes = init.to_datagram().expect("datagram");
        assert!(
            !bytes.windows(18).any(|w| w == b"a-long-player-name"),
            "the hello is sealed"
        );
        let request = init.as_connect_request();
        assert_eq!(request.protocol_id, PROTOCOL);
        assert_eq!(request.client_nonce, 0xC0FFEE);
        assert!(request.crypto_required);
    }

    /// The whole exchange: the server opens message 1 with the ticket's PSK and no curve work,
    /// writes message 2, and both ends hold the same link and the same next PSK.
    #[test]
    fn a_flight_and_its_reply_make_one_link_in_one_round_trip() {
        let credential = credential();
        let mut flight = ResumeFlight::start(&credential, PROTOCOL, 0xC0FFEE, 2, hello()).expect("start");
        assert_eq!(flight.flight(), 1);
        let responder = ResumeResponder::open(
            &[0x5A; 32],
            PROTOCOL,
            0xC0FFEE,
            2,
            credential.ticket(),
            flight.message(),
        )
        .expect("message 1 opens");
        let mut want = hello();
        want.flight = 1;
        assert_eq!(responder.hello(), &want);
        let (mut server, noise) = responder.reply(0x5EA1).expect("reply");
        // A forged reply leaves the flight usable.
        let mut forged = noise;
        forged[40] ^= 1;
        assert_eq!(flight.read_reply(&forged), Err(CryptoError::Auth));
        flight.read_reply(&noise).expect("message 2 opens");
        let mut client = flight.into_link().expect("link");
        assert_eq!(client.token(), 0x5EA1);
        assert_eq!(client.handshake_hash(), server.handshake_hash());
        // The next PSK: the same on both ends, new, and not the old one.
        assert_eq!(client.resume_psk(), server.resume_psk());
        assert_ne!(client.resume_psk(), &[0x5A; 32]);
        // The server's sealed accept opens on the client, and the client's first record (the key
        // confirmation) opens on the server.
        let accept = Accept {
            client_nonce: 0xC0FFEE,
            session_token: 0x5EA1,
            peer_id: 7,
            tick_rate_hz: 60,
            server_tick: 99,
            resume_ticket: [3; RESUME_TICKET_BYTES],
            resumption: Some([4; RESUMPTION_TICKET_BYTES]),
        };
        let record = server.seal(SealedKind::Accept, &accept.to_body()).expect("seal");
        let reply = ResumeReply {
            client_nonce: 0xC0FFEE,
            noise,
            accept: record,
        };
        let bytes = reply.to_datagram().expect("datagram");
        let (kind, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(kind, PacketKind::ResumeReply);
        let decoded = ResumeReply::decode(body).expect("decode");
        assert_eq!(decoded, reply);
        let (kind, body) = split_datagram(&decoded.accept).expect("the record is a datagram");
        assert_eq!(kind, PacketKind::Sealed);
        let (kind, plain) = client
            .open(SealedHeader::parse(body).expect("header"))
            .expect("the accept opens");
        assert_eq!(kind, SealedKind::Accept);
        assert_eq!(Accept::decode(&plain).expect("accept"), accept);
        let confirm = client.seal(SealedKind::Payload, b"first").expect("seal");
        let (_, body) = split_datagram(&confirm).expect("framing");
        let (kind, plain) = server
            .open(SealedHeader::parse(body).expect("header"))
            .expect("the confirmation opens");
        assert_eq!((kind, plain.as_slice()), (SealedKind::Payload, &b"first"[..]));
    }

    #[test]
    fn message_1_opens_only_under_the_psk_and_the_fields_it_was_made_for() {
        let credential = credential();
        let flight = ResumeFlight::start(&credential, PROTOCOL, 0xC0FFEE, 2, hello()).expect("start");
        let open = |psk: &[u8; 32], protocol, nonce, version, ticket: &[u8; RESUMPTION_TICKET_BYTES]| {
            ResumeResponder::open(psk, protocol, nonce, version, ticket, flight.message()).map(|_| ())
        };
        let ticket = credential.ticket();
        assert_eq!(open(&[0x5A; 32], PROTOCOL, 0xC0FFEE, 2, ticket), Ok(()));
        let refusal = open(&[0x5B; 32], PROTOCOL, 0xC0FFEE, 2, ticket).expect_err("another PSK");
        assert_eq!(refusal, ResumeRefusal::Psk);
        assert_eq!(refusal.as_str(), "resume_psk");
        assert_eq!(
            open(&[0x5A; 32], PROTOCOL + 1, 0xC0FFEE, 2, ticket),
            Err(ResumeRefusal::Psk)
        );
        assert_eq!(
            open(&[0x5A; 32], PROTOCOL, 0xC0FFEF, 2, ticket),
            Err(ResumeRefusal::Psk)
        );
        assert_eq!(
            open(&[0x5A; 32], PROTOCOL, 0xC0FFEE, 3, ticket),
            Err(ResumeRefusal::Psk)
        );
        let mut other = *ticket;
        other[100] ^= 1;
        assert_eq!(
            open(&[0x5A; 32], PROTOCOL, 0xC0FFEE, 2, &other),
            Err(ResumeRefusal::Psk),
            "the flight is bound to its ticket"
        );
        // A changed byte anywhere in the message, and a cut one.
        for at in [0, 31, 32, flight.message().len() - 1] {
            let mut bad = flight.message().to_vec();
            bad[at] ^= 1;
            assert_eq!(
                ResumeResponder::open(&[0x5A; 32], PROTOCOL, 0xC0FFEE, 2, ticket, &bad).map(|_| ()),
                Err(ResumeRefusal::Psk),
                "byte {at}"
            );
        }
        assert_eq!(
            ResumeResponder::open(
                &[0x5A; 32],
                PROTOCOL,
                0xC0FFEE,
                2,
                ticket,
                &flight.message()[..40]
            )
            .map(|_| ()),
            Err(ResumeRefusal::Malformed)
        );
    }

    /// ADR-8858 A3: every flight made with one credential has a higher number than the last,
    /// also through clones (a config cloned per attempt), and each has a fresh ephemeral.
    #[test]
    fn flights_of_one_credential_count_up_across_clones() {
        let credential = credential();
        let clone = credential.clone();
        let a = ResumeFlight::start(&credential, PROTOCOL, 1, 2, hello()).expect("start");
        let b = ResumeFlight::start(&clone, PROTOCOL, 2, 2, hello()).expect("start");
        let c = ResumeFlight::start(&credential, PROTOCOL, 3, 2, hello()).expect("start");
        assert_eq!((a.flight(), b.flight(), c.flight()), (1, 2, 3));
        assert_eq!(credential.flights(), 3);
        assert_ne!(a.message()[..32], b.message()[..32], "a fresh ephemeral each");
        assert_eq!(credential, clone);
        let printed = format!("{credential:?}");
        assert!(
            !printed.contains("psk") && !printed.contains("ticket"),
            "{printed}"
        );
    }

    /// ADR-8858 A7: both ends of an ordinary sealed link derive the same PSK, and it is neither
    /// the handshake hash nor recoverable from the wire.
    #[test]
    fn both_ends_of_a_link_derive_the_same_resume_psk() {
        let identity = ServerIdentity::generate().expect("random");
        let mut hs = ClientHandshake::start(&[1; 32], Some(identity.public())).expect("start");
        let (server, reply) = accept_handshake(&identity, &[1; 32], hs.message(), 9).expect("accept");
        hs.read_reply(&reply).expect("reply");
        let client = hs.into_link().expect("link");
        assert_eq!(client.resume_psk(), server.resume_psk());
        assert_ne!(client.resume_psk(), client.handshake_hash());
        assert_ne!(client.resume_psk(), &[0; 32]);
        // Another session, another PSK.
        let hs2 = ClientHandshake::start(&[1; 32], Some(identity.public())).expect("start");
        let (server2, _) = accept_handshake(&identity, &[1; 32], hs2.message(), 9).expect("accept");
        assert_ne!(server.resume_psk(), server2.resume_psk());
        assert_eq!(
            derive_resume_psk(&[1; 32], &[2; 32], &[3; 32]),
            derive_resume_psk(&[1; 32], &[2; 32], &[3; 32])
        );
        assert_ne!(
            derive_resume_psk(&[1; 32], &[2; 32], &[3; 32]),
            derive_resume_psk(&[1; 32], &[3; 32], &[2; 32])
        );
    }

    #[test]
    fn a_resume_retry_and_a_reply_round_trip_and_refuse_bad_sizes() {
        let retry = ResumeRetry {
            client_nonce: 5,
            cookie: [6; 16],
        };
        let bytes = retry.to_datagram();
        assert_eq!(bytes.len(), 29);
        let (kind, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(kind, PacketKind::ResumeRetry);
        assert_eq!(ResumeRetry::decode(body).expect("decode"), retry);
        assert!(ResumeRetry::decode(&body[..body.len() - 1]).is_err());
        let mut long = body.to_vec();
        long.push(0);
        assert!(ResumeRetry::decode(&long).is_err());

        let reply = ResumeReply {
            client_nonce: 5,
            noise: [7; RESUME_REPLY_NOISE_BYTES],
            accept: vec![8; MIN_RESUME_ACCEPT_RECORD_BYTES],
        };
        let bytes = reply.to_datagram().expect("datagram");
        let (_, body) = split_datagram(&bytes).expect("framing");
        assert_eq!(ResumeReply::decode(body).expect("decode"), reply);
        assert!(
            ResumeReply::decode(&body[..body.len() - 1]).is_err(),
            "no room for an accept"
        );
        let too_long = ResumeReply {
            accept: vec![8; MAX_RESUME_ACCEPT_RECORD_BYTES + 1],
            ..reply
        };
        assert!(too_long.to_datagram().is_err());
    }

    #[test]
    fn a_hello_round_trips_and_becomes_a_join_token_without_a_password_proof() {
        let mut w = WireWriter::new();
        let mut h = hello();
        h.flight = 41;
        h.encode(&mut w).expect("encode");
        let bytes = w.finish();
        assert_eq!(ResumeHello::decode(&bytes).expect("decode"), h);
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(ResumeHello::decode(&trailing).is_err());
        let token = h.into_join_token([9; RESUME_TICKET_BYTES]);
        assert_eq!(token.name, "Ada");
        assert_eq!(token.password_proof, [0; 32]);
        assert_eq!(token.resume, Some([9; RESUME_TICKET_BYTES]));
    }
}
