// SPDX-License-Identifier: MPL-2.0
//! Rendezvous relay: join codes, hole-punch introductions, an opaque forwarding fallback and a
//! small game directory.
//!
//! A host registers with the relay and gets a [join code](crate::join_code); a client looks the code
//! up and learns the host's public (reflexive) address. The relay can introduce both sides for a
//! UDP hole punch (each learns the other's reflexive address and both send at once, the technique
//! of Ford, Srisuresh and Kegel, "Peer-to-Peer Communication Across Network Address Translators",
//! USENIX 2005). When the punch fails (address-dependent mapping on one side, RFC 4787 § 4.1) the
//! client asks the relay to bind a flow and the relay forwards its datagrams to the host and back
//! without parsing them (the role TURN plays in RFC 8656). The relay never runs game code.
//!
//! [`RelayCore`] is sans-IO: [`RelayCore::handle`] takes one datagram and returns the datagrams to
//! send. [`RelayServer`] drives it over a [`DatagramSocket`]. [`SimNat`] and
//! [`simulate_hole_punch`] model NAT behavior so tests can check which pairs punch and which need
//! the relay.
//!
//! UDP source addresses can be forged, so a relay on a public address must not be a reflector
//! (ADR-1892): every control request that gets an answer is padded to at least the size of
//! everything sent back, one `ListGames` request gets one directory page, control traffic is
//! limited per source IP, and lobbies are capped globally and per IP. A `Register` proves its exact
//! source address, IP and port, with a cookie round trip
//! ([`crate::handshake::CookieMinter::cookie_for_addr`]), so forged sources cannot fill the lobby
//! table, and a cookie read on one port of a shared IP cannot register or renew another port's
//! lobby. A `Bind` proves knowledge of the key handed out with the join code and cannot displace a
//! flow in active use.
//!
//! Relay protocol 2 (spec 120 § 10.6, ADR-7982) gives each relayed client its own allocation: an
//! `Allocate` makes the relay open a port for that client alone, the TURN shape of RFC 8656 § 5, so
//! the host sees every relayed client as a distinct source address (`relay_ip:port`) and nothing
//! above the socket changes. An allocation forwards only between its permitted client and host
//! addresses (the permissions of RFC 8656 § 9); see [`RelayCore::handle_allocation`]. Every
//! protocol 2 control datagram carries a version byte: `RGNR | 0 | version | tag | body` (no
//! protocol 1 tag is 0, so the two framings never overlap). Protocol 1 requests keep their protocol 1
//! answers, and a protocol 1 request for an allocation operation gets
//! [`RelayReply::VersionTooOld`].
//!
//! Relay protocol 3 (spec 120 § 10.7, ADR-8340) adds the candidate exchange of NAT traversal
//! ([`crate::nat`]): a host's `Register` may carry its candidates (an extension in the padding,
//! which protocol 1 and 2 relays never read), and a protocol 3 `PunchIntro` names a join code and
//! the client's candidates. The relay resolves the code and introduces both sides at once
//! ([`RelayReply::Introduced`]): each learns the other's server-reflexive address (the one the
//! relay saw), its candidates and a fresh session key for their connectivity checks; the client
//! also gets the ticket and bind key a `Lookup` gives. The request is padded to
//! [`RELAY_INTRO_REQUEST_BYTES`] and the relay drops one whose two answers together would be
//! larger than it.

mod alloc;
mod host_check;

pub use alloc::{RelayAllocationView, RelayConfig, RelayReport, RelayStatus};

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use crate::addr_block::AddrBlock;
use crate::handshake::{CookieMinter, decode_addr, encode_addr, fresh_secret};
use crate::join_code::{mint_join_code, normalize_join_code};
use crate::nat::{
    Candidate, NAT_MAX_CANDIDATES, NAT_SESSION_KEY_BYTES, decode_candidates, encode_candidates,
};
use crate::rate_limit::SourceLimiter;
use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};
use crate::version_probe::{ProbeResponder, VersionStamp, is_probe_request};
use crate::wire::{WireError, WireReader, WireWriter, constant_time_eq};

/// Prefix of every relay control datagram.
pub const RELAY_MAGIC: [u8; 4] = *b"RGNR";
/// Default relay port.
pub const RELAY_DEFAULT_PORT: u16 = 7800;
/// Largest opaque connect token a host may deposit.
pub const RELAY_TOKEN_MAX_BYTES: usize = 256;
/// Largest directory listing.
pub const RELAY_LISTING_MAX_BYTES: usize = 512;
/// Listings the directory keeps.
pub const RELAY_DIRECTORY_CAP: usize = 64;
/// Largest directory page (one datagram).
pub const RELAY_DIRECTORY_PAGE_BYTES: usize = 1200;
/// Flows the relay forwards at most.
pub const RELAY_MAX_FLOWS: usize = 4096;
/// A flow idle this long is dropped.
pub const RELAY_FLOW_IDLE_MS: u64 = 60_000;
/// A flow idle this long may be replaced by another `Bind` for its host (a client that lost its
/// port comes back); a flow in active use is never displaced.
pub const RELAY_BIND_REPLACE_IDLE_MS: u64 = 5_000;
/// Bytes one direction of a flow forwards per second (and its burst); the rest is dropped.
pub const RELAY_FORWARD_BYTES_PER_SECOND: u64 = 4 * 1024 * 1024;
/// Bytes of the key that authorizes a `Bind`, handed out by `Lookup`.
pub const RELAY_BIND_KEY_BYTES: usize = 16;
/// Lobbies the relay keeps.
pub const RELAY_MAX_LOBBIES: usize = 4096;
/// Lobbies one source IP (or IPv6 /64) may hold.
pub const RELAY_MAX_LOBBIES_PER_IP: u32 = 8;
/// Control requests one source IP may make per second (and its burst, twice this).
pub const RELAY_SOURCE_REQUESTS_PER_SECOND: u32 = 32;
/// Source IPs the rate limiter tracks; new sources are refused while it is full.
pub const RELAY_MAX_TRACKED_SOURCES: usize = 65_536;
/// Longest lobby lifetime, seconds. A host renews before it ends; renewing keeps the join code.
pub const RELAY_MAX_LIFETIME_S: u16 = 3600;
/// Bytes of the cookie that proves a `Register`'s source address.
pub const RELAY_COOKIE_BYTES: usize = 16;
/// Least bytes of a `Register` request (the challenge and the join code replies are smaller).
pub const RELAY_REGISTER_REQUEST_BYTES: usize = 32;
/// Least bytes of a `Lookup` request (the `HostInfo` reply is smaller, whatever the token).
pub const RELAY_LOOKUP_REQUEST_BYTES: usize = 320;
/// Least bytes of a `PunchIntro` request (both notices together are smaller).
pub const RELAY_PUNCH_REQUEST_BYTES: usize = 64;
/// Least bytes of a `Bind` request (a refusal is smaller).
pub const RELAY_BIND_REQUEST_BYTES: usize = 64;
/// Least bytes of a `ListGames` request (a page is at most [`RELAY_DIRECTORY_PAGE_BYTES`]).
pub const RELAY_LIST_REQUEST_BYTES: usize = RELAY_DIRECTORY_PAGE_BYTES;
/// How long a client that looked a host up may be introduced to it by that host.
pub const RELAY_INTRO_WINDOW_MS: u64 = 30_000;
/// Recent lookups remembered for host-side introductions.
const RELAY_MAX_RECENT_LOOKUPS: usize = 16_384;
/// Least bytes of a `Publish` that can be answered (a refusal is six).
const RELAY_PUBLISH_REQUEST_BYTES: usize = 7;
/// Least time between two expiry passes over the tables.
const RELAY_EXPIRE_INTERVAL_MS: u64 = 1_000;
/// The relay control protocol this build speaks (spec 120 § 10.6): 2 adds allocations and the
/// version byte.
pub const RELAY_PROTOCOL_VERSION: u8 = 3;
/// Least protocol a join-code introduction with candidates needs (spec 120 § 10.7).
pub const RELAY_INTRO_MIN_VERSION: u8 = 3;
/// Least bytes of a protocol 3 `PunchIntro`: both introductions together (the largest ticket, IPv6
/// addresses and [`NAT_MAX_CANDIDATES`] candidates each) fit in it.
pub const RELAY_INTRO_REQUEST_BYTES: usize = 800;
/// The byte after a `Register`'s cookie that starts its candidate list (protocol 1 and 2 relays
/// read zero padding there and stop).
pub const RELAY_CANDIDATES_EXT: u8 = 0xCA;
/// The byte after a `Register`'s cookie that tells the relay not to publish this lobby's address
/// (ADR-10399). Protocol 1 and 2 relays read zero padding there and stop, so they ignore it.
/// A relay that understands it answers `HostInfo`, `Allocated` and `ClientJoined` with an
/// unspecified address. The UDP source is still the lobby's real endpoint, used only to forward.
pub const RELAY_HIDE_ADDR_EXT: u8 = 0xA1;
/// Least protocol an allocation operation needs.
pub const RELAY_ALLOCATION_MIN_VERSION: u8 = 2;
/// Least protocol the host check (port-forward probe, uplink measurement) needs (spec 120 § 10.7).
pub const RELAY_HOST_CHECK_MIN_VERSION: u8 = 3;
/// Bytes of a port-forward probe's nonce.
pub const RELAY_PROBE_NONCE_BYTES: usize = 8;
/// Least bytes of a `ProbeRequest` (its acknowledgement and the probe together are smaller).
pub const RELAY_PROBE_REQUEST_BYTES: usize = 64;
/// Bytes of one uplink measurement packet (the 1,200-byte floor of every path, spec 120 § 4.3).
pub const RELAY_MEASURE_BYTES: usize = 1200;
/// Measurement bytes the relay acknowledges per host per [`RELAY_MEASURE_WINDOW_MS`]; the rest is
/// dropped unanswered.
pub const RELAY_MEASURE_BYTES_PER_WINDOW: u64 = 16 * 1024 * 1024;
/// The measurement budget's window, milliseconds.
pub const RELAY_MEASURE_WINDOW_MS: u64 = 60_000;
/// Probes waiting for the relay's probe port at most.
const RELAY_MAX_QUEUED_PROBES: usize = 256;
/// The byte after [`RELAY_MAGIC`] that marks a versioned control datagram (protocol 2 and later).
/// Protocol 1 put its tag there, and no tag is 0.
pub const RELAY_VERSIONED: u8 = 0;
/// Bytes of a client's own allocation key (chosen by the client, kept by the relay) and of the
/// host's refresh key for one allocation.
pub const RELAY_ALLOCATION_KEY_BYTES: usize = 16;
/// Least bytes of an `Allocate` request: its answer and the host's notice together are smaller.
pub const RELAY_ALLOCATE_REQUEST_BYTES: usize = 128;
/// Least bytes of a `Refresh` request (its challenge and its answer are smaller).
pub const RELAY_REFRESH_REQUEST_BYTES: usize = 64;
/// Allocations the relay holds at most (its port pool), by default.
pub const RELAY_MAX_ALLOCATIONS: usize = 4096;
/// Relayed clients one host may have at once, by default (DO27: 16 per player host, 32 and
/// later 40 per community dedicated server).
pub const RELAY_MAX_CLIENTS_PER_HOST: u32 = 40;
/// Allocations one source IP (or IPv6 /64) may hold, by default.
pub const RELAY_MAX_ALLOCATIONS_PER_IP: u32 = 8;
/// Bytes per second one host's allocations forward together (both directions), by default.
pub const RELAY_HOST_BYTES_PER_SECOND: u64 = 64 * 1024 * 1024;

/// Control message tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayTag {
    /// Host → relay: register or renew a lobby (`lifetime_s u16`, token bytes, optional cookie).
    Register = 1,
    /// Client → relay: look up a code (six ASCII bytes, padded).
    Lookup = 2,
    /// Relay → client: the host's address, its token and the bind key.
    HostInfo = 3,
    /// Either → relay: introduce me to `addr` for a punch.
    PunchIntro = 4,
    /// Relay → both: punch toward this address now.
    PunchPeer = 5,
    /// Client → relay: forward my datagrams to the registered host at `addr` (with the bind key).
    Bind = 6,
    /// Relay → host: your join code.
    JoinCode = 7,
    /// Host → relay: publish a directory listing.
    Publish = 8,
    /// Anyone → relay: send directory page `page` (padded to the page size).
    ListGames = 9,
    /// Relay → asker: one directory page.
    Directory = 10,
    /// Relay → host: send your `Register` again with this cookie.
    RegisterChallenge = 11,
    /// Client → relay (protocol 2): give me my own allocation toward the registered host at `addr`
    /// (`addr`, bind key, my allocation key, optional cookie).
    Allocate = 12,
    /// Relay → client or host (protocol 2): you are permitted on allocation `port`, toward `addr`.
    Allocated = 13,
    /// Relay → host (protocol 2): a client has allocation `port` (`port`, client `addr`, the
    /// host's refresh key for it).
    ClientJoined = 14,
    /// Client or host → an allocation port (protocol 2): move my side of this allocation to the
    /// address this comes from (key, optional cookie).
    Refresh = 15,
    /// Relay → asker (protocol 2): send the `Allocate` or `Refresh` again with this cookie.
    Challenge = 16,
    /// Relay → client (protocol 2): refused, a cap of `cap` is reached.
    AllocationFull = 17,
    /// Relay → asker: this operation needs protocol `min` or later.
    VersionTooOld = 18,
    /// Relay → asker: this relay speaks protocol `max` at most.
    VersionTooNew = 19,
    /// Host → relay (protocol 3): send a probe to `target`, the host's mapped address (`nonce
    /// [8] | target`), from a relay port the host never sent to (spec 120 § 10.7).
    ProbeRequest = 20,
    /// Relay → host (protocol 3): the probe of `nonce` was sent (`nonce [8] | sent u8`; 0 when
    /// this relay has no probe port).
    ProbeSent = 21,
    /// Relay probe port → a host's mapped address (protocol 3): `nonce [8]`.
    ProbeHello = 22,
    /// Host → relay (protocol 3): one uplink measurement packet (`seq u32`, padded to
    /// [`RELAY_MEASURE_BYTES`]).
    Measure = 23,
    /// Relay → host (protocol 3): measurement packet `seq` arrived (`seq u32 | capped u8`).
    MeasureAck = 24,
    /// Relay → asker: refused (`code u8`).
    Error = 255,
}

/// Relay refusal codes carried by [`RelayTag::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RelayError {
    /// The join code is unknown or expired.
    UnknownCode = 1,
    /// The target is not a registered host.
    NotAHost = 2,
    /// Listing or token too large or empty.
    BadSize = 3,
    /// The directory or flow table is full.
    Full = 4,
    /// The sender must register first.
    NotRegistered = 5,
    /// The bind key is wrong.
    BadKey = 6,
    /// The host's relayed flow is in active use by another client.
    Busy = 7,
    /// A probe target is not on the requesting host's own address (protocol 3).
    ProbeTarget = 8,
}

impl RelayError {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::UnknownCode,
            2 => Self::NotAHost,
            3 => Self::BadSize,
            4 => Self::Full,
            5 => Self::NotRegistered,
            6 => Self::BadKey,
            7 => Self::Busy,
            8 => Self::ProbeTarget,
            _ => return None,
        })
    }
}

fn control(tag: RelayTag) -> WireWriter {
    control_v(tag, 1)
}

/// A control datagram head in the framing of `version` (protocol 1: `RGNR | tag`; later:
/// `RGNR | 0 | version | tag`).
fn control_v(tag: RelayTag, version: u8) -> WireWriter {
    let mut w = WireWriter::with_capacity(32);
    w.raw(&RELAY_MAGIC);
    if version >= 2 {
        w.u8(RELAY_VERSIONED);
        w.u8(version);
    }
    w.u8(tag as u8);
    w
}

/// The head of a control datagram: its protocol version, tag and where the body starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Frame {
    version: u8,
    tag: u8,
    body: usize,
}

/// Reads a control datagram's head; `None` when it is not relay control traffic.
fn frame(datagram: &[u8]) -> Option<Frame> {
    if datagram.len() < 5 || datagram[..4] != RELAY_MAGIC {
        return None;
    }
    if datagram[4] != RELAY_VERSIONED {
        return Some(Frame {
            version: 1,
            tag: datagram[4],
            body: 5,
        });
    }
    (datagram.len() >= 7).then(|| Frame {
        version: datagram[5],
        tag: datagram[6],
        body: 7,
    })
}

/// Tags that exist only from protocol 2 on.
fn allocation_tag(tag: u8) -> bool {
    (RelayTag::Allocate as u8..=RelayTag::VersionTooNew as u8).contains(&tag)
}

/// Tags of the host check that exist only from protocol 3 on.
fn host_check_tag(tag: u8) -> bool {
    (RelayTag::ProbeRequest as u8..=RelayTag::MeasureAck as u8).contains(&tag)
}

/// The protocol version of a relay control datagram (1 for the unversioned framing); `None`
/// when it is not relay control traffic.
pub fn relay_version(datagram: &[u8]) -> Option<u8> {
    frame(datagram).map(|f| f.version)
}

/// A protocol 1 request in protocol 2 framing (the version byte added, the body and its padding
/// kept). Bytes that are not a protocol 1 request come back unchanged.
pub fn versioned(request_v1: &[u8]) -> Vec<u8> {
    match frame(request_v1) {
        Some(f) if f.version == 1 => {
            let mut out = Vec::with_capacity(request_v1.len() + 2);
            out.extend_from_slice(&RELAY_MAGIC);
            // Protocol 2 framing (not the newest protocol): every versioned relay reads it, so a
            // `Lookup` sent this way is answered by a protocol 2 relay too (ADR-8340).
            out.extend_from_slice(&[RELAY_VERSIONED, RELAY_ALLOCATION_MIN_VERSION, f.tag]);
            out.extend_from_slice(&request_v1[5..]);
            out
        }
        _ => request_v1.to_vec(),
    }
}

/// Pads a request to `min` bytes, so the relay never answers with more than it was sent.
fn padded(w: WireWriter, min: usize) -> Vec<u8> {
    let mut bytes = w.finish();
    if bytes.len() < min {
        bytes.resize(min, 0);
    }
    bytes
}

/// `Register` datagram without a cookie: the relay answers with a
/// [`RelayReply::RegisterChallenge`] and registers nothing. Fails when the token is too large.
pub fn encode_register(lifetime_s: u16, token: &[u8]) -> Result<Vec<u8>, WireError> {
    let mut w = control(RelayTag::Register);
    w.u16(lifetime_s);
    w.bytes_u16(token, RELAY_TOKEN_MAX_BYTES)?;
    w.u8(0);
    Ok(padded(w, RELAY_REGISTER_REQUEST_BYTES))
}

/// `Register` datagram with the cookie of a [`RelayReply::RegisterChallenge`]: this one registers,
/// or renews the sender's lobby under the same join code. Fails when the token is too large.
pub fn encode_register_with_cookie(
    lifetime_s: u16,
    token: &[u8],
    cookie: &[u8; RELAY_COOKIE_BYTES],
) -> Result<Vec<u8>, WireError> {
    let mut w = control(RelayTag::Register);
    w.u16(lifetime_s);
    w.bytes_u16(token, RELAY_TOKEN_MAX_BYTES)?;
    w.u8(1);
    w.raw(cookie);
    Ok(padded(w, RELAY_REGISTER_REQUEST_BYTES))
}

/// `Register` datagram (protocol 1 framing, so every relay reads it) with the host's NAT traversal
/// candidates after the cookie (spec 120 § 10.7): a protocol 3 relay keeps them for the lobby and
/// hands them to each client it introduces; older relays read padding there. Fails when the token
/// is too large.
pub fn encode_register_with_candidates(
    lifetime_s: u16,
    token: &[u8],
    cookie: Option<&[u8; RELAY_COOKIE_BYTES]>,
    candidates: &[Candidate],
) -> Result<Vec<u8>, WireError> {
    let mut w = control(RelayTag::Register);
    w.u16(lifetime_s);
    w.bytes_u16(token, RELAY_TOKEN_MAX_BYTES)?;
    match cookie {
        Some(c) => {
            w.u8(1);
            w.raw(c);
        }
        None => w.u8(0),
    }
    if !candidates.is_empty() {
        w.u8(RELAY_CANDIDATES_EXT);
        encode_candidates(&mut w, candidates);
    }
    Ok(padded(w, RELAY_REGISTER_REQUEST_BYTES))
}

/// `Register` that asks the relay not to publish this socket's address (ADR-10399).
///
/// The hide byte follows the cookie. No candidates are written: a hidden lobby has none to
/// hand out. An older relay treats the byte as padding and still registers the UDP source.
/// Fails when the token is too large.
pub fn encode_register_hidden(
    lifetime_s: u16,
    token: &[u8],
    cookie: Option<&[u8; RELAY_COOKIE_BYTES]>,
) -> Result<Vec<u8>, WireError> {
    let mut w = control(RelayTag::Register);
    w.u16(lifetime_s);
    w.bytes_u16(token, RELAY_TOKEN_MAX_BYTES)?;
    match cookie {
        Some(c) => {
            w.u8(1);
            w.raw(c);
        }
        None => w.u8(0),
    }
    w.u8(RELAY_HIDE_ADDR_EXT);
    Ok(padded(w, RELAY_REGISTER_REQUEST_BYTES))
}

/// Protocol 3 `PunchIntro` that resolves a join code (spec 120 § 10.7): the relay answers with a
/// [`RelayReply::Introduced`] for the client (the host's address, ticket, bind key, candidates and
/// a session key) and sends the host one for this client. `None` when `code` is not a join code.
/// Padded to [`RELAY_INTRO_REQUEST_BYTES`].
pub fn encode_introduce(code: &str, candidates: &[Candidate]) -> Option<Vec<u8>> {
    let code = normalize_join_code(code)?;
    let mut w = control_v(RelayTag::PunchIntro, RELAY_PROTOCOL_VERSION);
    w.raw(code.as_bytes());
    encode_candidates(&mut w, candidates);
    Some(padded(w, RELAY_INTRO_REQUEST_BYTES))
}

/// `Lookup` datagram; `None` when `code` is not a join code.
pub fn encode_lookup(code: &str) -> Option<Vec<u8>> {
    let code = normalize_join_code(code)?;
    let mut w = control(RelayTag::Lookup);
    w.raw(code.as_bytes());
    Some(padded(w, RELAY_LOOKUP_REQUEST_BYTES))
}

/// `PunchIntro` datagram toward `peer`.
pub fn encode_punch_intro(peer: SocketAddr) -> Vec<u8> {
    let mut w = control(RelayTag::PunchIntro);
    encode_addr(&mut w, peer);
    padded(w, RELAY_PUNCH_REQUEST_BYTES)
}

/// `Bind` datagram toward the registered host `host`, with the key `Lookup` returned.
pub fn encode_bind(host: SocketAddr, key: &[u8; RELAY_BIND_KEY_BYTES]) -> Vec<u8> {
    let mut w = control(RelayTag::Bind);
    encode_addr(&mut w, host);
    w.raw(key);
    padded(w, RELAY_BIND_REQUEST_BYTES)
}

/// `Allocate` datagram (protocol 2) toward the registered host `host`, with the bind key
/// `Lookup` returned and the client's own `client_key` (random, kept secret: it moves this
/// allocation when the client's address changes). Without a cookie the relay answers
/// [`RelayReply::Challenge`]; send it again with that cookie.
pub fn encode_allocate(
    host: SocketAddr,
    bind_key: &[u8; RELAY_BIND_KEY_BYTES],
    client_key: &[u8; RELAY_ALLOCATION_KEY_BYTES],
    cookie: Option<&[u8; RELAY_COOKIE_BYTES]>,
) -> Vec<u8> {
    // Protocol 2 framing: an allocation is a protocol 2 operation, so a protocol 2 relay reads it.
    let mut w = control_v(RelayTag::Allocate, RELAY_ALLOCATION_MIN_VERSION);
    encode_addr(&mut w, host);
    w.raw(bind_key);
    w.raw(client_key);
    match cookie {
        Some(c) => {
            w.u8(1);
            w.raw(c);
        }
        None => w.u8(0),
    }
    padded(w, RELAY_ALLOCATE_REQUEST_BYTES)
}

/// `Refresh` datagram (protocol 2), sent to an allocation port: moves the sender's side of that
/// allocation to the address it comes from. `key` is the client's allocation key, or the host's
/// refresh key from [`RelayReply::ClientJoined`]. Without a cookie the relay answers
/// [`RelayReply::Challenge`] from the allocation port.
pub fn encode_refresh(
    key: &[u8; RELAY_ALLOCATION_KEY_BYTES],
    cookie: Option<&[u8; RELAY_COOKIE_BYTES]>,
) -> Vec<u8> {
    let mut w = control_v(RelayTag::Refresh, RELAY_ALLOCATION_MIN_VERSION);
    w.raw(key);
    match cookie {
        Some(c) => {
            w.u8(1);
            w.raw(c);
        }
        None => w.u8(0),
    }
    padded(w, RELAY_REFRESH_REQUEST_BYTES)
}

/// `Publish` datagram. Fails when the listing is too large.
pub fn encode_publish(listing: &[u8]) -> Result<Vec<u8>, WireError> {
    let mut w = control(RelayTag::Publish);
    w.bytes_u16(listing, RELAY_LISTING_MAX_BYTES)?;
    Ok(w.finish())
}

/// `ListGames` datagram for the first directory page.
pub fn encode_list_games() -> Vec<u8> {
    encode_list_games_page(0)
}

/// `ListGames` datagram for page `page`: one request, one page. A reply names the page count, so
/// a client asks for the others one at a time.
pub fn encode_list_games_page(page: u8) -> Vec<u8> {
    let mut w = control(RelayTag::ListGames);
    w.u8(page);
    padded(w, RELAY_LIST_REQUEST_BYTES)
}

/// A protocol 3 `ProbeRequest`: probe `target` (the host's mapped address) with `nonce`, padded to
/// [`RELAY_PROBE_REQUEST_BYTES`].
pub fn encode_probe_request(nonce: &[u8; RELAY_PROBE_NONCE_BYTES], target: SocketAddr) -> Vec<u8> {
    let mut w = control_v(RelayTag::ProbeRequest, RELAY_HOST_CHECK_MIN_VERSION);
    w.raw(nonce);
    encode_addr(&mut w, target);
    padded(w, RELAY_PROBE_REQUEST_BYTES)
}

/// A protocol 3 uplink measurement packet `seq`, [`RELAY_MEASURE_BYTES`] long.
pub fn encode_measure(seq: u32) -> Vec<u8> {
    let mut w = control_v(RelayTag::Measure, RELAY_HOST_CHECK_MIN_VERSION);
    w.u32(seq);
    padded(w, RELAY_MEASURE_BYTES)
}

/// A decoded reply from the relay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayReply {
    /// Our join code.
    JoinCode(String),
    /// Prove the source address: send the `Register` again with this cookie.
    RegisterChallenge([u8; RELAY_COOKIE_BYTES]),
    /// A looked-up host.
    HostInfo {
        /// Its reflexive address. Unspecified, port 0, when the lobby registered with
        /// [`RELAY_HIDE_ADDR_EXT`].
        addr: SocketAddr,
        /// The opaque token it deposited.
        token: Vec<u8>,
        /// The key [`encode_bind`] needs.
        bind_key: [u8; RELAY_BIND_KEY_BYTES],
    },
    /// Punch toward this address.
    PunchPeer(SocketAddr),
    /// One directory page: `page` of `pages`, with its listings.
    Directory {
        /// Page index.
        page: u8,
        /// Page count.
        pages: u8,
        /// Listings on this page.
        listings: Vec<Vec<u8>>,
    },
    /// Refused.
    Error(RelayError),
    /// Protocol 2: you may use allocation `port` on the relay's address, toward `peer` (the host
    /// for a client, the client for a host).
    Allocated {
        /// The allocation's port on the relay.
        port: u16,
        /// The other side. Unspecified when the lobby hid its address.
        peer: SocketAddr,
    },
    /// Protocol 2, to the host: `client` has allocation `port`; it reaches you from
    /// `relay_ip:port`. Send to that address from the game socket (to open your NAT toward it),
    /// and [`encode_refresh`] with `host_key` there when your game socket is not the registered one.
    ClientJoined {
        /// The allocation's port on the relay.
        port: u16,
        /// The client's address as the relay sees it. Unspecified when the lobby hid its address.
        client: SocketAddr,
        /// The key that moves the host side of this allocation.
        host_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
    },
    /// Protocol 2: prove the source address, send the request again with this cookie.
    Challenge([u8; RELAY_COOKIE_BYTES]),
    /// Protocol 2: refused, a cap of `cap` is reached (the host's clients, the allocations of one
    /// source IP, or the relay's port pool; 0 when this relay has no allocation ports).
    Full {
        /// The cap.
        cap: u16,
    },
    /// The operation needs protocol `min` or later.
    VersionTooOld {
        /// Least protocol version.
        min: u8,
    },
    /// The relay speaks protocol `max` at most.
    VersionTooNew {
        /// Highest protocol version.
        max: u8,
    },
    /// Protocol 3: an introduction for NAT traversal (spec 120 § 10.7), to each side of a join by
    /// code.
    Introduced(Introduction),
    /// Protocol 3, to a host: the port-forward probe `nonce` was sent (`sent` false: this relay has
    /// no probe port, so it cannot check).
    ProbeSent {
        /// The request's nonce.
        nonce: [u8; RELAY_PROBE_NONCE_BYTES],
        /// Whether a probe left the relay's probe port.
        sent: bool,
    },
    /// Protocol 3: a port-forward probe, from the relay's probe port to a host's mapped address.
    ProbeHello {
        /// The request's nonce.
        nonce: [u8; RELAY_PROBE_NONCE_BYTES],
    },
    /// Protocol 3, to a host: uplink measurement packet `seq` arrived.
    MeasureAck {
        /// The packet's sequence number.
        seq: u32,
        /// The host's measurement budget for this window is spent; later packets go unanswered.
        capped: bool,
    },
}

/// One side's introduction to the other (relay protocol 3, [`RelayReply::Introduced`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Introduction {
    /// The other side as the relay saw it (its server-reflexive address).
    pub peer: SocketAddr,
    /// The host's ticket for a client; empty for the host.
    pub token: Vec<u8>,
    /// The lobby's bind key for a client (an allocation needs it); zero for the host.
    pub bind_key: [u8; RELAY_BIND_KEY_BYTES],
    /// The key both sides' connectivity checks carry.
    pub session: [u8; NAT_SESSION_KEY_BYTES],
    /// This side as the relay saw it (its own server-reflexive address).
    pub observed: SocketAddr,
    /// The other side's announced candidates.
    pub candidates: Vec<Candidate>,
}

impl Introduction {
    fn encode(&self, version: u8) -> Vec<u8> {
        let mut w = control_v(RelayTag::PunchPeer, version);
        encode_addr(&mut w, self.peer);
        // The token was size-checked on registration.
        let _ = w.bytes_u16(&self.token, RELAY_TOKEN_MAX_BYTES);
        w.raw(&self.bind_key);
        w.raw(&self.session);
        encode_addr(&mut w, self.observed);
        encode_candidates(&mut w, &self.candidates);
        w.finish()
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, WireError> {
        Ok(Self {
            peer: decode_addr(r)?,
            token: r.bytes_u16(RELAY_TOKEN_MAX_BYTES)?.to_vec(),
            bind_key: r.array()?,
            session: r.array()?,
            observed: decode_addr(r)?,
            candidates: decode_candidates(r)?,
        })
    }
}

impl RelayReply {
    /// Decodes a relay reply; `Ok(None)` for datagrams that are not relay control traffic.
    /// Both framings decode (protocol 1 and the versioned one).
    pub fn decode(datagram: &[u8]) -> Result<Option<Self>, WireError> {
        let Some(head) = frame(datagram) else {
            return Ok(None);
        };
        let mut r = WireReader::new(&datagram[head.body..]);
        let reply = match head.tag {
            t if t == RelayTag::JoinCode as u8 => {
                let code = std::str::from_utf8(r.take(6)?).map_err(|_| WireError::Utf8)?;
                Self::JoinCode(normalize_join_code(code).ok_or(WireError::Invalid("join code"))?)
            }
            t if t == RelayTag::RegisterChallenge as u8 => Self::RegisterChallenge(r.array()?),
            t if t == RelayTag::HostInfo as u8 => Self::HostInfo {
                addr: decode_addr(&mut r)?,
                token: r.bytes_u16(RELAY_TOKEN_MAX_BYTES)?.to_vec(),
                bind_key: r.array::<RELAY_BIND_KEY_BYTES>()?,
            },
            t if t == RelayTag::PunchPeer as u8 && head.version >= RELAY_INTRO_MIN_VERSION => {
                Self::Introduced(Introduction::decode(&mut r)?)
            }
            t if t == RelayTag::PunchPeer as u8 => Self::PunchPeer(decode_addr(&mut r)?),
            t if t == RelayTag::Directory as u8 => {
                let page = r.u8()?;
                let pages = r.u8()?;
                let count = usize::from(r.u8()?);
                let mut listings = Vec::with_capacity(count.min(RELAY_DIRECTORY_CAP));
                for _ in 0..count {
                    listings.push(r.bytes_u16(RELAY_LISTING_MAX_BYTES)?.to_vec());
                }
                Self::Directory {
                    page,
                    pages,
                    listings,
                }
            }
            t if t == RelayTag::Error as u8 => {
                Self::Error(RelayError::from_u8(r.u8()?).ok_or(WireError::Invalid("relay error code"))?)
            }
            t if t == RelayTag::Allocated as u8 => Self::Allocated {
                port: r.u16()?,
                peer: decode_addr(&mut r)?,
            },
            t if t == RelayTag::ClientJoined as u8 => Self::ClientJoined {
                port: r.u16()?,
                client: decode_addr(&mut r)?,
                host_key: r.array()?,
            },
            t if t == RelayTag::Challenge as u8 => Self::Challenge(r.array()?),
            t if t == RelayTag::AllocationFull as u8 => Self::Full { cap: r.u16()? },
            t if t == RelayTag::VersionTooOld as u8 => Self::VersionTooOld { min: r.u8()? },
            t if t == RelayTag::VersionTooNew as u8 => Self::VersionTooNew { max: r.u8()? },
            t if t == RelayTag::ProbeSent as u8 => Self::ProbeSent {
                nonce: r.array()?,
                sent: r.u8()? != 0,
            },
            t if t == RelayTag::ProbeHello as u8 => Self::ProbeHello { nonce: r.array()? },
            t if t == RelayTag::MeasureAck as u8 => Self::MeasureAck {
                seq: r.u32()?,
                capped: r.u8()? != 0,
            },
            _ => return Ok(None),
        };
        r.finish()?;
        Ok(Some(reply))
    }
}

#[derive(Debug, Clone)]
struct Lobby {
    host: SocketAddr,
    token: Vec<u8>,
    bind_key: [u8; RELAY_BIND_KEY_BYTES],
    expires_ms: u64,
    /// The host's NAT traversal candidates (protocol 3 `Register` extension).
    candidates: Vec<Candidate>,
    /// `Register` carried [`RELAY_HIDE_ADDR_EXT`]. Replies name no address. Forwarding still
    /// uses `host`.
    hide_addr: bool,
}

/// An address that names nobody. Hidden replies publish this instead of a UDP source.
fn unpublished_addr() -> SocketAddr {
    SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0))
}

#[derive(Debug, Clone, Copy)]
struct Flow {
    dst: SocketAddr,
    last_used_ms: u64,
    /// When this direction's own sender last sent.
    last_sent_ms: u64,
    /// This entry's sender is the host.
    host_side: bool,
    /// Bytes this direction may still forward; refills at [`RELAY_FORWARD_BYTES_PER_SECOND`].
    credit: u64,
    credit_ms: u64,
}

impl Flow {
    fn new(dst: SocketAddr, now_ms: u64, host_side: bool) -> Self {
        Self {
            dst,
            last_used_ms: now_ms,
            last_sent_ms: now_ms,
            host_side,
            credit: RELAY_FORWARD_BYTES_PER_SECOND,
            credit_ms: now_ms,
        }
    }

    /// Spends `bytes` of credit; false when the flow is over its rate.
    fn spend(&mut self, bytes: usize, now_ms: u64) -> bool {
        let elapsed = now_ms.saturating_sub(self.credit_ms);
        let gained = elapsed.saturating_mul(RELAY_FORWARD_BYTES_PER_SECOND) / 1000;
        self.credit = self
            .credit
            .saturating_add(gained)
            .min(RELAY_FORWARD_BYTES_PER_SECOND);
        self.credit_ms = now_ms;
        let bytes = bytes as u64;
        if self.credit < bytes {
            return false;
        }
        self.credit -= bytes;
        true
    }
}

/// Relay state. Holds lobbies, flows and listings; never game state.
pub struct RelayCore {
    lobbies: HashMap<String, Lobby>,
    /// Join code of each registered host.
    hosts: HashMap<SocketAddr, String>,
    /// Lobbies per source block (an IPv4 address or an IPv6 /64, ADR-3283).
    per_ip: HashMap<AddrBlock, u32>,
    flows: HashMap<SocketAddr, Flow>,
    directory: Vec<(SocketAddr, Vec<u8>)>,
    sources: SourceLimiter,
    /// Who looked which host up lately, and until when that counts.
    lookups: HashMap<SocketAddr, (SocketAddr, u64)>,
    next_expire_ms: u64,
    next_seed: u64,
    secret: [u8; 32],
    cookies: CookieMinter,
    /// Protocol 2 allocations (one relay port per client).
    alloc: alloc::Allocations,
    /// Introductions made so far (the session keys' counter).
    intro_seq: u64,
    /// Protocol 3 introductions made.
    pub introductions: u64,
    /// Datagrams forwarded through flows and allocations.
    pub forwarded: u64,
    /// Datagrams a flow or an allocation dropped for going over its own rate
    /// ([`RELAY_FORWARD_BYTES_PER_SECOND`] by default).
    pub forward_dropped: u64,
    /// Control requests dropped by the per-source limit (unanswered, a forged source must not
    /// get a reply).
    pub rate_limited: u64,
    /// Answers the version probe of sources outside a flow (spec 120 § 12.2, ADR-7985); a
    /// probe inside a flow is forwarded, so the host answers it with the game's versions.
    probe: ProbeResponder,
    /// The relay has a probe port ([`RelayServer::with_probe_socket`]): port-forward probes are
    /// queued for it.
    probe_capable: bool,
    /// Probes waiting for the probe port.
    probes: Vec<(Vec<u8>, SocketAddr)>,
    /// Measurement bytes per registered host: `(window start, bytes)`.
    measure: HashMap<SocketAddr, (u64, u64)>,
    /// Port-forward probes sent (protocol 3).
    pub probes_sent: u64,
    /// Uplink measurement packets acknowledged (protocol 3).
    pub measure_acked: u64,
    /// Uplink measurement packets dropped: not from a registered host, or over its budget.
    pub measure_dropped: u64,
}

impl std::fmt::Debug for RelayCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayCore")
            .field("lobbies", &self.lobbies.len())
            .field("flows", &self.flows.len())
            .field("allocations", &self.allocation_count())
            .field("listings", &self.directory.len())
            .field("forwarded", &self.forwarded)
            .field("forward_dropped", &self.forward_dropped)
            .field("rate_limited", &self.rate_limited)
            .finish_non_exhaustive()
    }
}

impl Default for RelayCore {
    fn default() -> Self {
        Self::new(0)
    }
}

impl RelayCore {
    /// An empty relay with a fresh secret. `seed` is where the relay's code counter starts; the
    /// codes themselves come from the secret (a keyed hash of the counter), so they cannot be
    /// predicted from the seed, from the public [`mint_join_code`] or from other codes.
    pub fn new(seed: u64) -> Self {
        Self::with_secret(seed, fresh_secret())
    }

    /// An empty relay with a fixed secret for its join codes and bind keys (deterministic tests).
    /// It has the default [`RelayConfig`] and no allocation ports until
    /// [`RelayCore::add_allocation_ports`].
    pub fn with_secret(seed: u64, secret: [u8; 32]) -> Self {
        Self::with_config(seed, secret, RelayConfig::default())
    }

    /// An empty relay with a fixed secret and the operator settings `config`.
    pub fn with_config(seed: u64, secret: [u8; 32], config: RelayConfig) -> Self {
        Self {
            lobbies: HashMap::new(),
            hosts: HashMap::new(),
            per_ip: HashMap::new(),
            flows: HashMap::new(),
            directory: Vec::new(),
            sources: SourceLimiter::new(
                RELAY_SOURCE_REQUESTS_PER_SECOND,
                RELAY_MAX_TRACKED_SOURCES,
                blake3::derive_key("regia_net relay limiter v1", &secret),
            ),
            lookups: HashMap::new(),
            next_expire_ms: 0,
            next_seed: seed,
            secret,
            cookies: CookieMinter::new(blake3::derive_key("regia_net relay cookie v1", &secret)),
            alloc: alloc::Allocations::new(config, &secret),
            intro_seq: 0,
            introductions: 0,
            forwarded: 0,
            forward_dropped: 0,
            rate_limited: 0,
            probe: ProbeResponder::with_secret(
                VersionStamp::new(crate::payload::REGIA_NET_WIRE_VERSION, "", 0, 0),
                crate::version_probe::PROBE_ANSWERS_PER_PREFIX_PER_S,
                blake3::derive_key("regia_net relay probe limiter v1", &secret),
            ),
            probe_capable: false,
            probes: Vec::new(),
            measure: HashMap::new(),
            probes_sent: 0,
            measure_acked: 0,
            measure_dropped: 0,
        }
    }

    /// Stops reserving a new allocation. A client key that already holds a port can still
    /// `Allocate` again (a lost answer) and `Refresh` is unchanged. A new key is answered
    /// [`RelayReply::Full`] (NL-11). The relay does not drop allocations it already issued.
    pub fn stop_new_allocations(&mut self) {
        self.alloc.stop_new();
    }

    /// Replaces the per-source control limit. `0` admits every request. The hub's `hub.toml`
    /// sets this from `relay.per_address_per_sec` (default 30). The relay's own default stays
    /// [`RELAY_SOURCE_REQUESTS_PER_SECOND`].
    pub fn set_source_rate(&mut self, per_sec: u32) {
        self.sources = SourceLimiter::new(
            per_sec,
            RELAY_MAX_TRACKED_SOURCES,
            blake3::derive_key("regia_net relay limiter v1", &self.secret),
        );
    }

    /// The versions this relay announces to a version probe from outside a flow: this build's
    /// wire version, no game (ADR-7985).
    pub fn probe_responder(&self) -> &ProbeResponder {
        &self.probe
    }

    /// Lobbies registered and not expired at the last expiry pass.
    pub fn lobby_count(&self) -> usize {
        self.lobbies.len()
    }

    /// Bound flows (each direction counts once).
    pub fn flow_count(&self) -> usize {
        self.flows.len() / 2
    }

    /// Listings in the directory.
    pub fn listing_count(&self) -> usize {
        self.directory.len()
    }

    /// The live lobby of `host`.
    fn lobby_of(&self, host: SocketAddr, now_ms: u64) -> Option<&Lobby> {
        let lobby = self.lobbies.get(self.hosts.get(&host)?)?;
        (lobby.expires_ms > now_ms).then_some(lobby)
    }

    /// The live hidden lobby whose bind key is `bind_key`.
    ///
    /// Every lobby's key is compared, so the time taken does not say which key was close.
    /// The last match wins. A lobby that did not ask to hide its address is not returned:
    /// an unspecified `Allocate` must not resolve a lobby that published its address.
    fn hidden_lobby(&self, bind_key: &[u8; RELAY_BIND_KEY_BYTES], now_ms: u64) -> Option<&Lobby> {
        let mut found = None;
        for lobby in self.lobbies.values() {
            if constant_time_eq(&lobby.bind_key, bind_key) && lobby.hide_addr && lobby.expires_ms > now_ms {
                found = Some(lobby);
            }
        }
        found
    }

    /// The lobby an `Allocate` names. An unspecified address resolves through [`Self::hidden_lobby`].
    fn allocate_lobby(
        &self,
        asked: SocketAddr,
        bind_key: &[u8; RELAY_BIND_KEY_BYTES],
        now_ms: u64,
    ) -> Option<&Lobby> {
        if asked.ip().is_unspecified() {
            self.hidden_lobby(bind_key, now_ms)
        } else {
            self.lobby_of(asked, now_ms)
        }
    }

    fn remove_lobby(&mut self, code: &str) {
        let Some(lobby) = self.lobbies.remove(code) else {
            return;
        };
        if self.hosts.get(&lobby.host).is_some_and(|c| c == code) {
            self.hosts.remove(&lobby.host);
        }
        let block = AddrBlock::of(lobby.host.ip());
        if let Some(n) = self.per_ip.get_mut(&block) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.per_ip.remove(&block);
            }
        }
    }

    /// Drops expired lobbies, their listings and idle flows.
    pub fn expire(&mut self, now_ms: u64) {
        let dead: Vec<String> = self
            .lobbies
            .iter()
            .filter(|(_, l)| l.expires_ms <= now_ms)
            .map(|(code, _)| code.clone())
            .collect();
        for code in dead {
            self.remove_lobby(&code);
        }
        let hosts = &self.hosts;
        self.directory.retain(|(host, _)| hosts.contains_key(host));
        self.flows
            .retain(|_, f| now_ms.saturating_sub(f.last_used_ms) < RELAY_FLOW_IDLE_MS);
        self.lookups.retain(|_, (_, until)| *until > now_ms);
        let hosts = &self.hosts;
        self.measure.retain(|host, _| hosts.contains_key(host));
        self.alloc.expire(now_ms);
        self.next_expire_ms = now_ms.saturating_add(RELAY_EXPIRE_INTERVAL_MS);
    }

    /// The join code for counter value `n`: a keyed hash of the counter under the relay's secret,
    /// spelled in the code alphabet. A code is a capability (`Lookup` hands out the host's address
    /// and bind key), so it must not be guessable from the counter or from earlier codes.
    fn join_code(&self, n: u64) -> String {
        let key = blake3::derive_key("regia_net relay join code v1", &self.secret);
        let hash = blake3::keyed_hash(&key, &n.to_le_bytes());
        let mut word = [0u8; 8];
        word.copy_from_slice(&hash.as_bytes()[..8]);
        mint_join_code(u64::from_le_bytes(word))
    }

    /// The key that authorizes a `Bind` to the lobby `code` of `host`.
    fn bind_key(&self, code: &str, host: SocketAddr) -> [u8; RELAY_BIND_KEY_BYTES] {
        let mut w = WireWriter::with_capacity(32);
        w.raw(code.as_bytes());
        encode_addr(&mut w, host);
        let hash = blake3::keyed_hash(&self.secret, w.as_slice());
        let mut key = [0u8; RELAY_BIND_KEY_BYTES];
        key.copy_from_slice(&hash.as_bytes()[..RELAY_BIND_KEY_BYTES]);
        key
    }

    /// Handles one datagram from `from`; returns what to send. Bound flows forward first, without
    /// parsing, so game bytes that happen to look like a control message are never misread. The
    /// one exception is a host's own `Register` and `Publish`, on the host side of a flow or with a
    /// live lobby: a client that binds must not be able to swallow its host's renewals.
    ///
    /// Requests come in either framing; each answer uses the framing of its request. A protocol 1
    /// request for an allocation operation is answered [`RelayReply::VersionTooOld`], a request
    /// from a newer protocol [`RelayReply::VersionTooNew`]: an old or new peer is never met with
    /// silence. Allocation traffic arrives on the allocation ports instead
    /// ([`RelayCore::handle_allocation`]).
    pub fn handle(&mut self, from: SocketAddr, datagram: &[u8], now_ms: u64) -> Vec<(Vec<u8>, SocketAddr)> {
        let head = frame(datagram);
        let host_control = head
            .is_some_and(|h| h.tag == RelayTag::Register as u8 || h.tag == RelayTag::Publish as u8)
            && (self.lobby_of(from, now_ms).is_some() || self.flows.get(&from).is_some_and(|f| f.host_side));
        if !host_control && let Some(flow) = self.flows.get_mut(&from) {
            flow.last_used_ms = now_ms;
            flow.last_sent_ms = now_ms;
            let dst = flow.dst;
            let allowed = flow.spend(datagram.len(), now_ms);
            if let Some(back) = self.flows.get_mut(&dst) {
                back.last_used_ms = now_ms;
            }
            if !allowed {
                self.forward_dropped += 1;
                return Vec::new();
            }
            self.forwarded += 1;
            return vec![(datagram.to_vec(), dst)];
        }
        if is_probe_request(datagram) {
            return self
                .probe
                .answer(datagram, from, now_ms)
                .map(|answer| vec![(answer, from)])
                .unwrap_or_default();
        }
        let Some(head) = head else {
            return Vec::new();
        };
        if now_ms >= self.next_expire_ms {
            self.expire(now_ms);
        }
        if head.tag == RelayTag::Measure as u8 && head.version >= RELAY_HOST_CHECK_MIN_VERSION {
            // A registered host's measurement runs at its uplink's rate, so it has its own byte
            // budget instead of the per-source request limit.
            return self.measure(from, datagram, head.body, now_ms);
        }
        if !self.sources.admit(from.ip(), now_ms) {
            self.rate_limited += 1;
            return Vec::new();
        }
        let version = head.version;
        if version > RELAY_PROTOCOL_VERSION {
            // Answer in the newest framing this relay knows; the reply is no larger than the head.
            if datagram.len() < 8 {
                return Vec::new();
            }
            self.alloc.status.version_refused += 1;
            let mut w = control_v(RelayTag::VersionTooNew, RELAY_PROTOCOL_VERSION);
            w.u8(RELAY_PROTOCOL_VERSION);
            return vec![(w.finish(), from)];
        }
        if head.body > 5 && version < 2 {
            // The versioned framing starts at protocol 2.
            return Vec::new();
        }
        if version < RELAY_HOST_CHECK_MIN_VERSION && host_check_tag(head.tag) {
            if datagram.len() < 6 {
                return Vec::new();
            }
            let mut w = control_v(RelayTag::VersionTooOld, version);
            w.u8(RELAY_HOST_CHECK_MIN_VERSION);
            return vec![(w.finish(), from)];
        }
        if version < RELAY_ALLOCATION_MIN_VERSION && allocation_tag(head.tag) {
            if datagram.len() < 6 {
                return Vec::new();
            }
            self.alloc.status.version_refused += 1;
            let mut w = control_v(RelayTag::VersionTooOld, version);
            w.u8(RELAY_ALLOCATION_MIN_VERSION);
            return vec![(w.finish(), from)];
        }
        let tag = head.tag;
        // The minimum sizes count the protocol 1 head; a versioned one is two bytes longer, and so
        // is its answer.
        let extra = head.body - 5;
        let mut r = WireReader::new(&datagram[head.body..]);
        let error = |code: RelayError| {
            let mut w = control_v(RelayTag::Error, version);
            w.u8(code as u8);
            vec![(w.finish(), from)]
        };
        let len = datagram.len().saturating_sub(extra);
        match tag {
            t if t == RelayTag::Allocate as u8 => self.allocate(from, &mut r, head, datagram.len(), now_ms),
            t if t == RelayTag::ProbeRequest as u8 => {
                self.probe(from, &mut r, datagram.len(), version, now_ms)
            }
            t if t == RelayTag::Register as u8 => {
                if len < RELAY_REGISTER_REQUEST_BYTES {
                    return Vec::new();
                }
                let Ok(lifetime_s) = r.u16() else {
                    return Vec::new();
                };
                let Ok(token) = r.bytes_u16(RELAY_TOKEN_MAX_BYTES) else {
                    return error(RelayError::BadSize);
                };
                let cookie = match r.u8() {
                    Ok(1) => r.array::<RELAY_COOKIE_BYTES>().ok(),
                    _ => None,
                };
                // Extensions after the cookie. `0xA1` hides the address, `0xCA` is the candidate
                // list. Anything else (padding 0, or a byte this build does not know) ends the
                // list, which is what a relay that only knew padding did. A hidden lobby keeps
                // no candidates, even if a list followed the hide byte.
                let mut hide_addr = false;
                let mut candidates = None;
                loop {
                    match r.u8() {
                        Ok(RELAY_HIDE_ADDR_EXT) => hide_addr = true,
                        Ok(RELAY_CANDIDATES_EXT) => {
                            candidates = decode_candidates(&mut r).ok();
                            break;
                        }
                        _ => break,
                    }
                }
                if hide_addr {
                    candidates = None;
                }
                if cookie.is_none_or(|c| self.cookies.verify_addr(from, 0, &c, now_ms).is_none()) {
                    // Nothing is stored for an address that has not proved it can read this. The
                    // cookie binds the port too: lobbies are keyed by the exact address, so a cookie
                    // read on one port of a shared IP must not register or renew another port's.
                    let mut w = control_v(RelayTag::RegisterChallenge, version);
                    w.raw(&self.cookies.cookie_for_addr(from, 0, now_ms));
                    return vec![(w.finish(), from)];
                }
                // A lifetime of 0 withdraws this address's lobby and its join code at once (N-19,
                // ADR-10069): a later `Lookup` of the code answers `UnknownCode`. Flows already bound
                // stay up. The answer is `NotRegistered` either way. (Registers are sent in every
                // framing; a relay before this change clamped a 0 to one second.)
                if lifetime_s == 0 {
                    if let Some(code) = self.hosts.get(&from).cloned() {
                        self.remove_lobby(&code);
                    }
                    return error(RelayError::NotRegistered);
                }
                let expires_ms =
                    now_ms.saturating_add(u64::from(lifetime_s.clamp(1, RELAY_MAX_LIFETIME_S)) * 1000);
                // One lobby per host address: renewing keeps its code, so a code (and the bind key
                // derived from it) already handed out stays good.
                if let Some(code) = self.hosts.get(&from).cloned()
                    && let Some(lobby) = self.lobbies.get_mut(&code)
                {
                    lobby.token = token.to_vec();
                    lobby.expires_ms = expires_ms;
                    // A renewal that omits the hide byte publishes again. One that sets it
                    // drops any candidates a previous register stored.
                    lobby.hide_addr = hide_addr;
                    if hide_addr {
                        lobby.candidates.clear();
                    } else if let Some(c) = candidates {
                        lobby.candidates = c;
                    }
                    let mut w = control_v(RelayTag::JoinCode, version);
                    w.raw(code.as_bytes());
                    return vec![(w.finish(), from)];
                }
                let held = self.per_ip.get(&AddrBlock::of(from.ip())).copied().unwrap_or(0);
                if self.lobbies.len() >= RELAY_MAX_LOBBIES || held >= RELAY_MAX_LOBBIES_PER_IP {
                    return error(RelayError::Full);
                }
                let code = loop {
                    let code = self.join_code(self.next_seed);
                    self.next_seed = self.next_seed.wrapping_add(1);
                    if !self.lobbies.contains_key(&code) {
                        break code;
                    }
                };
                let bind_key = self.bind_key(&code, from);
                self.lobbies.insert(
                    code.clone(),
                    Lobby {
                        host: from,
                        token: token.to_vec(),
                        bind_key,
                        expires_ms,
                        candidates: candidates.unwrap_or_default(),
                        hide_addr,
                    },
                );
                self.hosts.insert(from, code.clone());
                *self.per_ip.entry(AddrBlock::of(from.ip())).or_insert(0) += 1;
                let mut w = control_v(RelayTag::JoinCode, version);
                w.raw(code.as_bytes());
                vec![(w.finish(), from)]
            }
            t if t == RelayTag::Lookup as u8 => {
                if len < RELAY_LOOKUP_REQUEST_BYTES {
                    return Vec::new();
                }
                let code = r
                    .take(6)
                    .ok()
                    .and_then(|b| std::str::from_utf8(b).ok())
                    .and_then(normalize_join_code);
                match code
                    .and_then(|c| self.lobbies.get(&c))
                    .filter(|l| l.expires_ms > now_ms)
                {
                    Some(lobby) => {
                        let host = lobby.host;
                        let published = if lobby.hide_addr { unpublished_addr() } else { host };
                        let mut w = control_v(RelayTag::HostInfo, version);
                        encode_addr(&mut w, published);
                        // The token was size-checked on registration.
                        let _ = w.bytes_u16(&lobby.token, RELAY_TOKEN_MAX_BYTES);
                        w.raw(&lobby.bind_key);
                        let reply = vec![(w.finish(), from)];
                        if self.lookups.len() < RELAY_MAX_RECENT_LOOKUPS || self.lookups.contains_key(&from) {
                            self.lookups
                                .insert(from, (host, now_ms.saturating_add(RELAY_INTRO_WINDOW_MS)));
                        }
                        reply
                    }
                    None => error(RelayError::UnknownCode),
                }
            }
            t if t == RelayTag::PunchIntro as u8 && version >= RELAY_INTRO_MIN_VERSION => {
                self.introduce(from, &mut r, datagram.len(), version, now_ms)
            }
            t if t == RelayTag::PunchIntro as u8 => {
                if len < RELAY_PUNCH_REQUEST_BYTES {
                    return Vec::new();
                }
                let Ok(peer) = decode_addr(&mut r) else {
                    return Vec::new();
                };
                // A client may ask for its host. A host may only ask for a client that looked it
                // up: the relay never writes to an address that had no part in this.
                let client_side = self.lobby_of(peer, now_ms).is_some();
                let host_side = self.lobby_of(from, now_ms).is_some()
                    && self
                        .lookups
                        .get(&peer)
                        .is_some_and(|(host, until)| *host == from && *until > now_ms);
                if !client_side && !host_side {
                    return error(RelayError::NotAHost);
                }
                // A hidden lobby's introduction would name one side to the other. Say nothing.
                let hidden = self.lobby_of(peer, now_ms).is_some_and(|l| l.hide_addr)
                    || self.lobby_of(from, now_ms).is_some_and(|l| l.hide_addr);
                if hidden {
                    return Vec::new();
                }
                let notice = |addr: SocketAddr| {
                    let mut w = control_v(RelayTag::PunchPeer, version);
                    encode_addr(&mut w, addr);
                    w.finish()
                };
                vec![(notice(peer), from), (notice(from), peer)]
            }
            t if t == RelayTag::Bind as u8 => {
                if len < RELAY_BIND_REQUEST_BYTES {
                    return Vec::new();
                }
                let Ok(host) = decode_addr(&mut r) else {
                    return Vec::new();
                };
                let Ok(key) = r.array::<RELAY_BIND_KEY_BYTES>() else {
                    return Vec::new();
                };
                let Some(lobby) = self.lobby_of(host, now_ms).filter(|_| host != from) else {
                    return error(RelayError::NotAHost);
                };
                if !constant_time_eq(&lobby.bind_key, &key) {
                    return error(RelayError::BadKey);
                }
                // A host has one relayed client at a time on this simple relay. A later bind
                // takes over only from a flow that went quiet, so a client that lost its port can
                // come back and nobody can push a playing client off.
                let old = self.flows.get(&host).copied();
                // What counts is the client's own traffic: a host keeps sending to a client that
                // is gone.
                let client_sent = old.and_then(|f| self.flows.get(&f.dst)).map(|f| f.last_sent_ms);
                if client_sent.is_some_and(|t| now_ms.saturating_sub(t) < RELAY_BIND_REPLACE_IDLE_MS) {
                    return error(RelayError::Busy);
                }
                if old.is_none() && self.flows.len() + 2 > RELAY_MAX_FLOWS {
                    return error(RelayError::Full);
                }
                if let Some(old) = old {
                    self.flows.remove(&host);
                    self.flows.remove(&old.dst);
                }
                self.flows.insert(from, Flow::new(host, now_ms, false));
                self.flows.insert(host, Flow::new(from, now_ms, true));
                Vec::new()
            }
            t if t == RelayTag::Publish as u8 => {
                if len < RELAY_PUBLISH_REQUEST_BYTES {
                    return Vec::new();
                }
                let Ok(listing) = r.bytes_u16(RELAY_LISTING_MAX_BYTES) else {
                    return error(RelayError::BadSize);
                };
                if listing.is_empty() {
                    return error(RelayError::BadSize);
                }
                if self.lobby_of(from, now_ms).is_none() {
                    return error(RelayError::NotRegistered);
                }
                if let Some(row) = self.directory.iter_mut().find(|(h, _)| *h == from) {
                    row.1 = listing.to_vec();
                } else if self.directory.len() >= RELAY_DIRECTORY_CAP {
                    return error(RelayError::Full);
                } else {
                    self.directory.push((from, listing.to_vec()));
                }
                Vec::new()
            }
            t if t == RelayTag::ListGames as u8 => {
                if len < RELAY_LIST_REQUEST_BYTES {
                    return Vec::new();
                }
                let Ok(page) = r.u8() else {
                    return Vec::new();
                };
                vec![(self.directory_page_v(page, version), from)]
            }
            _ => Vec::new(),
        }
    }

    /// A protocol 3 `PunchIntro`: resolves the join code and introduces the client at `from` and
    /// the code's host to each other (spec 120 § 10.7). Nothing is sent when both answers together
    /// would be larger than the request (`request_len`).
    fn introduce(
        &mut self,
        from: SocketAddr,
        r: &mut WireReader<'_>,
        request_len: usize,
        version: u8,
        now_ms: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        if request_len < RELAY_INTRO_REQUEST_BYTES {
            return Vec::new();
        }
        let code = r
            .take(6)
            .ok()
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(normalize_join_code);
        let Ok(mut client_candidates) = decode_candidates(r) else {
            return Vec::new();
        };
        client_candidates.truncate(NAT_MAX_CANDIDATES);
        let Some(lobby) = code
            .and_then(|c| self.lobbies.get(&c))
            .filter(|l| l.expires_ms > now_ms)
            .cloned()
        else {
            let mut w = control_v(RelayTag::Error, version);
            w.u8(RelayError::UnknownCode as u8);
            return vec![(w.finish(), from)];
        };
        if lobby.host == from || lobby.hide_addr {
            // A hidden lobby's introduction names both sides. Send nothing.
            return Vec::new();
        }
        let key = blake3::derive_key("regia_net relay intro session v1", &self.secret);
        let hash = blake3::keyed_hash(&key, &self.intro_seq.to_le_bytes());
        self.intro_seq = self.intro_seq.wrapping_add(1);
        let mut session = [0u8; NAT_SESSION_KEY_BYTES];
        session.copy_from_slice(&hash.as_bytes()[..NAT_SESSION_KEY_BYTES]);
        let to_client = Introduction {
            peer: lobby.host,
            token: lobby.token.clone(),
            bind_key: lobby.bind_key,
            session,
            observed: from,
            candidates: lobby.candidates.clone(),
        }
        .encode(version);
        let to_host = Introduction {
            peer: from,
            token: Vec::new(),
            bind_key: [0; RELAY_BIND_KEY_BYTES],
            session,
            observed: lobby.host,
            candidates: client_candidates,
        }
        .encode(version);
        if to_client.len() + to_host.len() > request_len {
            // Never a reflector: both answers must fit in what was sent.
            return Vec::new();
        }
        if self.lookups.len() < RELAY_MAX_RECENT_LOOKUPS || self.lookups.contains_key(&from) {
            self.lookups
                .insert(from, (lobby.host, now_ms.saturating_add(RELAY_INTRO_WINDOW_MS)));
        }
        self.introductions += 1;
        vec![(to_client, from), (to_host, lobby.host)]
    }

    /// Cuts the directory into groups that fit one datagram each.
    fn directory_groups(&self) -> Vec<Vec<&[u8]>> {
        // The page head (magic, the versioned framing's two bytes, tag, page, pages, count).
        const HEAD: usize = 10;
        let mut groups: Vec<Vec<&[u8]>> = vec![Vec::new()];
        let mut size = HEAD;
        for (_, listing) in &self.directory {
            let add = 2 + listing.len();
            if size + add > RELAY_DIRECTORY_PAGE_BYTES && groups.last().is_some_and(|g| !g.is_empty()) {
                groups.push(Vec::new());
                size = HEAD;
            }
            if let Some(g) = groups.last_mut() {
                g.push(listing);
            }
            size += add;
        }
        groups
    }

    /// One directory page. Past the last page (the directory shrank) it is an empty page that
    /// still names the page count.
    pub fn directory_page(&self, page: u8) -> Vec<u8> {
        self.directory_page_v(page, 1)
    }

    fn directory_page_v(&self, page: u8, version: u8) -> Vec<u8> {
        let groups = self.directory_groups();
        let pages = groups.len().min(usize::from(u8::MAX)) as u8;
        let empty = Vec::new();
        encode_page(
            page,
            pages,
            groups.get(usize::from(page)).unwrap_or(&empty),
            version,
        )
    }

    /// The directory as datagrams of at most [`RELAY_DIRECTORY_PAGE_BYTES`].
    pub fn directory_pages(&self) -> Vec<Vec<u8>> {
        let groups = self.directory_groups();
        let pages = groups.len().min(usize::from(u8::MAX)) as u8;
        groups
            .iter()
            .take(usize::from(u8::MAX))
            .enumerate()
            .map(|(i, g)| encode_page(i as u8, pages, g, 1))
            .collect()
    }
}

fn encode_page(page: u8, pages: u8, listings: &[&[u8]], version: u8) -> Vec<u8> {
    let mut w = control_v(RelayTag::Directory, version);
    w.u8(page);
    w.u8(pages);
    w.u8(listings.len() as u8);
    for listing in listings {
        // Sizes were checked on publish.
        let _ = w.bytes_u16(listing, RELAY_LISTING_MAX_BYTES);
    }
    w.finish()
}

/// A relay process: a [`RelayCore`] on a control socket, plus one socket per allocation port
/// (protocol 2).
#[derive(Debug)]
pub struct RelayServer<S: DatagramSocket> {
    socket: S,
    ports: HashMap<u16, S>,
    core: RelayCore,
    /// Where port-forward probes leave from (protocol 3): a port no host sends to.
    probe: Option<S>,
}

/// Datagrams one socket is read for in one [`RelayServer::update`].
const RELAY_READS_PER_UPDATE: usize = 4096;
/// Datagrams one allocation socket is read for in one [`RelayServer::update`].
const RELAY_ALLOCATION_READS_PER_UPDATE: usize = 256;

impl<S: DatagramSocket> RelayServer<S> {
    /// A relay on `socket` with no allocation ports (protocol 1 flows only; an `Allocate` is
    /// answered `Full { cap: 0 }`).
    pub fn new(socket: S, seed: u64) -> Self {
        Self {
            socket,
            ports: HashMap::new(),
            core: RelayCore::new(seed),
            probe: None,
        }
    }

    /// A relay on the control socket `socket` with `ports` as its allocation pool (each socket's
    /// local port is one allocation port) and the operator settings `config`. Sockets whose
    /// address cannot be read are left out.
    pub fn with_allocation_sockets(socket: S, ports: Vec<S>, seed: u64, config: RelayConfig) -> Self {
        Self::from_core(
            socket,
            ports,
            RelayCore::with_config(seed, fresh_secret(), config),
        )
    }

    /// A relay over a prepared core (a fixed secret for tests); the sockets as
    /// [`RelayServer::with_allocation_sockets`].
    pub fn from_core(socket: S, ports: Vec<S>, mut core: RelayCore) -> Self {
        let ports: HashMap<u16, S> = ports
            .into_iter()
            .filter_map(|s| s.local_addr().ok().map(|a| (a.port(), s)))
            .collect();
        core.add_allocation_ports(ports.keys().copied());
        Self {
            socket,
            ports,
            core,
            probe: None,
        }
    }

    /// Gives the relay a probe port (relay protocol 3, spec 120 § 10.7): port-forward probes leave
    /// from `socket`, which must be a port no host sends to (not the control port, not an
    /// allocation port). Anything that arrives on it is read and dropped.
    pub fn with_probe_socket(mut self, socket: S) -> Self {
        self.core.set_probe_capable(true);
        self.probe = Some(socket);
        self
    }

    /// The relay state.
    pub fn core(&self) -> &RelayCore {
        &self.core
    }

    /// The relay state, mutable (a drain, or `relay.per_address_per_sec`).
    pub fn core_mut(&mut self) -> &mut RelayCore {
        &mut self.core
    }

    /// Stops new allocations. See [`RelayCore::stop_new_allocations`].
    pub fn stop_new_allocations(&mut self) {
        self.core.stop_new_allocations();
    }

    /// Sets `relay.per_address_per_sec`. See [`RelayCore::set_source_rate`].
    pub fn set_source_rate(&mut self, per_sec: u32) {
        self.core.set_source_rate(per_sec);
    }

    /// What the relay holds and what it refused.
    pub fn status(&self) -> RelayStatus {
        self.core.status()
    }

    /// The relay as `net.relay_status` reports it: counters, caps and every allocation.
    pub fn report(&self) -> RelayReport {
        RelayReport {
            role: "relay".to_owned(),
            relay: self.local_addr().map(|a| a.to_string()).unwrap_or_default(),
            status: Some(self.core.status()),
            config: Some(self.core.relay_config().clone()),
            allocations: self.core.allocations(),
            ..RelayReport::default()
        }
    }

    /// The control socket's address.
    ///
    /// # Errors
    ///
    /// The socket cannot report it.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Handles every waiting datagram: at most 4096 on the control socket and 256 on each
    /// allocation port in use per call. Ports with no allocation are not read (nothing sent to
    /// them is forwarded anyway).
    pub fn update(&mut self, now_ms: u64) {
        let _span = tracing::trace_span!("relay_update").entered();
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        for _ in 0..RELAY_READS_PER_UPDATE {
            match self.socket.recv_from(&mut buf) {
                Ok(Some((n, from))) => {
                    for (bytes, dest) in self.core.handle(from, &buf[..n], now_ms) {
                        if let Err(e) = self.socket.send_to(&bytes, dest) {
                            tracing::debug!(error = %e, %dest, "relay send error");
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "relay receive error");
                    break;
                }
            }
        }
        if let Some(probe) = &mut self.probe {
            for (bytes, dest) in self.core.take_probes() {
                if let Err(e) = probe.send_to(&bytes, dest) {
                    tracing::debug!(error = %e, %dest, "relay probe send error");
                }
            }
            // Nothing is expected here; read it so it does not queue.
            for _ in 0..RELAY_ALLOCATION_READS_PER_UPDATE {
                if !matches!(probe.recv_from(&mut buf), Ok(Some(_))) {
                    break;
                }
            }
        }
        if self.ports.is_empty() {
            return;
        }
        let live = self.core.live_allocation_ports();
        for port in live {
            let Some(socket) = self.ports.get_mut(&port) else {
                continue;
            };
            for _ in 0..RELAY_ALLOCATION_READS_PER_UPDATE {
                match socket.recv_from(&mut buf) {
                    Ok(Some((n, from))) => {
                        for (bytes, dest) in self.core.handle_allocation(port, from, &buf[..n], now_ms) {
                            if let Err(e) = socket.send_to(&bytes, dest) {
                                tracing::debug!(error = %e, %dest, port, "relay allocation send error");
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::debug!(error = %e, port, "relay allocation receive error");
                        break;
                    }
                }
            }
        }
    }
}

impl RelayServer<crate::socket::UdpTransport> {
    /// Binds a relay's control socket at `addr` and `config.allocation_ports` allocation sockets
    /// on the same IP (from `config.allocation_port_base`, or system-chosen ports when it is 0).
    /// A pool port that cannot be bound (taken, or the file-descriptor limit reached) is left
    /// out with a warning; [`RelayStatus::ports`] says how many the relay got.
    ///
    /// # Errors
    ///
    /// The control socket cannot be bound.
    pub fn bind(addr: SocketAddr, config: RelayConfig, seed: u64) -> std::io::Result<Self> {
        use crate::socket::UdpTransport;
        let socket = UdpTransport::bind(addr)?;
        let mut ports = Vec::with_capacity(usize::from(config.allocation_ports));
        let mut failed = 0usize;
        for i in 0..config.allocation_ports {
            let port = match config.allocation_port_base {
                0 => 0,
                base => match base.checked_add(i) {
                    Some(p) => p,
                    None => break,
                },
            };
            match UdpTransport::bind(SocketAddr::new(addr.ip(), port)) {
                Ok(s) => ports.push(s),
                Err(e) => {
                    failed += 1;
                    if failed == 1 {
                        tracing::warn!(error = %e, port, "relay allocation port not bound");
                    }
                    if config.allocation_port_base == 0 {
                        // System-chosen ports fail only on a resource limit: stop asking.
                        break;
                    }
                }
            }
        }
        if ports.len() < usize::from(config.allocation_ports) {
            tracing::warn!(
                bound = ports.len(),
                asked = config.allocation_ports,
                "relay allocation pool is smaller than configured"
            );
        }
        let relay = Self::with_allocation_sockets(socket, ports, seed, config);
        // The probe port (protocol 3): a system-chosen port on the same IP.
        Ok(match UdpTransport::bind(SocketAddr::new(addr.ip(), 0)) {
            Ok(probe) => relay.with_probe_socket(probe),
            Err(e) => {
                tracing::warn!(error = %e, "relay probe port not bound: hosts' port-forward checks report relay_cannot_probe");
                relay
            }
        })
    }
}

/// NAT mapping behavior (RFC 4787 § 4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatMapping {
    /// One public port per private socket, whatever the destination.
    EndpointIndependent,
    /// A new public port per destination address (any port of one address shares it).
    AddressDependent,
    /// A new public port per destination address and port ("symmetric").
    AddressAndPortDependent,
}

impl NatMapping {
    /// Every mapping behavior of RFC 4787 § 4.1.
    pub const ALL: [Self; 3] = [
        Self::EndpointIndependent,
        Self::AddressDependent,
        Self::AddressAndPortDependent,
    ];
}

/// NAT filtering behavior (RFC 4787 § 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatFiltering {
    /// Anyone may send to a mapped port ("full cone").
    EndpointIndependent,
    /// Only addresses the inside host has sent to, from any of their ports ("restricted").
    AddressDependent,
    /// Only addresses and ports the inside host has sent to ("port restricted").
    AddressAndPortDependent,
}

impl NatFiltering {
    /// Every filtering behavior of RFC 4787 § 5.
    pub const ALL: [Self; 3] = [
        Self::EndpointIndependent,
        Self::AddressDependent,
        Self::AddressAndPortDependent,
    ];
}

/// A simulated NAT in front of one private socket.
#[derive(Debug, Clone)]
pub struct SimNat {
    mapping: NatMapping,
    filtering: NatFiltering,
    public_ip: Ipv4Addr,
    next_port: u16,
    ports: HashMap<Option<SocketAddr>, u16>,
    permits: HashSet<(u16, SocketAddr)>,
}

impl SimNat {
    /// A NAT with the given behavior and public IP.
    pub fn new(mapping: NatMapping, filtering: NatFiltering, public_ip: Ipv4Addr) -> Self {
        Self {
            mapping,
            filtering,
            public_ip,
            next_port: 40_000,
            ports: HashMap::new(),
            permits: HashSet::new(),
        }
    }

    /// Its mapping behavior.
    pub fn mapping(&self) -> NatMapping {
        self.mapping
    }

    /// Its filtering behavior.
    pub fn filtering(&self) -> NatFiltering {
        self.filtering
    }

    /// Sends from the private socket to `dest`; returns the public source address used.
    pub fn outbound(&mut self, dest: SocketAddr) -> SocketAddr {
        let key = match self.mapping {
            NatMapping::EndpointIndependent => None,
            NatMapping::AddressDependent => Some(SocketAddr::new(dest.ip(), 0)),
            NatMapping::AddressAndPortDependent => Some(dest),
        };
        let port = match self.ports.get(&key) {
            Some(port) => *port,
            None => {
                let port = self.next_port;
                self.next_port += 1;
                self.ports.insert(key, port);
                port
            }
        };
        self.permits.insert((port, dest));
        SocketAddr::V4(SocketAddrV4::new(self.public_ip, port))
    }

    /// Whether a datagram from `from` to public port `port` reaches the private socket.
    pub fn inbound(&self, from: SocketAddr, port: u16) -> bool {
        if !self.ports.values().any(|p| *p == port) {
            return false;
        }
        match self.filtering {
            NatFiltering::EndpointIndependent => true,
            NatFiltering::AddressDependent => self
                .permits
                .iter()
                .any(|(p, to)| *p == port && to.ip() == from.ip()),
            NatFiltering::AddressAndPortDependent => self.permits.contains(&(port, from)),
        }
    }
}

/// Outcome of a punch attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PunchResult {
    /// A direct path works both ways.
    Direct,
    /// Use the relay's forwarding.
    NeedsRelay,
}

/// Simulates a relay-introduced punch between the sockets behind `a` and `b`: both register with
/// `relay` (learning their reflexive addresses), both send to the other's reflexive address, and
/// each answers the source it actually saw.
pub fn simulate_hole_punch(a: &mut SimNat, b: &mut SimNat, relay: SocketAddr) -> PunchResult {
    let a_reflexive = a.outbound(relay);
    let b_reflexive = b.outbound(relay);
    // Round 1: simultaneous sends toward the advertised reflexive addresses.
    let a_src = a.outbound(b_reflexive);
    let b_src = b.outbound(a_reflexive);
    let a_heard_b = a.inbound(b_src, a_reflexive.port());
    let b_heard_a = b.inbound(a_src, b_reflexive.port());
    // Round 2: whoever heard the other answers the observed source.
    let mut a_to_b = b_heard_a;
    let mut b_to_a = a_heard_b;
    if b_heard_a {
        let reply_src = b.outbound(a_src);
        b_to_a |= a.inbound(reply_src, a_src.port());
    }
    if a_heard_b {
        let reply_src = a.outbound(b_src);
        a_to_b |= b.inbound(reply_src, b_src.port());
    }
    if a_to_b && b_to_a {
        PunchResult::Direct
    } else {
        PunchResult::NeedsRelay
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([203, 0, 113, last], port))
    }

    /// The key of `host`'s lobby, as `Lookup` hands it out.
    fn key_of(relay: &mut RelayCore, host: SocketAddr) -> [u8; RELAY_BIND_KEY_BYTES] {
        let code = relay.hosts.get(&host).expect("registered").clone();
        relay.bind_key(&code, host)
    }

    /// The two steps of a `Register`: the challenge, then the request with its cookie.
    fn registered(
        relay: &mut RelayCore,
        host: SocketAddr,
        lifetime_s: u16,
        token: &[u8],
        now: u64,
    ) -> Vec<(Vec<u8>, SocketAddr)> {
        let first = relay.handle(host, &encode_register(lifetime_s, token).expect("fits"), now);
        let RelayReply::RegisterChallenge(cookie) = reply(&first) else {
            panic!("expected a challenge")
        };
        let again = encode_register_with_cookie(lifetime_s, token, &cookie).expect("fits");
        relay.handle(host, &again, now)
    }

    fn register(relay: &mut RelayCore, host: SocketAddr, now: u64) -> String {
        match reply(&registered(relay, host, 60, b"", now)) {
            RelayReply::JoinCode(code) => code,
            other => panic!("expected a code, got {other:?}"),
        }
    }

    fn reply(out: &[(Vec<u8>, SocketAddr)]) -> RelayReply {
        assert_eq!(out.len(), 1, "one reply");
        RelayReply::decode(&out[0].0)
            .expect("valid")
            .expect("relay traffic")
    }

    /// Audit v034 net H1: a join code hands out the host's address and bind key, so nobody who
    /// lacks the relay's secret may predict one. Same seed, different secrets: different codes,
    /// and none of them is the public `mint_join_code` of a nearby counter.
    #[test]
    fn join_codes_are_not_predictable_from_the_seed() {
        let host = addr(1, 7777);
        let public: Vec<String> = (0..64u64).map(mint_join_code).collect();
        let mut seen = Vec::new();
        for secret in [[7u8; 32], [8u8; 32], [9u8; 32]] {
            let mut relay = RelayCore::with_secret(0, secret);
            let code = register(&mut relay, host, 0);
            assert!(
                !public.contains(&code),
                "code {code} is mint_join_code of a seed near the relay's start"
            );
            seen.push(code);
        }
        seen.sort();
        seen.dedup();
        assert_eq!(seen.len(), 3, "the secret decides the code: {seen:?}");
        // The defaults too: two relays started the same way do not hand out the same code.
        let a = register(&mut RelayCore::default(), host, 0);
        let b = register(&mut RelayCore::default(), host, 0);
        assert_ne!(a, b, "two default relays minted the same first code");
    }

    /// N-19 (ADR-10069): a `Register` with a lifetime of 0 (cookie-checked like any register)
    /// withdraws the sender's lobby and its code at once; only the holder can.
    #[test]
    fn a_zero_lifetime_register_withdraws_the_code() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        let RelayReply::JoinCode(code) = reply(&registered(&mut relay, host, 30, b"tok", 0)) else {
            panic!("expected a code")
        };
        let lookup = encode_lookup(&code).expect("code");
        let client = addr(2, 5000);
        assert!(matches!(
            reply(&relay.handle(client, &lookup, 100)),
            RelayReply::HostInfo { .. }
        ));
        // Another address cannot withdraw it.
        let other = addr(3, 7777);
        assert_eq!(
            reply(&registered(&mut relay, other, 0, b"tok", 200)),
            RelayReply::Error(RelayError::NotRegistered)
        );
        assert!(matches!(
            reply(&relay.handle(client, &lookup, 300)),
            RelayReply::HostInfo { .. }
        ));
        assert_eq!(
            reply(&registered(&mut relay, host, 0, b"tok", 400)),
            RelayReply::Error(RelayError::NotRegistered),
            "the withdrawal is answered"
        );
        assert_eq!(
            reply(&relay.handle(client, &lookup, 500)),
            RelayReply::Error(RelayError::UnknownCode),
            "the code still resolves after the host withdrew it"
        );
        assert_eq!(relay.lobby_count(), 0);
        assert!(relay.hosts.is_empty() && relay.per_ip.is_empty());
    }

    #[test]
    fn register_lookup_and_expiry() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        let out = registered(&mut relay, host, 30, b"tok", 0);
        let RelayReply::JoinCode(code) = reply(&out) else {
            panic!("expected a code")
        };
        let lookup = encode_lookup(&code.to_lowercase()).expect("code");
        let client = addr(2, 5000);
        assert_eq!(
            reply(&relay.handle(client, &lookup, 1_000)),
            RelayReply::HostInfo {
                addr: host,
                token: b"tok".to_vec(),
                bind_key: key_of(&mut relay, host),
            }
        );
        assert_eq!(
            reply(&relay.handle(client, &lookup, 30_000)),
            RelayReply::Error(RelayError::UnknownCode)
        );
        assert_eq!(relay.lobby_count(), 0);
    }

    #[test]
    fn bound_flows_forward_opaque_bytes_both_ways() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        let client = addr(2, 5000);
        registered(&mut relay, host, 60, b"", 0);
        let key = key_of(&mut relay, host);
        assert!(relay.handle(client, &encode_bind(host, &key), 0).is_empty());
        assert_eq!(relay.flow_count(), 1);
        // A game datagram that starts with the relay magic is still forwarded, not parsed.
        let tricky = encode_register(60, b"x").expect("fits");
        assert_eq!(relay.handle(client, &tricky, 1), vec![(tricky.clone(), host)]);
        assert_eq!(relay.handle(host, b"state", 2), vec![(b"state".to_vec(), client)]);
        assert_eq!(relay.lobby_count(), 1, "the forwarded bytes registered nothing");
        relay.expire(2 + RELAY_FLOW_IDLE_MS);
        assert_eq!(relay.flow_count(), 0, "idle flows expire");
    }

    /// The version probe (spec 120 § 12.2, ADR-7985): a stranger gets the relay's own answer, no
    /// larger than the probe; a probe inside a flow goes to the host, which answers for the game.
    #[test]
    fn the_relay_answers_probes_outside_a_flow_and_forwards_them_inside_one() {
        use crate::version_probe::{ProbeAnswer, ProbeRequest};
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let probe = ProbeRequest { nonce: 77 }.to_datagram();
        let stranger = addr(3, 6000);
        let out = relay.handle(stranger, &probe, 0);
        let [(answer, to)] = out.as_slice() else {
            panic!("one answer: {out:?}")
        };
        assert_eq!(*to, stranger);
        assert!(answer.len() <= probe.len());
        let answer = ProbeAnswer::decode(answer).expect("answer");
        assert_eq!(answer.nonce, 77);
        assert_eq!(answer.stamp.wire, crate::payload::REGIA_NET_WIRE_VERSION);
        assert!(relay.handle(stranger, &probe[..40], 0).is_empty(), "unpadded");

        let host = addr(1, 7777);
        let client = addr(2, 5000);
        registered(&mut relay, host, 60, b"", 0);
        let key = key_of(&mut relay, host);
        assert!(relay.handle(client, &encode_bind(host, &key), 0).is_empty());
        assert_eq!(relay.handle(client, &probe, 1), vec![(probe.clone(), host)]);
    }

    #[test]
    fn bind_requires_a_registered_host_and_publish_requires_registration() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let stranger = addr(9, 1);
        assert_eq!(
            reply(&relay.handle(addr(2, 5000), &encode_bind(stranger, &[0; 16]), 0)),
            RelayReply::Error(RelayError::NotAHost)
        );
        assert_eq!(
            reply(&relay.handle(stranger, &encode_publish(b"card").expect("fits"), 0)),
            RelayReply::Error(RelayError::NotRegistered)
        );
    }

    #[test]
    fn directory_pages_hold_every_listing() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        for i in 0..20u8 {
            let host = addr(i, 7777);
            registered(&mut relay, host, 60, b"", 0);
            relay.handle(host, &encode_publish(&[i; 300]).expect("fits"), 0);
        }
        assert_eq!(relay.listing_count(), 20);
        // One request, one page: the client walks the pages the first reply names.
        let mut total = 0;
        let mut page = 0u8;
        let mut pages = 1u8;
        while page < pages {
            let out = relay.handle(addr(200, 1), &encode_list_games_page(page), 0);
            assert_eq!(out.len(), 1, "one page per request");
            assert!(out[0].0.len() <= RELAY_DIRECTORY_PAGE_BYTES);
            match RelayReply::decode(&out[0].0).expect("valid") {
                Some(RelayReply::Directory {
                    page: got,
                    pages: n,
                    listings,
                }) => {
                    assert_eq!(got, page);
                    pages = n;
                    total += listings.len();
                }
                other => panic!("unexpected {other:?}"),
            }
            page += 1;
        }
        assert!(pages > 1, "300-byte cards need several pages");
        assert_eq!(usize::from(pages), relay.directory_pages().len());
        assert_eq!(total, 20);
    }

    /// Audit H1, M2: a forged source must get no more bytes than the request carried, for every
    /// control message at its largest.
    #[test]
    fn no_request_is_answered_with_more_bytes_than_it_carried() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        // Largest token, IPv6 host: the largest `HostInfo`.
        let host = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 7777));
        let RelayReply::JoinCode(code) =
            reply(&registered(&mut relay, host, 60, &[7; RELAY_TOKEN_MAX_BYTES], 0))
        else {
            panic!("expected a code")
        };
        for i in 0..RELAY_DIRECTORY_CAP as u8 {
            let lister = addr(i, 7000);
            registered(&mut relay, lister, 60, b"", 0);
            let card = vec![i; RELAY_LISTING_MAX_BYTES];
            relay.handle(lister, &encode_publish(&card).expect("fits"), 0);
        }
        let victim = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 9], 40000));
        let requests = [
            encode_register(60, b"").expect("fits"),
            encode_lookup(&code).expect("code"),
            encode_lookup("AAAAAA").expect("code"),
            encode_punch_intro(host),
            encode_bind(host, &[0; RELAY_BIND_KEY_BYTES]),
            encode_list_games(),
        ];
        for request in &requests {
            let out = relay.handle(victim, request, 1);
            let sent: usize = out.iter().map(|(b, _)| b.len()).sum();
            assert!(!out.is_empty(), "every request here is answered");
            assert!(
                sent <= request.len(),
                "{sent} bytes for a {}-byte request",
                request.len()
            );
        }
    }

    /// Audit H1: the short requests of the old protocol get nothing back.
    #[test]
    fn requests_shorter_than_their_reply_are_dropped() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let code = register(&mut relay, addr(1, 7777), 0);
        let client = addr(2, 5000);
        for (tag, min) in [
            (RelayTag::Register, RELAY_REGISTER_REQUEST_BYTES),
            (RelayTag::Lookup, RELAY_LOOKUP_REQUEST_BYTES),
            (RelayTag::PunchIntro, RELAY_PUNCH_REQUEST_BYTES),
            (RelayTag::Bind, RELAY_BIND_REQUEST_BYTES),
            (RelayTag::ListGames, RELAY_LIST_REQUEST_BYTES),
            (RelayTag::Publish, RELAY_PUBLISH_REQUEST_BYTES),
        ] {
            let mut short = control(tag).finish();
            short.extend_from_slice(code.as_bytes());
            short.resize(min - 1, 0);
            assert!(relay.handle(client, &short, 1).is_empty(), "{tag:?}");
        }
        assert_eq!(relay.lobby_count(), 1, "the short Register made no lobby");
    }

    /// Audit H1, M4: a source over its rate gets silence; another source is unaffected and the
    /// budget comes back with time.
    #[test]
    fn control_requests_are_limited_per_source_ip() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let flooder = addr(1, 1);
        let list = encode_list_games();
        let burst = RELAY_SOURCE_REQUESTS_PER_SECOND as usize * 2;
        let answered = (0..burst * 3)
            .filter(|i| !relay.handle(addr(1, *i as u16 + 1), &list, 0).is_empty())
            .count();
        assert_eq!(answered, burst, "other ports of one IP share the bucket");
        assert_eq!(relay.rate_limited as usize, burst * 2);
        assert!(relay.handle(flooder, &list, 0).is_empty());
        assert_eq!(relay.handle(addr(2, 1), &list, 0).len(), 1, "another IP");
        assert_eq!(relay.handle(flooder, &list, 1_000).len(), 1, "refilled");
    }

    /// Review: spoofed sources cannot lock a new source out of the relay.
    #[test]
    fn forged_sources_do_not_lock_out_a_new_one() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let list = encode_list_games();
        for i in 0..RELAY_MAX_TRACKED_SOURCES as u32 * 2 {
            let ip = Ipv4Addr::from(0x0A00_0000 + i);
            relay.handle(SocketAddr::from((ip, 1)), &list, 0);
        }
        assert_eq!(relay.sources.slots(), RELAY_MAX_TRACKED_SOURCES);
        assert_eq!(relay.handle(addr(1, 1), &list, 3_000).len(), 1);
    }

    /// Review: a forged `Register` stores nothing, so forged sources cannot fill the lobby table
    /// and turn real hosts away. Only the address that reads the cookie gets a lobby.
    #[test]
    fn a_register_must_prove_its_source_address() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let forged = addr(1, 1);
        let out = relay.handle(forged, &encode_register(u16::MAX, b"").expect("fits"), 0);
        assert!(matches!(reply(&out), RelayReply::RegisterChallenge(_)));
        assert_eq!(relay.lobby_count(), 0);
        // A made-up cookie, or another address's, is another challenge.
        let RelayReply::RegisterChallenge(theirs) = reply(&out) else {
            panic!("expected a challenge")
        };
        let bad = encode_register_with_cookie(60, b"", &[0; RELAY_COOKIE_BYTES]).expect("fits");
        assert!(matches!(
            reply(&relay.handle(forged, &bad, 1)),
            RelayReply::RegisterChallenge(_)
        ));
        let stolen = encode_register_with_cookie(60, b"", &theirs).expect("fits");
        assert!(matches!(
            reply(&relay.handle(addr(2, 2), &stolen, 1)),
            RelayReply::RegisterChallenge(_)
        ));
        assert_eq!(relay.lobby_count(), 0);
        // It expires with its bucket.
        let late = relay.handle(forged, &stolen, 30_000);
        assert!(matches!(reply(&late), RelayReply::RegisterChallenge(_)));
        assert!(matches!(
            reply(&relay.handle(forged, &stolen, 1)),
            RelayReply::JoinCode(_)
        ));
    }

    /// Review row 19: the relay cookie binds the port. A cookie read on one port of a shared IP
    /// (a CGNAT address, a VPN exit) neither renews the lobby of another port on that IP, which
    /// would overwrite its connect token behind a live join code, nor registers a new lobby that
    /// points at another port. It still works from the port that read it.
    #[test]
    fn a_register_cookie_is_bound_to_the_port_that_read_it() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let victim = addr(1, 7777);
        let RelayReply::JoinCode(code) = reply(&registered(&mut relay, victim, 60, b"victim", 0)) else {
            panic!("expected a code")
        };
        // A neighbor on the same public IP reads a cookie on its own port.
        let neighbor = addr(1, 40_000);
        let RelayReply::RegisterChallenge(cookie) =
            reply(&relay.handle(neighbor, &encode_register(60, b"").expect("fits"), 1))
        else {
            panic!("expected a challenge")
        };
        // Spoofed as the victim: the renewal path refuses it and the token stays.
        let poison = encode_register_with_cookie(60, b"poison", &cookie).expect("fits");
        assert!(matches!(
            reply(&relay.handle(victim, &poison, 2)),
            RelayReply::RegisterChallenge(_)
        ));
        let lookup = encode_lookup(&code).expect("code");
        let RelayReply::HostInfo {
            addr: host, token, ..
        } = reply(&relay.handle(addr(2, 5000), &lookup, 3))
        else {
            panic!("expected host info")
        };
        assert_eq!((host, token.as_slice()), (victim, b"victim".as_slice()));
        // Spoofed as a third port: no lobby points there.
        let third = addr(1, 9999);
        assert!(matches!(
            reply(&relay.handle(third, &poison, 4)),
            RelayReply::RegisterChallenge(_)
        ));
        assert_eq!(relay.lobby_count(), 1);
        // From the port that read it, the cookie registers.
        let own = encode_register_with_cookie(60, b"mine", &cookie).expect("fits");
        assert!(matches!(
            reply(&relay.handle(neighbor, &own, 5)),
            RelayReply::JoinCode(_)
        ));
        assert_eq!(relay.lobby_count(), 2);
    }

    /// ADR-10399: a register that asks to hide its address publishes none. `HostInfo` and the
    /// allocation replies omit the host the relay saw and the member. Forwarding still uses
    /// those endpoints.
    #[test]
    fn a_hidden_register_publishes_no_address() {
        let mut relay = RelayCore::with_secret(1, [9; 32]);
        relay.add_allocation_ports(50_000..50_004);
        let host = SocketAddr::from(([198, 51, 100, 7], 7777));
        let member = SocketAddr::from(([203, 0, 113, 20], 5000));
        let host_octets = [198, 51, 100, 7];
        let member_octets = [203, 0, 113, 20];
        let names = |bytes: &[u8]| {
            bytes.windows(4).any(|window| window == host_octets)
                || bytes.windows(4).any(|window| window == member_octets)
        };
        let first = relay.handle(
            host,
            &encode_register_hidden(60, b"ticket", None).expect("fits"),
            0,
        );
        let RelayReply::RegisterChallenge(cookie) = reply(&first) else {
            panic!("expected a challenge");
        };
        let again = encode_register_hidden(60, b"ticket", Some(&cookie)).expect("fits");
        let RelayReply::JoinCode(code) = reply(&relay.handle(host, &again, 1)) else {
            panic!("expected a code");
        };
        let info = relay.handle(member, &encode_lookup(&code).expect("code"), 2);
        let RelayReply::HostInfo { addr, bind_key, .. } = reply(&info) else {
            panic!("expected host info");
        };
        assert!(addr.ip().is_unspecified() && addr.port() == 0);
        assert!(!names(&info[0].0), "HostInfo named a side");
        let client_key = [3u8; RELAY_ALLOCATION_KEY_BYTES];
        let asked = unpublished_addr();
        let challenge = relay.handle(member, &encode_allocate(asked, &bind_key, &client_key, None), 3);
        let RelayReply::Challenge(cookie) = reply(&challenge) else {
            panic!("expected an allocate challenge");
        };
        let granted = relay.handle(
            member,
            &encode_allocate(asked, &bind_key, &client_key, Some(&cookie)),
            4,
        );
        assert_eq!(granted.len(), 2, "Allocated and ClientJoined");
        for (bytes, _) in &granted {
            assert!(!names(bytes), "an allocation reply named a side");
        }
        assert_eq!(relay.allocation_count(), 1);
        let port = granted
            .iter()
            .find_map(|(bytes, _)| match RelayReply::decode(bytes) {
                Ok(Some(RelayReply::Allocated { port, peer })) => {
                    assert!(peer.ip().is_unspecified());
                    Some(port)
                }
                _ => None,
            });
        let port = port.expect("allocated");
        assert_eq!(
            relay.handle_allocation(port, member, b"play", 5),
            vec![(b"play".to_vec(), host)]
        );
    }

    /// Review: a renewal keeps the code and the key. A client that looked the host up before the
    /// renewal can still bind after it, and the lifetime is capped.
    #[test]
    fn a_renewal_keeps_the_join_code_and_the_bind_key() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        let code = register(&mut relay, host, 0);
        let client = addr(2, 5000);
        let lookup = encode_lookup(&code).expect("code");
        let RelayReply::HostInfo { bind_key, .. } = reply(&relay.handle(client, &lookup, 1)) else {
            panic!("expected host info")
        };
        let out = registered(&mut relay, host, u16::MAX, b"new", 2);
        assert_eq!(reply(&out), RelayReply::JoinCode(code.clone()));
        assert_eq!(relay.lobby_count(), 1);
        assert!(relay.handle(client, &encode_bind(host, &bind_key), 3).is_empty());
        assert_eq!(relay.flow_count(), 1);
        relay.expire(u64::from(RELAY_MAX_LIFETIME_S) * 1000 + 2);
        assert_eq!(relay.lobby_count(), 0, "the lifetime was capped");
    }

    /// Review: a host still sending to a client that is gone does not keep the slot taken.
    #[test]
    fn only_the_clients_own_traffic_holds_a_flow() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        register(&mut relay, host, 0);
        let key = key_of(&mut relay, host);
        let client = addr(2, 5000);
        let returning = addr(2, 5001);
        relay.handle(client, &encode_bind(host, &key), 0);
        let mut now = 0;
        while now < RELAY_BIND_REPLACE_IDLE_MS {
            now += 100;
            assert_eq!(relay.handle(host, b"state", now).len(), 1);
        }
        assert!(relay.handle(returning, &encode_bind(host, &key), now).is_empty());
        assert_eq!(
            relay.handle(host, b"state", now + 1),
            vec![(b"state".to_vec(), returning)]
        );
    }

    /// Review: a renewal that arrives just after expiry still reaches the relay with a client bound.
    #[test]
    fn a_host_renews_after_expiry_with_a_flow_bound() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        registered(&mut relay, host, 1, b"", 0);
        let key = key_of(&mut relay, host);
        let client = addr(2, 5000);
        relay.handle(client, &encode_bind(host, &key), 0);
        let out = registered(&mut relay, host, 60, b"", 1_500);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, host, "answered, not forwarded to the client");
        assert!(matches!(reply(&out), RelayReply::JoinCode(_)));
    }

    /// Audit M8 / L1: one IP cannot fill the lobby table, and the table has a hard cap.
    #[test]
    fn lobbies_are_capped_per_ip_and_in_total() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        for port in 0..RELAY_MAX_LOBBIES_PER_IP as u16 {
            register(&mut relay, addr(1, port + 1), 0);
        }
        assert_eq!(
            reply(&registered(&mut relay, addr(1, 999), 60, b"", 0)),
            RelayReply::Error(RelayError::Full)
        );
        // A host renewing its own lobby is not a new one.
        register(&mut relay, addr(1, 1), 0);
        assert_eq!(relay.lobby_count(), RELAY_MAX_LOBBIES_PER_IP as usize);
        let mut n = relay.lobby_count();
        let mut ip = 0x0B00_0000u32;
        while n < RELAY_MAX_LOBBIES {
            ip += 1;
            register(&mut relay, SocketAddr::from((Ipv4Addr::from(ip), 1)), 0);
            n += 1;
        }
        assert_eq!(
            reply(&registered(&mut relay, addr(9, 1), 60, b"", 0)),
            RelayReply::Error(RelayError::Full)
        );
        assert_eq!(relay.lobby_count(), RELAY_MAX_LOBBIES);
        relay.expire(60_000);
        assert_eq!(relay.lobby_count(), 0);
        assert!(
            relay.hosts.is_empty() && relay.per_ip.is_empty(),
            "indexes follow"
        );
    }

    /// ADR-3283 (finding 28): one IPv6 /64 is one source for the lobby cap; the next /64 is not.
    #[test]
    fn lobbies_are_capped_per_ipv6_block() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let v6 = |net: u16, host: u16| {
            SocketAddr::from((
                std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, net, host, 0, 0, 1),
                4000,
            ))
        };
        // One register a second, so the control-request budget never decides.
        let mut now = 0;
        for host in 0..RELAY_MAX_LOBBIES_PER_IP as u16 {
            register(&mut relay, v6(0, host + 1), now);
            now += 1000;
        }
        assert_eq!(
            reply(&registered(&mut relay, v6(0, 999), 60, b"", now)),
            RelayReply::Error(RelayError::Full)
        );
        register(&mut relay, v6(1, 1), now + 1000);
        assert_eq!(relay.lobby_count(), RELAY_MAX_LOBBIES_PER_IP as usize + 1);
    }

    /// An expired lobby is gone at once, even between expiry passes.
    #[test]
    fn an_expired_lobby_is_dead_between_passes() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        registered(&mut relay, host, 1, b"", 0);
        let key = key_of(&mut relay, host);
        relay.handle(addr(2, 1), &encode_list_games(), 999);
        assert_eq!(
            reply(&relay.handle(addr(3, 1), &encode_bind(host, &key), 1_000)),
            RelayReply::Error(RelayError::NotAHost)
        );
        assert_eq!(
            reply(&relay.handle(host, &encode_publish(b"card").expect("fits"), 1_000)),
            RelayReply::Error(RelayError::NotRegistered)
        );
    }

    /// Audit H2: knowing a host's address is not enough to take its traffic.
    #[test]
    fn bind_needs_the_key_from_lookup() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        let code = register(&mut relay, host, 0);
        let client = addr(2, 5000);
        let attacker = addr(3, 6000);
        assert_eq!(
            reply(&relay.handle(attacker, &encode_bind(host, &[0xAB; 16]), 1)),
            RelayReply::Error(RelayError::BadKey)
        );
        assert_eq!(relay.flow_count(), 0);
        let lookup = encode_lookup(&code).expect("code");
        let RelayReply::HostInfo { bind_key, .. } = reply(&relay.handle(client, &lookup, 2)) else {
            panic!("expected host info")
        };
        assert!(relay.handle(client, &encode_bind(host, &bind_key), 3).is_empty());
        assert_eq!(relay.flow_count(), 1);
    }

    /// Audit H2: a live flow cannot be displaced, even with the key; a quiet one can, so a client
    /// that lost its port comes back.
    #[test]
    fn a_flow_in_use_is_not_displaced() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        register(&mut relay, host, 0);
        let key = key_of(&mut relay, host);
        let client = addr(2, 5000);
        let rival = addr(3, 6000);
        relay.handle(client, &encode_bind(host, &key), 0);
        assert_eq!(relay.handle(client, b"input", 4_000).len(), 1);
        assert_eq!(
            reply(&relay.handle(
                rival,
                &encode_bind(host, &key),
                4_000 + RELAY_BIND_REPLACE_IDLE_MS - 1
            )),
            RelayReply::Error(RelayError::Busy)
        );
        assert_eq!(
            relay.handle(host, b"state", 8_000),
            vec![(b"state".to_vec(), client)]
        );
        let later = 8_000 + RELAY_BIND_REPLACE_IDLE_MS;
        assert!(relay.handle(rival, &encode_bind(host, &key), later).is_empty());
        assert_eq!(relay.flow_count(), 1, "the old flow left with both directions");
        assert_eq!(
            relay.handle(host, b"state", later + 1),
            vec![(b"state".to_vec(), rival)]
        );
        assert!(
            relay.handle(client, b"input", later + 2).is_empty(),
            "no flow, no magic"
        );
    }

    /// Audit zai M1: a bound client cannot swallow the host's renewals and listings.
    #[test]
    fn a_host_keeps_its_lobby_while_a_flow_is_bound() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        register(&mut relay, host, 0);
        let key = key_of(&mut relay, host);
        let client = addr(2, 5000);
        relay.handle(client, &encode_bind(host, &key), 0);
        let publish = encode_publish(b"card").expect("fits");
        assert!(relay.handle(host, &publish, 1).is_empty(), "not forwarded");
        assert_eq!(relay.listing_count(), 1);
        let out = registered(&mut relay, host, 600, b"", 2);
        assert!(
            matches!(reply(&out), RelayReply::JoinCode(_)),
            "parsed, not forwarded"
        );
        assert_eq!(out[0].1, host);
        relay.expire(59_000);
        assert_eq!(relay.lobby_count(), 1, "the renewal took effect");
        // Game bytes keep flowing, and the host's other control tags stay opaque.
        let tricky = encode_list_games();
        assert_eq!(relay.handle(host, &tricky, 3), vec![(tricky.clone(), client)]);
    }

    /// Audit H2 (grok): a flow is not an unlimited pipe.
    #[test]
    fn forwarding_is_limited_per_direction() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        register(&mut relay, host, 0);
        let key = key_of(&mut relay, host);
        let client = addr(2, 5000);
        relay.handle(client, &encode_bind(host, &key), 0);
        let big = vec![0u8; MAX_DATAGRAM_BYTES];
        let burst = (RELAY_FORWARD_BYTES_PER_SECOND as usize) / big.len();
        for _ in 0..burst {
            assert_eq!(relay.handle(client, &big, 0).len(), 1);
        }
        assert!(relay.handle(client, &big, 0).is_empty(), "over the rate");
        assert_eq!(relay.forward_dropped, 1);
        assert_eq!(
            relay.handle(host, &big, 0).len(),
            1,
            "the other direction has its own"
        );
        assert_eq!(relay.handle(client, &big, 500).len(), 1, "half a second later");
    }

    /// Audit M10: the relay only writes to addresses that have a part in the introduction.
    #[test]
    fn punch_notices_never_reach_a_stranger() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        let host = addr(1, 7777);
        let code = register(&mut relay, host, 0);
        let client = addr(2, 5000);
        let stranger = addr(9, 9999);
        // A host cannot aim the relay at an address that never asked.
        assert_eq!(
            reply(&relay.handle(host, &encode_punch_intro(stranger), 1)),
            RelayReply::Error(RelayError::NotAHost)
        );
        // Neither can anyone else.
        assert_eq!(
            reply(&relay.handle(client, &encode_punch_intro(stranger), 1)),
            RelayReply::Error(RelayError::NotAHost)
        );
        // A client introduces itself to the host it found.
        let out = relay.handle(client, &encode_punch_intro(host), 2);
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].1, out[1].1), (client, host));
        // A host may answer a client that looked it up, while that is recent.
        relay.handle(client, &encode_lookup(&code).expect("code"), 3);
        assert_eq!(relay.handle(host, &encode_punch_intro(client), 4).len(), 2);
        let late = 3 + RELAY_INTRO_WINDOW_MS;
        assert_eq!(
            reply(&relay.handle(host, &encode_punch_intro(client), late)),
            RelayReply::Error(RelayError::NotAHost)
        );
    }

    #[test]
    fn punch_outcomes_follow_nat_behavior() {
        use NatFiltering as F;
        use NatMapping as M;
        let relay = addr(100, RELAY_DEFAULT_PORT);
        let nat = |m, f, ip: u8| SimNat::new(m, f, Ipv4Addr::new(198, 51, 100, ip));
        let cases = [
            (
                (M::EndpointIndependent, F::EndpointIndependent),
                (M::EndpointIndependent, F::EndpointIndependent),
                PunchResult::Direct,
            ),
            (
                (M::EndpointIndependent, F::AddressAndPortDependent),
                (M::EndpointIndependent, F::AddressAndPortDependent),
                PunchResult::Direct,
            ),
            (
                (M::AddressAndPortDependent, F::AddressAndPortDependent),
                (M::EndpointIndependent, F::EndpointIndependent),
                PunchResult::Direct,
            ),
            (
                (M::AddressAndPortDependent, F::AddressAndPortDependent),
                (M::EndpointIndependent, F::AddressAndPortDependent),
                PunchResult::NeedsRelay,
            ),
            (
                (M::AddressAndPortDependent, F::AddressAndPortDependent),
                (M::AddressAndPortDependent, F::AddressAndPortDependent),
                PunchResult::NeedsRelay,
            ),
        ];
        for (i, (a, b, expect)) in cases.into_iter().enumerate() {
            let mut na = nat(a.0, a.1, 1);
            let mut nb = nat(b.0, b.1, 2);
            assert_eq!(simulate_hole_punch(&mut na, &mut nb, relay), expect, "case {i}");
        }
    }

    #[test]
    fn hostile_relay_bytes_never_panic() {
        let mut relay = RelayCore::with_secret(1, [7; 32]);
        for tag in 0..=255u8 {
            // Past each padded request size too; a source per tag stays under the rate limit.
            for len in (0..12usize).chain([16, 64, 320, 1200, 1400]) {
                for fill in [0x00, 0xFF] {
                    let mut d = RELAY_MAGIC.to_vec();
                    d.push(tag);
                    d.extend(std::iter::repeat_n(fill, len));
                    // Time moves on between tags, so two tags sharing a bucket stay under it too.
                    let _ = relay.handle(addr(tag, 1), &d, u64::from(tag) * 2_000);
                    let _ = RelayReply::decode(&d);
                }
            }
        }
        assert_eq!(relay.rate_limited, 0, "every shape above reached the parser");
    }

    /// Relay protocol 3 (ADR-8340): a host's candidates ride its `Register` and survive a renewal
    /// that does not carry them; an introduction resolves the code and answers both sides with each
    /// other's reflexive address, candidates and one session key, never more bytes than it was sent;
    /// a short request is dropped and an unknown code refused.
    #[test]
    fn an_introduction_carries_both_sides_candidates_and_never_outweighs_its_request() {
        use crate::nat::{Candidate, CandidateKind, NAT_MAX_CANDIDATES};
        let mut relay = RelayCore::with_secret(3, [8; 32]);
        let host = addr(1, 7777);
        let host_cands: Vec<Candidate> = (0..NAT_MAX_CANDIDATES as u8)
            .map(|i| {
                Candidate::new(
                    CandidateKind::Host,
                    SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, u16::from(i)], 7777)),
                )
            })
            .collect();
        let token = vec![0xAB; RELAY_TOKEN_MAX_BYTES];
        let first = encode_register_with_candidates(60, &token, None, &host_cands).expect("fits");
        let RelayReply::RegisterChallenge(cookie) = reply(&relay.handle(host, &first, 0)) else {
            panic!("challenge")
        };
        let again = encode_register_with_candidates(60, &token, Some(&cookie), &host_cands).expect("fits");
        let RelayReply::JoinCode(code) = reply(&relay.handle(host, &again, 0)) else {
            panic!("code")
        };
        // A protocol 1 renewal (no extension) keeps the candidates.
        let renew = encode_register_with_cookie(60, &token, &cookie).expect("fits");
        assert!(matches!(
            reply(&relay.handle(host, &renew, 1)),
            RelayReply::JoinCode(_)
        ));

        let client = addr(2, 5000);
        let client_cands: Vec<Candidate> = (0..NAT_MAX_CANDIDATES as u8)
            .map(|i| {
                Candidate::new(
                    CandidateKind::Host,
                    SocketAddr::from(([0x2001, 0xdb8, 1, 0, 0, 0, 0, u16::from(i)], 5000)),
                )
            })
            .collect();
        let intro = encode_introduce(&code, &client_cands).expect("a code");
        assert_eq!(intro.len(), RELAY_INTRO_REQUEST_BYTES);
        assert_eq!(
            relay.handle(client, &intro[..RELAY_INTRO_REQUEST_BYTES - 1], 2),
            Vec::new(),
            "short"
        );
        let out = relay.handle(client, &intro, 2);
        assert_eq!(out.len(), 2);
        let sent: usize = out.iter().map(|(b, _)| b.len()).sum();
        assert!(
            sent <= intro.len(),
            "{sent} bytes out for {} in (largest ticket, IPv6, 8 + 8 candidates)",
            intro.len()
        );
        let decode = |to: SocketAddr| {
            let (bytes, _) = out.iter().find(|(_, d)| *d == to).expect("an answer");
            match RelayReply::decode(bytes).expect("valid").expect("relay") {
                RelayReply::Introduced(i) => i,
                other => panic!("{other:?}"),
            }
        };
        let to_client = decode(client);
        let to_host = decode(host);
        assert_eq!(to_client.peer, host);
        assert_eq!(to_client.observed, client);
        assert_eq!(to_client.token, token);
        assert_eq!(
            to_client.candidates, host_cands,
            "kept across the protocol 1 renewal"
        );
        assert_eq!(to_client.bind_key, key_of(&mut relay, host));
        assert_eq!(to_host.peer, client);
        assert_eq!(to_host.candidates, client_cands);
        assert!(to_host.token.is_empty());
        assert_eq!(to_client.session, to_host.session, "one session key for both");
        let second = relay.handle(client, &intro, 3);
        let RelayReply::Introduced(again) = RelayReply::decode(&second[0].0).expect("valid").expect("relay")
        else {
            panic!("introduced")
        };
        assert_ne!(again.session, to_client.session, "a fresh key per introduction");
        assert_eq!(relay.introductions, 2);
        let unknown = encode_introduce("ZZZZZZ", &[]).expect("a code");
        assert_eq!(
            reply(&relay.handle(addr(3, 1), &unknown, 4)),
            RelayReply::Error(RelayError::UnknownCode)
        );
    }
}
