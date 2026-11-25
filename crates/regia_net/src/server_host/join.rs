// SPDX-License-Identifier: MPL-2.0
//! Join codes for a hosted server (ADR-2440): the host registers with a rendezvous relay
//! ([`crate::relay`]) and deposits a [`JoinTicket`] (the game port and the server key) as the
//! lobby's opaque token; a client looks the code up and connects straight to the game port, with
//! the key pinned.
//!
//! The relay records the reflexive address of the socket that registered, which is not the game
//! port (the endpoint owns that socket), so the ticket carries the port. A ticket whose address is
//! unspecified (`0.0.0.0`, a server bound to every interface) takes the IP the relay saw: on one
//! machine that is `127.0.0.1`, behind a NAT the public address.
//!
//! Everything here is sans-clock: [`RelayRegistrar::update`] and [`JoinResolver::update`] take
//! the caller's milliseconds, so a test drives them from a manual clock. [`LocalRelay`] is a relay
//! inside the host process (`--relay local`), for dev runs and tests on one machine.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use crate::crypto::ServerKey;
use crate::handshake::{decode_addr, encode_addr};
use crate::host_check::{
    HostCheckConfig, HostCheckReport, MappingView, PortForwardCheck, Reachability, UplinkProbe,
    estimate_clients,
};
use crate::join_code::normalize_join_code;
// `LinkSocket` stays the parameter name of `RelayRegistrar::new` (the API snapshot). With `ecs` it
// is the link's socket; without `ecs` it is the same box, public so the registrar's signature does
// not name a private type (ADR-10376).
#[cfg(feature = "ecs")]
use crate::link::LinkSocket;
/// The socket a [`RelayRegistrar`] or [`JoinResolver`] owns in a build without `ecs`:
/// `Box<dyn DatagramSocket + Send>`, the same type as `link::LinkSocket` in an `ecs` build.
#[cfg(not(feature = "ecs"))]
pub type LinkSocket = Box<dyn crate::socket::DatagramSocket + Send>;
use crate::nat::{
    Candidate, CandidateKind, CandidateSource, CheckAgent, CheckPacer, HostCandidates, NAT_MAX_SESSIONS,
    NAT_SESSION_LIFETIME_MS, NatReport, Role, decode_check, gather, is_check,
};
use crate::port_map::PortMapping;
use crate::relay::{
    RELAY_DEFAULT_PORT, RELAY_MAX_LIFETIME_S, RelayConfig, RelayError, RelayReply, RelayServer, RelayStatus,
    encode_lookup, encode_register, encode_register_hidden, encode_register_with_candidates,
};
use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES, UdpTransport};
use crate::wire::{WireError, WireReader, WireWriter};

/// First bytes of a [`JoinTicket`].
pub const JOIN_TICKET_MAGIC: [u8; 4] = *b"RGJT";
/// Version byte of the ticket layout (`format_version` of this wire shape).
pub const JOIN_TICKET_VERSION: u8 = 1;
/// How often an unanswered `Register` or `Lookup` is sent again, milliseconds.
pub const JOIN_RETRY_MS: u64 = 250;
/// `Lookup` attempts before a resolver gives up (at [`JOIN_RETRY_MS`], about five seconds).
pub const JOIN_LOOKUP_ATTEMPTS: u32 = 20;
/// Lobby lifetime the host asks for, seconds; it renews at half of it, or every
/// [`JOIN_KEEPALIVE_MS`] when that is sooner.
pub const JOIN_LOBBY_LIFETIME_S: u16 = 600;
/// How often a registered host renews its lobby, milliseconds: often enough that the NAT mapping
/// of its game socket toward the relay stays open (the relay sends `ClientJoined` there; RFC 8445
/// § 11 keepalives are 15 s or more), so a client can join at any time.
pub const JOIN_KEEPALIVE_MS: u64 = 15_000;
/// Relay allocations a registrar confirms at once (above the relay's per-host default of 40).
pub const JOIN_MAX_PENDING_ALLOCATIONS: usize = 256;
/// `Refresh` attempts a registrar makes for one new allocation before it gives up on it.
pub const JOIN_ALLOCATION_ATTEMPTS: u32 = 16;

/// One relayed client the host is confirming (relay protocol 2, spec 120 § 10.6).
#[derive(Debug, Clone)]
struct HostAllocation {
    port: u16,
    host_key: [u8; crate::relay::RELAY_ALLOCATION_KEY_BYTES],
    cookie: Option<[u8; crate::relay::RELAY_COOKIE_BYTES]>,
    attempts: u32,
    next_send_ms: u64,
}

/// What a join code resolves to: where the game port is and the key the server proves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinTicket {
    /// The game port. An unspecified IP means "the address the relay saw".
    pub addr: SocketAddr,
    /// The server's public key (clients pin it); `None` for a server that does not encrypt.
    pub server_key: Option<ServerKey>,
}

impl JoinTicket {
    /// The token bytes: `"RGJT" | version u8 | addr | has_key u8 | key [32]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(64);
        w.raw(&JOIN_TICKET_MAGIC);
        w.u8(JOIN_TICKET_VERSION);
        encode_addr(&mut w, self.addr);
        match &self.server_key {
            Some(key) => {
                w.u8(1);
                w.raw(&key.0);
            }
            None => w.u8(0),
        }
        w.finish()
    }

    /// Reads [`Self::encode`]'s bytes.
    ///
    /// # Errors
    ///
    /// Not a ticket, an unknown version, or trailing bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = WireReader::new(bytes);
        if r.array::<4>()? != JOIN_TICKET_MAGIC {
            return Err(WireError::Invalid("join ticket magic"));
        }
        if r.u8()? != JOIN_TICKET_VERSION {
            return Err(WireError::Invalid("join ticket version"));
        }
        let addr = decode_addr(&mut r)?;
        let server_key = match r.u8()? {
            0 => None,
            1 => Some(ServerKey(r.array()?)),
            _ => return Err(WireError::Invalid("join ticket key flag")),
        };
        r.finish()?;
        Ok(Self { addr, server_key })
    }

    /// The ticket of a host that registers from its game socket (spec 120 § 10.7): the address
    /// is unspecified with port 0, "the address the relay saw is the game port".
    pub fn from_game_socket(server_key: Option<ServerKey>) -> Self {
        Self {
            addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            server_key,
        }
    }

    /// The address to connect to: the ticket's own; with an unspecified IP the IP the relay
    /// recorded for the host (`seen`) and the ticket's port; with an unspecified IP and port 0
    /// (a host that registered from its game socket) `seen` itself.
    pub fn connect_addr(&self, seen: SocketAddr) -> SocketAddr {
        if self.addr.ip().is_unspecified() && self.addr.port() == 0 {
            seen
        } else if self.addr.ip().is_unspecified() {
            SocketAddr::new(seen.ip(), self.addr.port())
        } else {
            self.addr
        }
    }
}

/// Why a join code could not be minted or resolved. [`JoinError::code`] is the stable code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JoinError {
    /// The text is not a join code (six letters and digits, `-` and spaces ignored).
    #[error("not a join code: {0}")]
    BadCode(String),
    /// The relay address does not parse or resolve.
    #[error("relay address: {0}")]
    BadRelay(String),
    /// The socket toward the relay could not be bound.
    #[error("relay socket: {0}")]
    Bind(String),
    /// The relay refused (`unknown_code`, `full`, ...).
    #[error("the relay refused: {0}")]
    Refused(&'static str),
    /// The relay's token is not a join ticket of this engine.
    #[error("the lobby's token is not a join ticket: {0}")]
    BadTicket(String),
    /// No answer after every attempt.
    #[error("no answer from the relay")]
    Timeout,
    /// The direct path failed and the relay speaks protocol 1, which has no allocation per
    /// client (relay protocol 2, spec 120 § 10.6): update the relay.
    #[error("the direct path failed and the relay is too old to relay this client (protocol 1)")]
    RelayTooOld,
    /// The direct path failed and the relay refused an allocation: a cap of this value is reached.
    #[error("the direct path failed and the relay is full (cap {0})")]
    RelayFull(u16),
    /// The direct path failed and the relay never confirmed an allocation.
    #[error("the direct path failed and the relay gave no allocation")]
    RelayNoAllocation,
}

impl JoinError {
    /// Stable lowercase code (agent protocol, logs, the client's exit message).
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadCode(_) => "join_code_invalid",
            Self::BadRelay(_) => "join_relay_invalid",
            Self::Bind(_) => "join_relay_bind_failed",
            Self::Refused(_) => "join_refused",
            Self::BadTicket(_) => "join_ticket_invalid",
            Self::Timeout => "join_timeout",
            Self::RelayTooOld => "relay_too_old",
            Self::RelayFull(_) => "relay_full",
            Self::RelayNoAllocation => "relay_no_allocation",
        }
    }
}

/// The lowercase name of a relay refusal.
pub fn relay_error_name(e: RelayError) -> &'static str {
    match e {
        RelayError::UnknownCode => "unknown_code",
        RelayError::NotAHost => "not_a_host",
        RelayError::BadSize => "bad_size",
        RelayError::Full => "full",
        RelayError::NotRegistered => "not_registered",
        RelayError::BadKey => "bad_key",
        RelayError::Busy => "busy",
        RelayError::ProbeTarget => "probe_target",
    }
}

/// Where the host registers: `--relay local`, `--relay local:PORT`, or `--relay HOST:PORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelaySpec {
    /// A relay inside the host on `127.0.0.1:PORT` (0 picks a free port).
    Local(u16),
    /// A relay elsewhere.
    Remote(SocketAddr),
}

impl RelaySpec {
    /// Parses the `--relay` value. A host name resolves once, here.
    ///
    /// # Errors
    ///
    /// [`JoinError::BadRelay`] when the text is neither `local[:PORT]` nor an address.
    pub fn parse(text: &str) -> Result<Self, JoinError> {
        let text = text.trim();
        if text == "local" {
            return Ok(Self::Local(RELAY_DEFAULT_PORT));
        }
        if let Some(port) = text.strip_prefix("local:") {
            return port
                .parse()
                .map(Self::Local)
                .map_err(|_| JoinError::BadRelay(format!("`{text}`: local:PORT")));
        }
        if let Ok(addr) = text.parse() {
            return Ok(Self::Remote(addr));
        }
        text.to_socket_addrs()
            .ok()
            .and_then(|mut addrs| addrs.next())
            .map(Self::Remote)
            .ok_or_else(|| JoinError::BadRelay(format!("`{text}`: local, local:PORT or HOST:PORT")))
    }
}

/// The local address a socket toward `relay` binds: loopback for a loopback relay (the relay then
/// records `127.0.0.1`), any interface otherwise, in the relay's address family.
pub fn registrar_bind(relay: SocketAddr) -> SocketAddr {
    match relay.ip() {
        IpAddr::V4(ip) if ip.is_loopback() => SocketAddr::from((Ipv4Addr::LOCALHOST, 0)),
        IpAddr::V4(_) => SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
        IpAddr::V6(ip) if ip.is_loopback() => SocketAddr::from((Ipv6Addr::LOCALHOST, 0)),
        IpAddr::V6(_) => SocketAddr::from((Ipv6Addr::UNSPECIFIED, 0)),
    }
}

/// The game socket's port mapping as a candidate source (spec 120 § 10.7): the hosting flow
/// writes the mapped address into the shared slot when the race wins or a renewal moves it, and
/// clears it when the mapping lapses. Clone the slot with [`MappedCandidates::slot`].
#[derive(Debug, Clone, Default)]
pub struct MappedCandidates {
    slot: std::sync::Arc<std::sync::Mutex<Option<SocketAddr>>>,
}

impl MappedCandidates {
    /// An empty source (no mapping yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared slot (another handle to the same mapping).
    pub fn slot(&self) -> Self {
        self.clone()
    }

    /// Sets (or clears) the mapped address.
    pub fn set(&self, addr: Option<SocketAddr>) {
        if let Ok(mut slot) = self.slot.lock() {
            *slot = addr;
        }
    }

    /// The mapped address, if any.
    pub fn get(&self) -> Option<SocketAddr> {
        self.slot.lock().ok().and_then(|s| *s)
    }
}

impl CandidateSource for MappedCandidates {
    fn candidates(&mut self, _base: SocketAddr, _now_ms: u64) -> Vec<Candidate> {
        self.get()
            .map(|a| vec![Candidate::new(CandidateKind::Mapped, a)])
            .unwrap_or_default()
    }
}

/// A running host check (spec 120 § 10.7): its configuration and the two measurements.
#[derive(Debug)]
struct HostCheckRun {
    config: HostCheckConfig,
    forward: Option<PortForwardCheck>,
    uplink: Option<UplinkProbe>,
}

/// One client's connectivity-check session on the host (relay protocol 3, spec 120 § 10.7).
#[derive(Debug)]
struct HostSession {
    agent: CheckAgent,
    nominated: bool,
}

/// The host's side: keeps a lobby registered on the relay and knows its join code. A registrar on
/// the game socket ([`JoinTicket::from_game_socket`]) also runs the host's side of NAT traversal:
/// it announces the game socket's candidates with each `Register`, and for each client the relay
/// introduces it answers that client's connectivity checks and checks the client's candidates
/// back, paced (spec 120 § 10.7, `crate::nat`).
pub struct RelayRegistrar {
    socket: LinkSocket,
    relay: SocketAddr,
    token: Vec<u8>,
    lifetime_s: u16,
    cookie: Option<[u8; crate::relay::RELAY_COOKIE_BYTES]>,
    code: Option<String>,
    next_send_ms: u64,
    last_refusal: Option<RelayError>,
    sent: u64,
    pending: Vec<HostAllocation>,
    relayed_clients: u64,
    confirmed: u64,
    /// NAT traversal runs (the registrar is on the game socket).
    traversal: bool,
    /// Relay-only (spec 89 § 16.4): `Register` carries no candidates, and this socket sends no
    /// punch introduction and answers no connectivity check.
    hide_reach: bool,
    sources: Vec<Box<dyn CandidateSource>>,
    candidates: Vec<Candidate>,
    gathered: bool,
    sessions: Vec<HostSession>,
    pacer: CheckPacer,
    next_session: usize,
    nat: NatReport,
    /// The port mapping's candidate (spec 120 § 10.7, net-nat-hosting-flow).
    mapped: MappedCandidates,
    mapping: Option<MappingView>,
    mapping_error: Option<String>,
    host_check: Option<HostCheckRun>,
    /// [`Self::revoke_code`] ran: no renewal; a `Register` with lifetime 0 until the relay
    /// confirms (N-19).
    revoke: Option<Revoke>,
}

/// The state of a code withdrawal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Revoke {
    /// Not yet sent with the relay's cookie (the relay answers a challenge first).
    Sending,
    /// A `Register` with lifetime 0 went out with the cookie.
    SentZero,
    Confirmed,
}

impl std::fmt::Debug for RelayRegistrar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayRegistrar")
            .field("relay", &self.relay)
            .field("code", &self.code)
            .field("sent", &self.sent)
            .finish_non_exhaustive()
    }
}

impl RelayRegistrar {
    /// A registrar on `socket` for `ticket`; the first `Register` goes out on the first update.
    ///
    /// Register from the game socket (spec 120 § 10.7): pass the control half of
    /// [`super::relay_path::relay_split`] of the game socket and [`JoinTicket::from_game_socket`],
    /// so the address the relay records, hands to clients and relays to is the game's. The
    /// registrar then also confirms each relayed client's allocation (`ClientJoined`, a `Refresh`
    /// from the game socket).
    ///
    /// # Errors
    ///
    /// The ticket does not fit a relay token (never for this layout).
    pub fn new(socket: LinkSocket, relay: SocketAddr, ticket: &JoinTicket) -> Result<Self, JoinError> {
        let token = ticket.encode();
        encode_register(JOIN_LOBBY_LIFETIME_S, &token).map_err(|e| JoinError::BadTicket(e.to_string()))?;
        let traversal = ticket.addr == JoinTicket::from_game_socket(None).addr;
        let mapped = MappedCandidates::new();
        Ok(Self {
            socket,
            relay,
            token,
            lifetime_s: JOIN_LOBBY_LIFETIME_S.min(RELAY_MAX_LIFETIME_S),
            cookie: None,
            code: None,
            next_send_ms: 0,
            last_refusal: None,
            sent: 0,
            pending: Vec::new(),
            relayed_clients: 0,
            confirmed: 0,
            traversal,
            hide_reach: false,
            sources: vec![
                Box::new(HostCandidates { toward: relay }),
                Box::new(mapped.slot()),
            ],
            candidates: Vec::new(),
            gathered: false,
            sessions: Vec::new(),
            pacer: CheckPacer::default(),
            next_session: 0,
            nat: NatReport::default(),
            mapped,
            mapping: None,
            mapping_error: None,
            host_check: None,
            revoke: None,
        })
    }

    /// Withdraws the join code on the relay now (N-19, ADR-10069): the registrar stops renewing
    /// and sends a `Register` with a lifetime of 0 until the relay confirms; from then a `Lookup`
    /// of the code fails `UnknownCode` (a relay built before this change clamps the 0 to one
    /// second and answers the code: the lobby lapses within that second). Players already joined stay. [`Self::join_code`] is `None`
    /// at once. A host that wants a code again makes a new registrar (a new code).
    pub fn revoke_code(&mut self, now_ms: u64) {
        if self.revoke.is_none() {
            self.revoke = Some(Revoke::Sending);
            self.code = None;
            self.next_send_ms = now_ms;
        }
    }

    /// True once the relay confirmed a [`Self::revoke_code`].
    pub fn code_revoked(&self) -> bool {
        self.revoke == Some(Revoke::Confirmed)
    }

    /// Starts the host check (spec 120 § 10.7): the uplink measurement, and the port-forward
    /// self-check of the mapping once [`Self::set_port_mapping`] gave one. Both run once the relay
    /// registered the host (they need a lobby). Starting again restarts both.
    pub fn start_host_check(&mut self, config: HostCheckConfig, now_ms: u64) {
        self.host_check = Some(HostCheckRun {
            config,
            forward: self.mapped.get().map(|a| PortForwardCheck::new(a, now_ms)),
            uplink: None,
        });
    }

    /// The game socket's port mapping, granted or renewed by `mapper` (the hosting flow's race,
    /// [`crate::port_map::race_port_mappers`]): announced as a mapped candidate with the next
    /// `Register` (sent at once), and checked again when its address is new.
    pub fn set_port_mapping(&mut self, mapper: &str, mapping: &PortMapping, renewals: u64, now_ms: u64) {
        let moved = self.mapped.get() != Some(mapping.external);
        self.mapping = Some(MappingView::new(mapper, mapping, renewals));
        self.mapping_error = None;
        if moved {
            self.mapped.set(Some(mapping.external));
            self.gathered = false;
            self.next_send_ms = now_ms;
            if let Some(run) = &mut self.host_check {
                run.forward = Some(PortForwardCheck::new(mapping.external, now_ms));
            }
        }
    }

    /// No port mapping (the race failed with `error`, the mapping lapsed, or the host turned it
    /// off): the mapped candidate is withdrawn with the next `Register`.
    pub fn clear_port_mapping(&mut self, error: &str, now_ms: u64) {
        self.mapping = None;
        self.mapping_error = Some(error.to_owned());
        if self.mapped.get().is_some() {
            self.mapped.set(None);
            self.gathered = false;
            self.next_send_ms = now_ms;
        }
        if let Some(run) = &mut self.host_check {
            run.forward = None;
        }
    }

    /// The host check as `net.host_check` reports it; `None` before [`Self::start_host_check`].
    pub fn host_check(&self) -> Option<HostCheckReport> {
        let run = self.host_check.as_ref()?;
        let reachable = run
            .forward
            .as_ref()
            .map_or(Reachability::NoMapping, PortForwardCheck::state);
        let uplink = run
            .uplink
            .as_ref()
            .and_then(UplinkProbe::estimate_bytes_per_second);
        let uplink_end = run.uplink.as_ref().map(|u| u.end());
        let uplink_done = !run.config.measure_uplink
            || uplink_end.is_some_and(|e| e != crate::host_check::UplinkEnd::Running);
        let mapping_done = self.mapping.is_some() || self.mapping_error.is_some();
        Some(HostCheckReport {
            mapping: self.mapping.clone(),
            mapping_error: self.mapping_error.clone(),
            reachable: reachable.as_str().to_owned(),
            uplink_bytes_per_second: uplink,
            uplink_end: uplink_end.map(|e| e.as_str().to_owned()),
            per_client_bytes_per_second: run.config.per_client_bytes_per_second,
            estimated_clients: uplink.map(|u| estimate_clients(u, run.config.per_client_bytes_per_second)),
            done: uplink_done && mapping_done && reachable.is_final(),
        })
    }

    /// Sends what the host check has due (after registration only: the relay answers hosts).
    fn run_host_check(&mut self, now_ms: u64) {
        if self.code.is_none() {
            return;
        }
        let Some(run) = &mut self.host_check else {
            return;
        };
        let mut out: Vec<Vec<u8>> = Vec::new();
        if let Some(forward) = &mut run.forward
            && let Some(bytes) = forward.poll(now_ms)
        {
            out.push(bytes);
        }
        if run.config.measure_uplink {
            let uplink = run.uplink.get_or_insert_with(|| UplinkProbe::new(now_ms));
            while let Some(bytes) = uplink.poll(now_ms) {
                out.push(bytes);
            }
        }
        for bytes in out {
            if let Err(e) = self.socket.send_to(&bytes, self.relay) {
                tracing::debug!(error = %e, relay = %self.relay, "host check: send error");
            }
        }
    }

    /// A relay reply for the host check.
    fn on_host_check_reply(&mut self, reply: &RelayReply, now_ms: u64) {
        let Some(run) = &mut self.host_check else {
            return;
        };
        if let Some(forward) = &mut run.forward {
            forward.on_reply(reply, now_ms);
        }
        if let Some(uplink) = &mut run.uplink {
            uplink.on_reply(reply, now_ms);
        }
    }

    /// Hide this socket's addresses (spec 89 § 16.4, ADR-10391).
    ///
    /// Call it before the first [`Self::update`] when the lobby, or this member's plan, is
    /// relay-only. `Register` still renews the join code and the registrar still confirms relay
    /// allocations. The ticket is rewritten with [`JoinTicket::from_game_socket`], so the token
    /// carries no address. No host, reflexive or mapped candidate is gathered or sent. The
    /// register carries the hide-address byte, so the relay does not publish this socket.
    /// An introduction is ignored, and connectivity checks are not answered and not sent.
    pub fn relay_only(mut self) -> Self {
        if let Ok(ticket) = JoinTicket::decode(&self.token) {
            self.token = JoinTicket::from_game_socket(ticket.server_key).encode();
        }
        self.hide_reach = true;
        self.sources.clear();
        self.candidates.clear();
        self.gathered = true;
        self
    }

    /// Adds a source of candidates (a port mapping, from net-nat-hosting-flow); the next
    /// `Register` announces what it gives. Only a registrar on the game socket uses candidates.
    /// A [`Self::relay_only`] registrar ignores the source.
    pub fn add_candidate_source(&mut self, source: Box<dyn CandidateSource>) {
        self.sources.push(source);
        self.gathered = false;
    }

    /// The candidates the last `Register` announced (empty off the game socket).
    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// NAT traversal counters (`net.relay_status`'s `nat`).
    pub fn nat_report(&self) -> NatReport {
        let mut report = self.nat.clone();
        report.candidates = self.candidates.iter().map(Into::into).collect();
        report.active_sessions = self.sessions.len() as u64;
        report
    }

    /// The checks a client sent the game socket: answer them (and learn peer-reflexive
    /// candidates) when they carry a live session's key.
    fn on_check(&mut self, bytes: &[u8], from: SocketAddr, now_ms: u64) {
        let Some(check) = decode_check(bytes) else {
            self.nat.checks_dropped += 1;
            return;
        };
        let session = match check {
            crate::nat::Check::Request { session, .. } | crate::nat::Check::Response { session, .. } => {
                session
            }
        };
        let Some(s) = self
            .sessions
            .iter_mut()
            .find(|s| crate::wire::constant_time_eq(s.agent.session(), &session))
        else {
            self.nat.checks_dropped += 1;
            return;
        };
        let before = s.agent.counts();
        let answer = s.agent.on_check(&check, from, now_ms);
        let after = s.agent.counts();
        self.nat.checks_answered += after.requests_answered - before.requests_answered;
        self.nat.checks_dropped += after.dropped - before.dropped;
        if !s.nominated && s.agent.nominated().is_some() {
            s.nominated = true;
            self.nat.nominated += 1;
        }
        if let Some((bytes, to)) = answer
            && let Err(e) = self.socket.send_to(&bytes, to)
        {
            tracing::debug!(error = %e, %to, "join code: check answer send error");
        }
    }

    /// A client the relay introduced: start checking its candidates.
    fn on_introduced(&mut self, intro: crate::relay::Introduction, now_ms: u64) {
        if !self.traversal {
            return;
        }
        if self
            .sessions
            .iter()
            .any(|s| crate::wire::constant_time_eq(s.agent.session(), &intro.session))
        {
            // The relay's answer came twice (the client sent its introduction again).
            return;
        }
        if self.sessions.len() >= NAT_MAX_SESSIONS {
            let old = self.sessions.remove(0);
            if !old.nominated {
                self.nat.not_nominated += 1;
            }
        }
        let mut remote = intro.candidates;
        remote.push(Candidate::new(CandidateKind::ServerReflexive, intro.peer));
        self.nat.sessions += 1;
        self.sessions.push(HostSession {
            agent: CheckAgent::new(Role::Controlled, intro.session, &self.candidates, &remote, now_ms),
            nominated: false,
        });
    }

    /// Ends old sessions and sends the checks that are due: one per session per Ta, every
    /// session together at most one per `nat::NAT_CHECK_FLOOR_MS`, sessions in turn.
    fn run_checks(&mut self, now_ms: u64) {
        let mut ended = 0u64;
        self.sessions.retain(|s| {
            let live = now_ms.saturating_sub(s.agent.started_ms()) < NAT_SESSION_LIFETIME_MS;
            if !live && !s.nominated {
                ended += 1;
            }
            live
        });
        self.nat.not_nominated += ended;
        let n = self.sessions.len();
        for k in 0..n {
            let i = (self.next_session + k) % n;
            if self.sessions[i].agent.next_check_ms() > now_ms {
                continue;
            }
            if !self.pacer.take(now_ms) {
                self.next_session = i;
                return;
            }
            if let Some((bytes, to)) = self.sessions[i].agent.poll(now_ms) {
                self.nat.checks_sent += 1;
                if let Err(e) = self.socket.send_to(&bytes, to) {
                    tracing::debug!(error = %e, %to, "join code: check send error");
                }
            }
        }
        if n > 0 {
            self.next_session = (self.next_session + 1) % n;
        }
    }

    /// Relayed clients the relay announced (`ClientJoined`) since the start.
    pub fn relayed_clients(&self) -> u64 {
        self.relayed_clients
    }

    /// Allocations whose host side the relay confirmed (`Allocated` on the allocation port).
    pub fn confirmed_allocations(&self) -> u64 {
        self.confirmed
    }

    /// The host's registration as `net.relay_status` reports it (`role` `"host"`).
    pub fn report(&self) -> crate::relay::RelayReport {
        crate::relay::RelayReport {
            role: "host".to_owned(),
            relay: self.relay.to_string(),
            join_code: self.code.clone(),
            relayed_clients: self.relayed_clients,
            confirmed_allocations: self.confirmed,
            nat: self.traversal.then(|| self.nat_report()),
            host_check: self.host_check(),
            ..crate::relay::RelayReport::default()
        }
    }

    /// The host side of a new allocation: a `Refresh` from this socket to the allocation port
    /// opens the host's NAT toward it and, when the mapping differs from the registered one (an
    /// address-dependent NAT), moves the permission to it after a cookie round trip.
    fn on_client_joined(
        &mut self,
        port: u16,
        host_key: [u8; crate::relay::RELAY_ALLOCATION_KEY_BYTES],
        now_ms: u64,
    ) {
        self.relayed_clients += 1;
        if let Some(p) = self.pending.iter_mut().find(|p| p.port == port) {
            // A port handed out again (the old client left): a new key, start over.
            p.host_key = host_key;
            p.cookie = None;
            p.attempts = 0;
            p.next_send_ms = now_ms;
            return;
        }
        if self.pending.len() >= JOIN_MAX_PENDING_ALLOCATIONS {
            self.pending.remove(0);
        }
        self.pending.push(HostAllocation {
            port,
            host_key,
            cookie: None,
            attempts: 0,
            next_send_ms: now_ms,
        });
    }

    fn send_refreshes(&mut self, now_ms: u64) {
        let relay_ip = self.relay.ip();
        let mut i = 0;
        while i < self.pending.len() {
            let p = &mut self.pending[i];
            if p.attempts >= JOIN_ALLOCATION_ATTEMPTS {
                tracing::debug!(
                    port = p.port,
                    "join code: a relayed client's allocation was never confirmed"
                );
                self.pending.remove(i);
                continue;
            }
            if now_ms >= p.next_send_ms {
                let bytes = crate::relay::encode_refresh(&p.host_key, p.cookie.as_ref());
                p.attempts += 1;
                p.next_send_ms = now_ms.saturating_add(JOIN_RETRY_MS);
                let to = SocketAddr::new(relay_ip, p.port);
                if let Err(e) = self.socket.send_to(&bytes, to) {
                    tracing::debug!(error = %e, %to, "join code: refresh send error");
                }
            }
            i += 1;
        }
    }

    /// Binds a UDP socket toward `relay` (see [`registrar_bind`]) and makes a registrar.
    ///
    /// # Errors
    ///
    /// The socket cannot be bound.
    pub fn udp(relay: SocketAddr, ticket: &JoinTicket) -> Result<Self, JoinError> {
        let socket = UdpTransport::bind(registrar_bind(relay)).map_err(|e| JoinError::Bind(e.to_string()))?;
        Self::new(Box::new(socket), relay, ticket)
    }

    /// The join code, once the relay gave one.
    pub fn join_code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// The relay's address.
    pub fn relay(&self) -> SocketAddr {
        self.relay
    }

    /// The relay's last refusal, if any.
    pub fn last_refusal(&self) -> Option<&'static str> {
        self.last_refusal.map(relay_error_name)
    }

    /// `Register` datagrams sent so far.
    pub fn sent(&self) -> u64 {
        self.sent
    }

    /// Reads the relay's replies and sends a `Register` when one is due (first, after a
    /// challenge, on a retry, and at half the lifetime to renew). True when this call learned the
    /// join code (the first time, or a different one after the lobby expired).
    pub fn update(&mut self, now_ms: u64) -> bool {
        let mut learned = false;
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        // A running uplink measurement brings thousands of acknowledgements a second.
        let measuring = self
            .host_check
            .as_ref()
            .and_then(|r| r.uplink.as_ref())
            .is_some_and(|u| u.end() == crate::host_check::UplinkEnd::Running);
        let reads = if measuring { 4096 } else { 64 };
        for _ in 0..reads {
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(Some(got)) => got,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "join code: relay receive error");
                    break;
                }
            };
            if is_check(&buf[..n]) {
                if self.traversal && !self.hide_reach {
                    self.on_check(&buf[..n], from, now_ms);
                }
                continue;
            }
            if from.ip() != self.relay.ip() {
                continue;
            }
            if from != self.relay {
                // An allocation port: the host side of a relayed client's allocation.
                match RelayReply::decode(&buf[..n]) {
                    Ok(Some(RelayReply::Challenge(cookie))) => {
                        if let Some(p) = self.pending.iter_mut().find(|p| p.port == from.port()) {
                            p.cookie = Some(cookie);
                            p.next_send_ms = now_ms;
                        }
                    }
                    Ok(Some(RelayReply::Allocated { port, .. })) if port == from.port() => {
                        if let Some(i) = self.pending.iter().position(|p| p.port == port) {
                            self.pending.remove(i);
                            self.confirmed += 1;
                        }
                    }
                    // The port-forward probe: from a relay port this socket never sent to.
                    Ok(Some(RelayReply::ProbeHello { nonce })) => {
                        if let Some(forward) = self.host_check.as_mut().and_then(|r| r.forward.as_mut()) {
                            forward.on_probe(&nonce);
                        }
                    }
                    _ => {}
                }
                continue;
            }
            match RelayReply::decode(&buf[..n]) {
                Ok(Some(RelayReply::ClientJoined { port, host_key, .. })) => {
                    self.on_client_joined(port, host_key, now_ms);
                }
                Ok(Some(RelayReply::Introduced(intro))) => {
                    if !self.hide_reach {
                        self.on_introduced(intro, now_ms);
                    }
                }
                Ok(Some(
                    reply @ (RelayReply::ProbeSent { .. }
                    | RelayReply::MeasureAck { .. }
                    | RelayReply::VersionTooNew { .. }
                    | RelayReply::VersionTooOld { .. }),
                )) => self.on_host_check_reply(&reply, now_ms),
                Ok(Some(RelayReply::RegisterChallenge(cookie))) => {
                    self.cookie = Some(cookie);
                    self.next_send_ms = now_ms;
                }
                // After the cookie-carrying 0 went out, a code answer comes from a relay built before
                // withdrawals, which clamps the 0 to one second: stop sending, and the lobby expires
                // within that second. Before it, the code answer belongs to an earlier renewal.
                Ok(Some(RelayReply::JoinCode(_))) if self.revoke.is_some() => {
                    if self.revoke == Some(Revoke::SentZero) {
                        self.revoke = Some(Revoke::Confirmed);
                        self.next_send_ms = u64::MAX;
                    }
                }
                Ok(Some(RelayReply::Error(RelayError::NotRegistered))) if self.revoke.is_some() => {
                    self.revoke = Some(Revoke::Confirmed);
                    self.next_send_ms = u64::MAX;
                }
                Ok(Some(RelayReply::JoinCode(code))) => {
                    learned |= self.code.as_deref() != Some(code.as_str());
                    self.code = Some(code);
                    self.last_refusal = None;
                    self.next_send_ms =
                        now_ms.saturating_add((u64::from(self.lifetime_s) * 500).min(JOIN_KEEPALIVE_MS));
                }
                Ok(Some(RelayReply::Error(e))) => {
                    self.last_refusal = Some(e);
                    // A refused renewal (the cookie went stale): start over with a fresh challenge.
                    self.cookie = None;
                    self.next_send_ms = now_ms.saturating_add(JOIN_RETRY_MS * 4);
                }
                _ => {}
            }
        }
        if now_ms >= self.next_send_ms {
            if self.traversal && !self.hide_reach && !self.gathered {
                let base = self.socket.local_addr().unwrap_or(self.relay);
                self.candidates = gather(&mut self.sources, base, now_ms);
                self.gathered = true;
            }
            let lifetime_s = if self.revoke.is_some() { 0 } else { self.lifetime_s };
            let datagram = if self.hide_reach {
                encode_register_hidden(lifetime_s, &self.token, self.cookie.as_ref())
            } else {
                encode_register_with_candidates(
                    lifetime_s,
                    &self.token,
                    self.cookie.as_ref(),
                    &self.candidates,
                )
            };
            if let Ok(bytes) = datagram {
                if let Err(e) = self.socket.send_to(&bytes, self.relay) {
                    tracing::debug!(error = %e, relay = %self.relay, "join code: register send error");
                }
                self.sent += 1;
                if self.revoke.is_some() && self.cookie.is_some() {
                    self.revoke = Some(Revoke::SentZero);
                }
            }
            // Retry until the relay answers; its join code moves the next send to the renewal.
            self.next_send_ms = now_ms.saturating_add(JOIN_RETRY_MS);
            // The renewal's cookie answer comes back as a challenge, then the same code.
            if self.code.is_some() {
                self.cookie = None;
            }
        }
        self.send_refreshes(now_ms);
        if self.traversal && !self.hide_reach {
            self.run_checks(now_ms);
        }
        if !self.hide_reach {
            self.run_host_check(now_ms);
        }
        learned
    }
}

/// The client's side: looks a join code up on the relay.
pub struct JoinResolver {
    socket: LinkSocket,
    relay: SocketAddr,
    lookup: Vec<u8>,
    next_send_ms: u64,
    attempts: u32,
    max_attempts: u32,
}

impl std::fmt::Debug for JoinResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinResolver")
            .field("relay", &self.relay)
            .field("attempts", &self.attempts)
            .finish_non_exhaustive()
    }
}

impl JoinResolver {
    /// A resolver of `code` on `socket`.
    ///
    /// # Errors
    ///
    /// [`JoinError::BadCode`] when `code` is not a join code.
    pub fn new(socket: LinkSocket, relay: SocketAddr, code: &str) -> Result<Self, JoinError> {
        let lookup = encode_lookup(code).ok_or_else(|| JoinError::BadCode(code.to_owned()))?;
        Ok(Self {
            socket,
            relay,
            lookup,
            next_send_ms: 0,
            attempts: 0,
            max_attempts: JOIN_LOOKUP_ATTEMPTS,
        })
    }

    /// Binds a UDP socket toward `relay` and makes a resolver.
    ///
    /// # Errors
    ///
    /// A bad code, or the socket cannot be bound.
    pub fn udp(relay: SocketAddr, code: &str) -> Result<Self, JoinError> {
        normalize_join_code(code).ok_or_else(|| JoinError::BadCode(code.to_owned()))?;
        let socket = UdpTransport::bind(registrar_bind(relay)).map_err(|e| JoinError::Bind(e.to_string()))?;
        Self::new(Box::new(socket), relay, code)
    }

    /// `Lookup` datagrams sent so far.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Reads the relay's answer, or sends a `Lookup` when one is due. `None` while waiting; the
    /// ticket (with [`JoinTicket::connect_addr`] applied) or why not once done.
    pub fn update(&mut self, now_ms: u64) -> Option<Result<JoinTicket, JoinError>> {
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        for _ in 0..64 {
            let (n, from) = match self.socket.recv_from(&mut buf) {
                Ok(Some(got)) => got,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "join code: relay receive error");
                    break;
                }
            };
            if from != self.relay {
                continue;
            }
            match RelayReply::decode(&buf[..n]) {
                Ok(Some(RelayReply::HostInfo { addr, token, .. })) => {
                    return Some(
                        JoinTicket::decode(&token)
                            .map(|t| JoinTicket {
                                addr: t.connect_addr(addr),
                                server_key: t.server_key,
                            })
                            .map_err(|e| JoinError::BadTicket(e.to_string())),
                    );
                }
                Ok(Some(RelayReply::Error(e))) => return Some(Err(JoinError::Refused(relay_error_name(e)))),
                _ => {}
            }
        }
        if now_ms >= self.next_send_ms {
            if self.attempts >= self.max_attempts {
                return Some(Err(JoinError::Timeout));
            }
            if let Err(e) = self.socket.send_to(&self.lookup, self.relay) {
                tracing::debug!(error = %e, relay = %self.relay, "join code: lookup send error");
            }
            self.attempts += 1;
            self.next_send_ms = now_ms.saturating_add(JOIN_RETRY_MS);
        }
        None
    }
}

/// Resolves `code` on `relay` over UDP on the wall clock (a client binary's start; tests use
/// [`JoinResolver`] on their own clock).
///
/// # Errors
///
/// See [`JoinError`].
pub fn resolve_join_code(relay: SocketAddr, code: &str) -> Result<JoinTicket, JoinError> {
    let mut resolver = JoinResolver::udp(relay, code)?;
    let start = std::time::Instant::now();
    loop {
        if let Some(done) = resolver.update(start.elapsed().as_millis() as u64) {
            return done;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Allocation ports the local relay opens (relay protocol 2, spec 120 § 10.6): one per relayed
/// client, more than any one host's default cap of 40.
pub const LOCAL_RELAY_ALLOCATION_PORTS: u16 = 64;

/// A relay inside the host process (`--relay local`): join codes on one machine with no relay to
/// run. It listens on `127.0.0.1` only, with [`LOCAL_RELAY_ALLOCATION_PORTS`] allocation ports.
pub struct LocalRelay {
    server: RelayServer<UdpTransport>,
    addr: SocketAddr,
}

impl std::fmt::Debug for LocalRelay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalRelay")
            .field("addr", &self.addr)
            .finish_non_exhaustive()
    }
}

impl LocalRelay {
    /// Binds `127.0.0.1:port` (0 picks a free port).
    ///
    /// # Errors
    ///
    /// The port cannot be bound.
    pub fn bind(port: u16, seed: u64) -> Result<Self, JoinError> {
        let config = RelayConfig {
            allocation_ports: LOCAL_RELAY_ALLOCATION_PORTS,
            ..RelayConfig::default()
        };
        let server = RelayServer::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port)), config, seed)
            .map_err(|e| JoinError::Bind(format!("local relay 127.0.0.1:{port}: {e}")))?;
        let addr = server
            .local_addr()
            .map_err(|e| JoinError::Bind(format!("local relay: {e}")))?;
        Ok(Self { server, addr })
    }

    /// What the relay holds and what it refused.
    pub fn status(&self) -> RelayStatus {
        self.server.status()
    }

    /// The relay as `net.relay_status` reports it.
    pub fn report(&self) -> crate::relay::RelayReport {
        self.server.report()
    }

    /// The bound address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Lobbies registered.
    pub fn lobby_count(&self) -> usize {
        self.server.core().lobby_count()
    }

    /// Handles every waiting datagram.
    pub fn update(&mut self, now_ms: u64) {
        self.server.update(now_ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::LoopbackNet;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 1], port))
    }

    #[test]
    fn a_ticket_round_trips_with_and_without_a_key() {
        for key in [None, Some(ServerKey([7; 32]))] {
            let t = JoinTicket {
                addr: addr(7777),
                server_key: key,
            };
            assert_eq!(JoinTicket::decode(&t.encode()), Ok(t));
        }
        let mut bad = JoinTicket {
            addr: addr(1),
            server_key: None,
        }
        .encode();
        bad[4] = 9;
        assert!(JoinTicket::decode(&bad).is_err(), "an unknown version is refused");
        assert!(JoinTicket::decode(b"RGJ").is_err());
        let mut long = JoinTicket {
            addr: addr(1),
            server_key: None,
        }
        .encode();
        long.push(0);
        assert!(JoinTicket::decode(&long).is_err(), "trailing bytes are refused");
    }

    #[test]
    fn an_unspecified_ticket_takes_the_ip_the_relay_saw() {
        let t = JoinTicket {
            addr: SocketAddr::from(([0, 0, 0, 0], 7777)),
            server_key: None,
        };
        let seen = SocketAddr::from(([127, 0, 0, 1], 40_000));
        assert_eq!(t.connect_addr(seen), SocketAddr::from(([127, 0, 0, 1], 7777)));
        let fixed = JoinTicket {
            addr: addr(7000),
            server_key: None,
        };
        assert_eq!(fixed.connect_addr(seen), addr(7000));
    }

    #[test]
    fn relay_specs_parse() {
        assert_eq!(
            RelaySpec::parse("local"),
            Ok(RelaySpec::Local(RELAY_DEFAULT_PORT))
        );
        assert_eq!(RelaySpec::parse("local:0"), Ok(RelaySpec::Local(0)));
        assert_eq!(
            RelaySpec::parse("10.0.0.1:7800"),
            Ok(RelaySpec::Remote(addr(7800)))
        );
        assert_eq!(
            RelaySpec::parse("local:x").map_err(|e| e.code()),
            Err("join_relay_invalid")
        );
        assert_eq!(
            RelaySpec::parse("no port here").map_err(|e| e.code()),
            Err("join_relay_invalid")
        );
    }

    /// Register (challenge, cookie, code), look the code up, get the ticket back: the whole
    /// rendezvous over the in-memory network on a counted clock.
    #[test]
    fn a_registered_ticket_resolves_by_its_code() {
        let net = LoopbackNet::new();
        let relay_addr = addr(7800);
        let mut relay = RelayServer::new(net.bind(relay_addr).expect("relay"), 5);
        let ticket = JoinTicket {
            addr: SocketAddr::from(([0, 0, 0, 0], 7777)),
            server_key: Some(ServerKey([3; 32])),
        };
        let mut host =
            RelayRegistrar::new(Box::new(net.bind(addr(5000)).expect("host")), relay_addr, &ticket)
                .expect("fits");
        let mut code = None;
        for step in 0..10u64 {
            let now = step * 16;
            if host.update(now) {
                code = host.join_code().map(str::to_owned);
            }
            relay.update(now);
        }
        let code = code.expect("a join code within ten steps");
        let mut resolver =
            JoinResolver::new(Box::new(net.bind(addr(6000)).expect("client")), relay_addr, &code)
                .expect("code");
        let mut got = None;
        for step in 10..20u64 {
            let now = step * 16;
            if let Some(done) = resolver.update(now) {
                got = Some(done);
                break;
            }
            relay.update(now);
        }
        let got = got.expect("answered").expect("resolved");
        assert_eq!(
            got.addr,
            SocketAddr::from(([10, 0, 0, 1], 7777)),
            "the relay's IP, the ticket's port"
        );
        assert_eq!(got.server_key, ticket.server_key);

        let mut unknown = JoinResolver::new(
            Box::new(net.bind(addr(6001)).expect("client")),
            relay_addr,
            "ZZZZZZ",
        )
        .expect("code");
        let mut refused = None;
        for step in 20..30u64 {
            if let Some(done) = unknown.update(step * 16) {
                refused = Some(done);
                break;
            }
            relay.update(step * 16);
        }
        assert_eq!(
            refused.expect("answered").map_err(|e| e.code()),
            Err("join_refused")
        );
    }

    #[test]
    fn a_resolver_gives_up_after_its_attempts() {
        let net = LoopbackNet::new();
        let mut r = JoinResolver::new(
            Box::new(net.bind(addr(6000)).expect("client")),
            addr(7800),
            "ABCDEF",
        )
        .expect("code");
        let mut now = 0;
        let end = loop {
            if let Some(done) = r.update(now) {
                break done;
            }
            now += JOIN_RETRY_MS;
        };
        assert_eq!(end, Err(JoinError::Timeout));
        assert_eq!(r.attempts(), JOIN_LOOKUP_ATTEMPTS);
        assert!(JoinResolver::udp(addr(7800), "nope").is_err());
    }

    #[test]
    fn join_error_codes_are_protocol_shaped() {
        let all = [
            JoinError::BadCode(String::new()),
            JoinError::BadRelay(String::new()),
            JoinError::Bind(String::new()),
            JoinError::Refused("full"),
            JoinError::BadTicket(String::new()),
            JoinError::Timeout,
            JoinError::RelayTooOld,
            JoinError::RelayFull(40),
            JoinError::RelayNoAllocation,
        ];
        for e in all {
            assert!(
                e.code()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "{}",
                e.code()
            );
        }
    }
}
