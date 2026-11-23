// SPDX-License-Identifier: MPL-2.0
//! The version probe and version stamps (spec 120 § 12.2, ADR-7985).
//!
//! Two builds that cannot talk refuse each other at the handshake with `protocol_mismatch`
//! ([`crate::payload::protocol_id`] mixes the framing, game, protocol and content versions). That
//! refusal alone cannot say which side should update. This module carries the versions themselves:
//!
//! - A **version stamp** ([`VersionStamp`]: wire version, game version text, game protocol version,
//!   content hash) rides in the padding of every `ConnectRequest` and, when the request carried
//!   one, after the detail of a `protocol_mismatch` `Deny`. A build that predates stamps ignores the
//!   padding and never sends the trailer, so old and new builds still refuse each other cleanly.
//! - The **version probe** is a plaintext datagram every build from this one on answers on its
//!   game socket (the server, the relay, the shared-port reader). Its bytes are frozen forever:
//!
//! ```text
//! request  RGVP | probe_version u8 | nonce u64 | zero padding to 64 bytes
//! answer   RGVR | probe_version u8 | nonce u64 | wire u32 | game_version str_u8 (<= 32)
//!               | protocol u32 | content_hash u64                         (at most 64 bytes)
//! ```
//!
//! Integers are little-endian; `str_u8` is a one-byte length and UTF-8 bytes. The answer is never
//! larger than the request, so a forged source address cannot use a server as an amplifier (the
//! rule of RFC 9000 § 8 for unvalidated addresses). Answers are rate limited per source prefix
//! (IPv4 /24, IPv6 /56, [`probe_prefix`]): a forger can spend one prefix's probe budget, which only
//! costs that prefix the version text, never a join.
//!
//! A client that gets a `protocol_mismatch` deny without a stamp sends probes for
//! [`PROBE_WAIT_MS`]; an answer names the server's versions, and no answer after a valid deny means
//! the server predates the probe, so it is older than this build ([`MismatchSide::ServerOlder`]).
//! The design is the owner's DO27 version probe, re-derived with Regia's own bytes (the `[D27]` rows
//! of spec 120).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::payload::{NetGame, REGIA_NET_WIRE_VERSION};
use crate::rate_limit::SourceLimiter;
use crate::wire::{WireError, WireReader, WireWriter};

/// First four bytes of a probe request.
pub const PROBE_REQUEST_MAGIC: [u8; 4] = *b"RGVP";
/// First four bytes of a probe answer.
pub const PROBE_ANSWER_MAGIC: [u8; 4] = *b"RGVR";
/// The probe layout version this build writes. Frozen: a later build may add a version 2, but it
/// still answers a version 1 request with this layout.
pub const PROBE_VERSION: u8 = 1;
/// A probe request is padded to this size; shorter requests get no answer.
pub const PROBE_REQUEST_BYTES: usize = 64;
/// The largest probe answer (never more than the request).
pub const MAX_PROBE_ANSWER_BYTES: usize = 64;
/// The longest game version text a stamp carries; longer text is cut at a character boundary.
pub const MAX_GAME_VERSION_BYTES: usize = 32;
/// Marks a version stamp inside a `ConnectRequest`'s padding or after a `Deny`'s detail.
pub const STAMP_MAGIC: [u8; 4] = *b"RGVS";
/// The largest encoded stamp, marker included.
pub const MAX_STAMP_BYTES: usize = 4 + 4 + 4 + 8 + 1 + MAX_GAME_VERSION_BYTES;
/// Probe answers per source prefix per second (the bucket holds two seconds of it).
pub const PROBE_ANSWERS_PER_PREFIX_PER_S: u32 = 4;
/// Buckets of the per-prefix probe limiter (a fixed table, so forged sources cannot grow it).
pub const PROBE_LIMITER_SLOTS: usize = 4096;
/// Bits of an IPv4 source that name its probe prefix.
pub const IPV4_PROBE_PREFIX: u8 = 24;
/// Bits of an IPv6 source that name its probe prefix.
pub const IPV6_PROBE_PREFIX: u8 = 56;
/// How long a client waits for a probe answer after a stamp-less `protocol_mismatch` deny.
pub const PROBE_WAIT_MS: u64 = 1_500;
/// How often the client resends its probe while it waits.
pub const PROBE_RESEND_MS: u64 = 250;

/// The frozen layouts, as `cargo xtask check-formats` fingerprints them (format `version-probe`,
/// `docs/formats/version-probe.md`). Never edit a line: add a probe version instead.
pub const PROBE_LAYOUT: [&str; 3] = [
    "request: RGVP | probe_version u8 | nonce u64 le | zero padding to 64 bytes",
    "answer: RGVR | probe_version u8 | nonce u64 le | wire u32 le | game_version str_u8 <= 32 | protocol u32 le \
     | content_hash u64 le (at most 64 bytes)",
    "stamp: RGVS | wire u32 le | protocol u32 le | content_hash u64 le | game_version str_u8 <= 32",
];

/// The versions one build runs, as the handshake and the probe carry them.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VersionStamp {
    /// `regia_net`'s framing version ([`REGIA_NET_WIRE_VERSION`]).
    pub wire: u32,
    /// The game's version text ([`NetGame::GAME_VERSION`]; at most [`MAX_GAME_VERSION_BYTES`]).
    pub game_version: String,
    /// The game's protocol version ([`NetGame::PROTOCOL_VERSION`]).
    pub protocol: u32,
    /// The content hash both sides must agree on (`0` = none declared).
    pub content_hash: u64,
}

impl VersionStamp {
    /// The stamp of game `G` in this build.
    pub fn of<G: NetGame>() -> Self {
        Self::of_with::<G>(0)
    }

    /// The stamp of game `G` with a run-time content hash folded in
    /// ([`crate::payload::combined_content_hash`]).
    pub fn of_with<G: NetGame>(runtime_content: u64) -> Self {
        Self::new(
            REGIA_NET_WIRE_VERSION,
            G::GAME_VERSION,
            G::PROTOCOL_VERSION,
            crate::payload::combined_content_hash(G::content_hash(), runtime_content),
        )
    }

    /// A stamp from its parts; the game version is cut to [`MAX_GAME_VERSION_BYTES`].
    pub fn new(wire: u32, game_version: &str, protocol: u32, content_hash: u64) -> Self {
        Self {
            wire,
            game_version: cut(game_version, MAX_GAME_VERSION_BYTES).to_owned(),
            protocol,
            content_hash,
        }
    }

    /// The same stamp with `content_hash` when it is non-zero (a runtime content hash overrides
    /// the game's compiled-in one).
    #[must_use]
    pub fn with_content_hash(mut self, content_hash: u64) -> Self {
        if content_hash != 0 {
            self.content_hash = content_hash;
        }
        self
    }

    /// The game version for messages: the text, or `wire W protocol P` when the game set none.
    pub fn display_version(&self) -> String {
        if self.game_version.is_empty() {
            format!("wire {} protocol {}", self.wire, self.protocol)
        } else {
            self.game_version.clone()
        }
    }

    /// Appends the stamp with its [`STAMP_MAGIC`] marker (at most [`MAX_STAMP_BYTES`]).
    pub fn encode_tagged(&self, w: &mut WireWriter) {
        w.raw(&STAMP_MAGIC);
        w.u32(self.wire);
        w.u32(self.protocol);
        w.u64(self.content_hash);
        // Cannot fail: the text was cut to the limit.
        let _ = w.str_u8(
            cut(&self.game_version, MAX_GAME_VERSION_BYTES),
            MAX_GAME_VERSION_BYTES,
        );
    }

    /// Reads a tagged stamp at the start of `bytes` (bytes after it, such as zero padding, are
    /// ignored). `Ok(None)` when no marker is there (a build that predates stamps: zero padding, or
    /// nothing); an error when the marker is there but the stamp is cut or malformed.
    pub fn from_tagged(bytes: &[u8]) -> Result<Option<Self>, WireError> {
        let Some(rest) = bytes.strip_prefix(&STAMP_MAGIC[..]) else {
            return Ok(None);
        };
        let mut r = WireReader::new(rest);
        let wire = r.u32()?;
        let protocol = r.u32()?;
        let content_hash = r.u64()?;
        let game_version = r.str_u8(MAX_GAME_VERSION_BYTES)?.to_owned();
        Ok(Some(Self {
            wire,
            game_version,
            protocol,
            content_hash,
        }))
    }
}

/// Which side of a refused join is older, from the client's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MismatchSide {
    /// The client is older than the server: update the game.
    ClientOlder,
    /// The server is older than the client (or predates the version probe).
    ServerOlder,
    /// Same versions, different content: install the same content.
    ContentDiffers,
    /// The versions cannot be ordered (another game, or unordered version text).
    Unordered,
}

impl MismatchSide {
    /// Compares the client's and the server's stamps. A side without a stamp predates stamps and
    /// the probe, so it is the older one.
    pub fn between(client: Option<&VersionStamp>, server: Option<&VersionStamp>) -> Self {
        let (client, server) = match (client, server) {
            (Some(c), Some(s)) => (c, s),
            (Some(_), None) => return Self::ServerOlder,
            (None, Some(_)) => return Self::ClientOlder,
            (None, None) => return Self::Unordered,
        };
        let ordered = match (
            parse_dotted(&client.game_version),
            parse_dotted(&server.game_version),
        ) {
            (Some(c), Some(s)) if c != s => Some(c.cmp(&s)),
            _ => None,
        };
        let ordering = ordered
            .or_else(|| (client.wire != server.wire).then(|| client.wire.cmp(&server.wire)))
            .or_else(|| (client.protocol != server.protocol).then(|| client.protocol.cmp(&server.protocol)));
        match ordering {
            Some(std::cmp::Ordering::Less) => Self::ClientOlder,
            Some(_) => Self::ServerOlder,
            None if client.content_hash != server.content_hash
                && client.game_version == server.game_version =>
            {
                Self::ContentDiffers
            }
            None => Self::Unordered,
        }
    }

    /// `Some(true)` when the server is older, `Some(false)` when the client is, `None` when the
    /// versions do not say.
    pub fn server_is_older(self) -> Option<bool> {
        match self {
            Self::ServerOlder => Some(true),
            Self::ClientOlder => Some(false),
            Self::ContentDiffers | Self::Unordered => None,
        }
    }

    /// What the refused player should do: `update the game`, `the server is older than this
    /// build`, `install the same content as the server` or `another game or build`.
    pub fn advice(self) -> &'static str {
        match self {
            Self::ClientOlder => "update the game",
            Self::ServerOlder => "the server is older than this build",
            Self::ContentDiffers => "install the same content as the server",
            Self::Unordered => "another game or build",
        }
    }

    /// Lowercase code for logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ClientOlder => "client_older",
            Self::ServerOlder => "server_older",
            Self::ContentDiffers => "content_differs",
            Self::Unordered => "unordered",
        }
    }

    /// The side named by a code inside a refusal's detail text (the `(client_older)` tag the
    /// engine's refusals end with), for clients that only have the text.
    pub fn in_detail(detail: &str) -> Option<Self> {
        [
            Self::ClientOlder,
            Self::ServerOlder,
            Self::ContentDiffers,
            Self::Unordered,
        ]
        .into_iter()
        .find(|side| detail.contains(&format!("({})", side.as_str())))
    }
}

/// A refused join's versions, as `net.status` reports it (`version_mismatch`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionMismatchReport {
    /// Which side is older (`client_older`, `server_older`, `content_differs`, `unordered`).
    pub side: MismatchSide,
    /// `Some(true)` when the server is older, `Some(false)` when the client is.
    pub server_is_older: Option<bool>,
    /// The client's versions (`None`: the client predates stamps).
    pub client: Option<VersionStamp>,
    /// The server's versions (`None`: the server predates the probe).
    pub server: Option<VersionStamp>,
    /// The line to show the player.
    pub message: String,
}

impl VersionMismatchReport {
    /// The report for a client and a server stamp.
    pub fn new(client: Option<VersionStamp>, server: Option<VersionStamp>) -> Self {
        let side = MismatchSide::between(client.as_ref(), server.as_ref());
        let message = mismatch_message(side, client.as_ref(), server.as_ref());
        Self {
            side,
            server_is_older: side.server_is_older(),
            client,
            server,
            message,
        }
    }
}

/// The player-facing line for a mismatch, ending with the side's code in parentheses.
pub fn mismatch_message(
    side: MismatchSide,
    client: Option<&VersionStamp>,
    server: Option<&VersionStamp>,
) -> String {
    let server_text = server.map_or_else(|| "an older build".to_owned(), VersionStamp::display_version);
    let client_text = client.map_or_else(|| "an older build".to_owned(), VersionStamp::display_version);
    let what = side.advice();
    match (side, client, server) {
        (MismatchSide::ContentDiffers, Some(c), Some(s)) => format!(
            "the server's content {:016x} differs from yours {:016x}: {what} ({})",
            s.content_hash,
            c.content_hash,
            side.as_str()
        ),
        _ => format!(
            "the server runs {server_text}, you have {client_text}: {what} ({})",
            side.as_str()
        ),
    }
}

/// A probe request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeRequest {
    /// The asker's nonce, echoed in the answer.
    pub nonce: u64,
}

impl ProbeRequest {
    /// The padded datagram ([`PROBE_REQUEST_BYTES`]).
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(PROBE_REQUEST_BYTES);
        out.extend_from_slice(&PROBE_REQUEST_MAGIC);
        out.push(PROBE_VERSION);
        out.extend_from_slice(&self.nonce.to_le_bytes());
        out.resize(PROBE_REQUEST_BYTES, 0);
        out
    }

    /// Reads a request. Refuses foreign traffic, unpadded requests and probe version 0. A request
    /// of a later probe version is read by its frozen prefix.
    pub fn decode(datagram: &[u8]) -> Result<Self, WireError> {
        if datagram.len() < PROBE_REQUEST_BYTES {
            return Err(WireError::Invalid("probe request is not padded"));
        }
        let mut r = WireReader::new(datagram);
        if r.array::<4>()? != PROBE_REQUEST_MAGIC {
            return Err(WireError::Invalid("not a probe request"));
        }
        if r.u8()? == 0 {
            return Err(WireError::Invalid("probe version"));
        }
        Ok(Self { nonce: r.u64()? })
    }
}

/// A probe answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeAnswer {
    /// The request's nonce.
    pub nonce: u64,
    /// The answering build's versions.
    pub stamp: VersionStamp,
}

impl ProbeAnswer {
    /// The datagram (at most [`MAX_PROBE_ANSWER_BYTES`]).
    pub fn to_datagram(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(MAX_PROBE_ANSWER_BYTES);
        w.raw(&PROBE_ANSWER_MAGIC);
        w.u8(PROBE_VERSION);
        w.u64(self.nonce);
        w.u32(self.stamp.wire);
        // Cannot fail: the text was cut to the limit.
        let _ = w.str_u8(
            cut(&self.stamp.game_version, MAX_GAME_VERSION_BYTES),
            MAX_GAME_VERSION_BYTES,
        );
        w.u32(self.stamp.protocol);
        w.u64(self.stamp.content_hash);
        w.finish()
    }

    /// Reads an answer. Bytes after the frozen fields (a later probe version's additions, within
    /// [`MAX_PROBE_ANSWER_BYTES`]) are ignored.
    pub fn decode(datagram: &[u8]) -> Result<Self, WireError> {
        if datagram.len() > MAX_PROBE_ANSWER_BYTES {
            return Err(WireError::Invalid("probe answer is too large"));
        }
        let mut r = WireReader::new(datagram);
        if r.array::<4>()? != PROBE_ANSWER_MAGIC {
            return Err(WireError::Invalid("not a probe answer"));
        }
        if r.u8()? == 0 {
            return Err(WireError::Invalid("probe version"));
        }
        let nonce = r.u64()?;
        let wire = r.u32()?;
        let game_version = r.str_u8(MAX_GAME_VERSION_BYTES)?.to_owned();
        let protocol = r.u32()?;
        let content_hash = r.u64()?;
        Ok(Self {
            nonce,
            stamp: VersionStamp {
                wire,
                game_version,
                protocol,
                content_hash,
            },
        })
    }
}

/// True when the datagram starts like a probe request (checked before any other parsing).
pub fn is_probe_request(datagram: &[u8]) -> bool {
    datagram.starts_with(&PROBE_REQUEST_MAGIC)
}

/// True when the datagram starts like a probe answer.
pub fn is_probe_answer(datagram: &[u8]) -> bool {
    datagram.starts_with(&PROBE_ANSWER_MAGIC)
}

/// The prefix a probe source is limited by: its IPv4 /24 or its IPv6 /56 network address. An
/// IPv4-mapped IPv6 address is its IPv4 address.
pub fn probe_prefix(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V4(v4) => {
            let host_bits = 32 - u32::from(IPV4_PROBE_PREFIX);
            IpAddr::V4(Ipv4Addr::from((u32::from(v4) >> host_bits) << host_bits))
        }
        IpAddr::V6(v6) => {
            let host_bits = 128 - u32::from(IPV6_PROBE_PREFIX);
            IpAddr::V6(Ipv6Addr::from((u128::from(v6) >> host_bits) << host_bits))
        }
    }
}

/// Answers probes for one socket: the stamp to send, the per-prefix limiter and the counters.
#[derive(Debug, Clone)]
pub struct ProbeResponder {
    stamp: VersionStamp,
    limiter: SourceLimiter,
    answered: u64,
    shed: u64,
}

impl ProbeResponder {
    /// A responder answering with `stamp`, at most [`PROBE_ANSWERS_PER_PREFIX_PER_S`] per prefix.
    pub fn new(stamp: VersionStamp) -> Self {
        Self::with_rate(stamp, PROBE_ANSWERS_PER_PREFIX_PER_S)
    }

    /// A responder with another per-prefix rate (`0` = unlimited, for tests and closed networks).
    pub fn with_rate(stamp: VersionStamp, per_prefix_per_s: u32) -> Self {
        Self::with_secret(stamp, per_prefix_per_s, crate::handshake::fresh_secret())
    }

    /// A responder whose limiter table is indexed under `key` (a server derives it from its secret,
    /// so a test with a fixed secret sees the same prefix collisions every run).
    pub fn with_secret(stamp: VersionStamp, per_prefix_per_s: u32, key: [u8; 32]) -> Self {
        Self {
            stamp,
            limiter: SourceLimiter::new(per_prefix_per_s, PROBE_LIMITER_SLOTS, key),
            answered: 0,
            shed: 0,
        }
    }

    /// The stamp answers carry.
    pub fn stamp(&self) -> &VersionStamp {
        &self.stamp
    }

    /// Replaces the stamp (a session host announces its manifest's versions).
    pub fn set_stamp(&mut self, stamp: VersionStamp) {
        self.stamp = stamp;
    }

    /// Probes answered.
    pub fn answered(&self) -> u64 {
        self.answered
    }

    /// Probes dropped by the per-prefix limit.
    pub fn shed(&self) -> u64 {
        self.shed
    }

    /// The answer to `datagram` from `from` at `now_ms`: `None` when it is not a valid probe
    /// request or its prefix is over its budget.
    pub fn answer(&mut self, datagram: &[u8], from: SocketAddr, now_ms: u64) -> Option<Vec<u8>> {
        let request = ProbeRequest::decode(datagram).ok()?;
        if !self.limiter.admit(probe_prefix(from.ip()), now_ms) {
            self.shed += 1;
            return None;
        }
        self.answered += 1;
        Some(
            ProbeAnswer {
                nonce: request.nonce,
                stamp: self.stamp.clone(),
            }
            .to_datagram(),
        )
    }
}

/// `s` cut to at most `max` bytes at a character boundary.
fn cut(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `1.2.10` as `[1, 2, 10]`; a leading `v` and a `-suffix` / `+build` are ignored. `None` for text
/// that is not dotted numbers.
fn parse_dotted(text: &str) -> Option<Vec<u64>> {
    let core = text.strip_prefix('v').unwrap_or(text);
    let core = core.split(['-', '+']).next()?;
    if core.is_empty() {
        return None;
    }
    let mut parts: Vec<u64> = core
        .split('.')
        .map(|p| p.parse::<u64>().ok())
        .collect::<Option<_>>()?;
    while parts.len() > 1 && parts.last() == Some(&0) {
        parts.pop();
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(game: &str, wire: u32, protocol: u32, content: u64) -> VersionStamp {
        VersionStamp::new(wire, game, protocol, content)
    }

    /// The probe bytes are frozen forever (spec 120 § 12.2): this pins every byte of a request and
    /// an answer. Changing either breaks every older build's probe; add a probe version instead.
    #[test]
    fn probe_bytes_are_pinned() {
        let request = ProbeRequest {
            nonce: 0x0102_0304_0506_0708,
        }
        .to_datagram();
        let mut expected = b"RGVP".to_vec();
        expected.push(1);
        expected.extend_from_slice(&[8, 7, 6, 5, 4, 3, 2, 1]);
        expected.resize(64, 0);
        assert_eq!(request, expected);

        let answer = ProbeAnswer {
            nonce: 0x0102_0304_0506_0708,
            stamp: stamp("1.2.0", 3, 7, 0x1122_3344_5566_7788),
        }
        .to_datagram();
        let mut expected = b"RGVR".to_vec();
        expected.push(1);
        expected.extend_from_slice(&[8, 7, 6, 5, 4, 3, 2, 1]);
        expected.extend_from_slice(&[3, 0, 0, 0]);
        expected.push(5);
        expected.extend_from_slice(b"1.2.0");
        expected.extend_from_slice(&[7, 0, 0, 0]);
        expected.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        assert_eq!(answer, expected);

        let mut tagged = WireWriter::new();
        stamp("1.2.0", 3, 7, 0x1122_3344_5566_7788).encode_tagged(&mut tagged);
        let mut expected = b"RGVS".to_vec();
        expected.extend_from_slice(&[3, 0, 0, 0, 7, 0, 0, 0]);
        expected.extend_from_slice(&[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
        expected.push(5);
        expected.extend_from_slice(b"1.2.0");
        assert_eq!(tagged.finish(), expected);
    }

    #[test]
    fn the_answer_is_never_larger_than_the_request() {
        let longest = ProbeAnswer {
            nonce: u64::MAX,
            stamp: stamp(&"9".repeat(200), u32::MAX, u32::MAX, u64::MAX),
        }
        .to_datagram();
        assert!(longest.len() <= MAX_PROBE_ANSWER_BYTES, "{}", longest.len());
        assert!(longest.len() <= PROBE_REQUEST_BYTES);
        assert_eq!(MAX_STAMP_BYTES, 53);
        let back = ProbeAnswer::decode(&longest).expect("answer");
        assert_eq!(back.stamp.game_version.len(), MAX_GAME_VERSION_BYTES);
    }

    #[test]
    fn requests_round_trip_and_short_ones_are_refused() {
        let request = ProbeRequest { nonce: 42 };
        let bytes = request.to_datagram();
        assert_eq!(ProbeRequest::decode(&bytes), Ok(request));
        assert!(ProbeRequest::decode(&bytes[..63]).is_err(), "unpadded");
        let mut zero = bytes.clone();
        zero[4] = 0;
        assert!(ProbeRequest::decode(&zero).is_err(), "probe version 0");
        let mut later = bytes;
        later[4] = 2;
        assert_eq!(
            ProbeRequest::decode(&later),
            Ok(request),
            "a later probe version keeps the prefix"
        );
    }

    #[test]
    fn answers_round_trip_and_tolerate_later_additions() {
        let answer = ProbeAnswer {
            nonce: 9,
            stamp: stamp("2.0.1-beta", 4, 11, 5),
        };
        let mut bytes = answer.to_datagram();
        assert_eq!(ProbeAnswer::decode(&bytes).as_ref(), Ok(&answer));
        bytes.extend_from_slice(&[0xAA; 4]);
        assert_eq!(ProbeAnswer::decode(&bytes).as_ref(), Ok(&answer));
        bytes.resize(MAX_PROBE_ANSWER_BYTES + 1, 0);
        assert!(
            ProbeAnswer::decode(&bytes).is_err(),
            "an answer may not outgrow the request"
        );
    }

    #[test]
    fn decoders_never_panic_on_any_prefix() {
        let answer = ProbeAnswer {
            nonce: 1,
            stamp: stamp("1.0", 3, 1, 2),
        }
        .to_datagram();
        for n in 0..answer.len() {
            let _ = ProbeAnswer::decode(&answer[..n]);
            let _ = ProbeRequest::decode(&answer[..n]);
            let _ = VersionStamp::from_tagged(&answer[..n]);
        }
    }

    #[test]
    fn sides_follow_game_version_then_wire_then_protocol() {
        let old = stamp("1.2.0", 3, 4, 0);
        let new = stamp("1.3.0", 3, 5, 0);
        assert_eq!(
            MismatchSide::between(Some(&old), Some(&new)),
            MismatchSide::ClientOlder
        );
        assert_eq!(
            MismatchSide::between(Some(&new), Some(&old)),
            MismatchSide::ServerOlder
        );
        // No game version: the wire version orders, then the protocol.
        let a = stamp("", 3, 9, 0);
        let b = stamp("", 4, 1, 0);
        assert_eq!(
            MismatchSide::between(Some(&a), Some(&b)),
            MismatchSide::ClientOlder
        );
        let c = stamp("", 3, 2, 0);
        assert_eq!(
            MismatchSide::between(Some(&a), Some(&c)),
            MismatchSide::ServerOlder
        );
        // Same versions, other content.
        let d = stamp("1.2.0", 3, 4, 7);
        assert_eq!(
            MismatchSide::between(Some(&old), Some(&d)),
            MismatchSide::ContentDiffers
        );
        assert_eq!(MismatchSide::ContentDiffers.server_is_older(), None);
        // Text that is not dotted numbers does not order.
        let e = stamp("nightly", 3, 4, 0);
        let f = stamp("stable", 3, 4, 0);
        assert_eq!(MismatchSide::between(Some(&e), Some(&f)), MismatchSide::Unordered);
        // A side without a stamp predates it.
        assert_eq!(MismatchSide::between(Some(&old), None), MismatchSide::ServerOlder);
        assert_eq!(MismatchSide::between(None, Some(&old)), MismatchSide::ClientOlder);
        assert_eq!(parse_dotted("v1.2.0"), parse_dotted("1.2"));
        assert!(parse_dotted("1.10") > parse_dotted("1.9"));
    }

    #[test]
    fn messages_name_both_versions_and_end_with_the_code() {
        let report = VersionMismatchReport::new(Some(stamp("1.2.0", 3, 4, 0)), Some(stamp("1.3.0", 3, 5, 0)));
        assert_eq!(report.server_is_older, Some(false));
        assert!(
            report.message.contains("1.3.0") && report.message.contains("1.2.0"),
            "{}",
            report.message
        );
        assert!(report.message.contains("update the game"), "{}", report.message);
        assert_eq!(
            MismatchSide::in_detail(&report.message),
            Some(MismatchSide::ClientOlder)
        );
        let old_server = VersionMismatchReport::new(Some(stamp("1.2.0", 3, 4, 0)), None);
        assert!(
            old_server.message.contains("the server is older than this build"),
            "{}",
            old_server.message
        );
        assert_eq!(
            MismatchSide::in_detail(&old_server.message),
            Some(MismatchSide::ServerOlder)
        );
    }

    #[test]
    fn prefixes_are_slash_24_and_slash_56() {
        let a: IpAddr = "203.0.113.7".parse().expect("ip");
        assert_eq!(probe_prefix(a), "203.0.113.0".parse::<IpAddr>().expect("ip"));
        let mapped: IpAddr = "::ffff:203.0.113.99".parse().expect("ip");
        assert_eq!(probe_prefix(mapped), probe_prefix(a));
        let v6: IpAddr = "2001:db8:1234:56ff:1:2:3:4".parse().expect("ip");
        assert_eq!(
            probe_prefix(v6),
            "2001:db8:1234:5600::".parse::<IpAddr>().expect("ip")
        );
    }

    /// The per-prefix limit (spec 120 § 12.2): one /24 gets its burst, then nothing until time
    /// refills it; another /24 is untouched; every address of a /56 shares one budget.
    #[test]
    fn answers_are_rate_limited_per_prefix() {
        let mut responder =
            ProbeResponder::with_secret(stamp("1.0", 3, 1, 0), PROBE_ANSWERS_PER_PREFIX_PER_S, [5; 32]);
        let request = ProbeRequest { nonce: 1 }.to_datagram();
        let burst = PROBE_ANSWERS_PER_PREFIX_PER_S as usize * 2;
        let answered = (0..burst * 4)
            .filter(|i| {
                let from = SocketAddr::from(([198, 51, 100, (*i % 250) as u8], 40_000));
                responder.answer(&request, from, 1_000).is_some()
            })
            .count();
        assert_eq!(answered, burst, "one /24 shares one bucket");
        assert_eq!(responder.shed(), (burst * 3) as u64);
        let other = SocketAddr::from(([198, 51, 101, 1], 40_000));
        assert!(
            responder.answer(&request, other, 1_000).is_some(),
            "the next /24 has its own budget"
        );
        let later = SocketAddr::from(([198, 51, 100, 3], 40_000));
        assert!(
            responder.answer(&request, later, 2_000).is_some(),
            "time refills the bucket"
        );

        let mut v6 =
            ProbeResponder::with_secret(stamp("1.0", 3, 1, 0), PROBE_ANSWERS_PER_PREFIX_PER_S, [5; 32]);
        let answered = (0..burst * 2)
            .filter(|i| {
                let ip: IpAddr = format!("2001:db8:1:{:x}::{:x}", *i % 256, i + 1)
                    .parse()
                    .expect("ip");
                v6.answer(&request, SocketAddr::new(ip, 9), 0).is_some()
            })
            .count();
        assert_eq!(answered, burst, "every /64 of one /56 shares one bucket");
        assert!(
            responder.answer(&request[..32], other, 5_000).is_none(),
            "an unpadded probe gets nothing"
        );
    }
}
