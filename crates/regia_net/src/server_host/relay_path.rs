// SPDX-License-Identifier: MPL-2.0
//! The relay on the game socket (spec 120 § 10.6 and § 10.7, ADR-8183): the host registers and
//! keeps its lobby alive from the socket the game uses, and a client that joins by code races a
//! direct path against a relay allocation on its own game socket.
//!
//! [`relay_split`] turns one socket into a game half and a relay-control half. Relay control
//! datagrams start with `RGNR` (`crate::relay::RELAY_MAGIC`) and every endpoint datagram starts
//! with `RGN1` (`crate::handshake::MAGIC`), so a datagram from the relay's IP that starts with
//! `RGNR` goes to the control half and everything else to the game half: the endpoint never sees
//! the relay's notices, and the NAT mapping the relay records is the one the game's datagrams
//! use.
//!
//! [`JoinPath`] is the client's side: `Lookup` (in both relay framings, so a protocol 1 relay is
//! recognized), then the direct address, and from [`RELAY_RACE_START_MS`] an allocation of relay
//! protocol 2 in parallel (`Allocate`, its cookie, then a `Refresh` on the allocation port that
//! confirms the path and opens the client's NAT toward it). If nothing came back from the direct
//! address by [`RELAY_FALLBACK_MS`] and the allocation is confirmed, the game's datagrams go to the
//! allocation port from then on; [`JoinSocket`] rewrites the addresses so the endpoint above
//! still talks to one server address. A protocol 1 relay cannot allocate: the path ends with
//! [`JoinError::RelayTooOld`] instead of waiting.
//!
//! On a relay of protocol 3 the join is ICE-shaped (spec 120 § 10.7, ADR-8340): the client's
//! first request is a `PunchIntro` that names the code and the client's candidates; the relay
//! introduces both sides (candidates, server-reflexive addresses, a session key) and both run
//! paced connectivity checks from their game sockets (`crate::nat::CheckAgent`). The best pair
//! whose check is answered carries the endpoint's handshake (its last datagram is sent there again
//! at once); the pair the sealed handshake completes on is nominated. The relay pair still starts
//! at 500 ms and wins at 1.5 s when nothing else completed. A protocol 2 relay answers the
//! introduction `VersionTooNew` and the join goes on with `Lookup` as before; a protocol 1 relay
//! drops it, and the second attempt's protocol 1 `Lookup` finds it.
//!
//! Everything is sans-clock: [`JoinPath::update`] takes the caller's milliseconds.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::join_code::normalize_join_code;
use crate::nat::{
    Candidate, CandidateKind, CandidateSource, CheckAgent, HostCandidates, Role, decode_check, gather,
    is_check,
};
use crate::relay::{
    RELAY_ALLOCATION_KEY_BYTES, RELAY_BIND_KEY_BYTES, RELAY_COOKIE_BYTES, RELAY_INTRO_MIN_VERSION,
    RelayReply, encode_allocate, encode_introduce, encode_lookup, encode_refresh, relay_version, versioned,
};
use crate::server_host::join::{
    JOIN_LOOKUP_ATTEMPTS, JOIN_RETRY_MS, JoinError, JoinTicket, relay_error_name,
};
use crate::socket::{DatagramSocket, MAX_DATAGRAM_BYTES};

/// Most datagrams one half of a [`relay_split`] socket holds before new ones are dropped.
pub const RELAY_SPLIT_QUEUE: usize = 4096;
/// Most datagrams one read pass of a split socket takes from the socket.
const READS_PER_PASS: usize = 512;
/// When a joining client starts its relay allocation in parallel with the direct attempt,
/// milliseconds after the direct attempt began (spec 120 § 10.7).
pub const RELAY_RACE_START_MS: u64 = 500;
/// When the relay path wins if nothing came back from the direct address, milliseconds after the
/// direct attempt began (spec 120 § 10.7: "relay fallback within 1.5 s").
pub const RELAY_FALLBACK_MS: u64 = 1_500;
/// `Allocate` and confirming `Refresh` attempts before a client gives up on the relay.
pub const RELAY_ALLOCATE_ATTEMPTS: u32 = 16;
/// How often a relayed client refreshes its permission (keeps its NAT mapping toward the
/// allocation port open, and moves the permission if the mapping changed), milliseconds.
pub const RELAY_REFRESH_INTERVAL_MS: u64 = 15_000;
/// How long a protocol 1 answer to `Lookup` waits for the protocol 2 answer before the client
/// takes the relay for a protocol 1 relay, milliseconds.
const VERSION_GRACE_MS: u64 = 2 * JOIN_RETRY_MS;
/// Introductions a client asks for again when its ticket came without one.
const INTRO_RESENDS: u32 = 3;
/// How long it waits between them, milliseconds.
const INTRO_RESEND_MS: u64 = 100;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Counters of a split socket.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RelaySplitStats {
    /// Relay control datagrams routed to the control half.
    pub control_in: u64,
    /// Datagrams routed to the game half.
    pub game_in: u64,
    /// Datagrams dropped because their half's queue was full.
    pub dropped: u64,
}

struct SplitInner<S> {
    socket: S,
    relay_ip: IpAddr,
    control: VecDeque<(Vec<u8>, SocketAddr)>,
    game: VecDeque<(Vec<u8>, SocketAddr)>,
    stats: RelaySplitStats,
}

impl<S: DatagramSocket> SplitInner<S> {
    /// Reads what is waiting and routes it. Errors end the pass (the next one tries again).
    fn pass(&mut self) -> io::Result<()> {
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        for _ in 0..READS_PER_PASS {
            let Some((n, from)) = self.socket.recv_from(&mut buf)? else {
                break;
            };
            let bytes = &buf[..n];
            // Relay control from the relay's IP, and connectivity checks (`RGNC`) from anyone: the
            // checks of NAT traversal leave and arrive on the game socket (spec 120 § 10.7).
            let control =
                (from.ip() == self.relay_ip && relay_version(bytes).is_some()) || crate::nat::is_check(bytes);
            let queue = if control {
                &mut self.control
            } else {
                &mut self.game
            };
            if queue.len() >= RELAY_SPLIT_QUEUE {
                self.stats.dropped += 1;
                continue;
            }
            queue.push_back((bytes.to_vec(), from));
            if control {
                self.stats.control_in += 1;
            } else {
                self.stats.game_in += 1;
            }
        }
        Ok(())
    }

    fn take(&mut self, control: bool, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        let queue = if control {
            &mut self.control
        } else {
            &mut self.game
        };
        if queue.is_empty() {
            self.pass()?;
        }
        let queue = if control {
            &mut self.control
        } else {
            &mut self.game
        };
        Ok(queue.pop_front().map(|(bytes, from)| {
            // Like UDP, a datagram longer than the buffer is truncated.
            let n = bytes.len().min(buf.len());
            buf[..n].copy_from_slice(&bytes[..n]);
            (n, from)
        }))
    }
}

/// The game half of a [`relay_split`] socket: every datagram except the relay's control traffic.
pub struct RelayGamePort<S> {
    inner: Arc<Mutex<SplitInner<S>>>,
}

/// The relay-control half of a [`relay_split`] socket: datagrams from the relay's IP that start
/// with the relay magic, and connectivity checks (`crate::nat::NAT_CHECK_MAGIC`) from any source.
/// It sends from the same socket as the game half.
pub struct RelayControlPort<S> {
    inner: Arc<Mutex<SplitInner<S>>>,
}

impl<S> std::fmt::Debug for RelayGamePort<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayGamePort").finish_non_exhaustive()
    }
}

impl<S> std::fmt::Debug for RelayControlPort<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayControlPort").finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> RelayControlPort<S> {
    /// The split socket's counters.
    pub fn stats(&self) -> RelaySplitStats {
        lock(&self.inner).stats
    }
}

impl<S: DatagramSocket> DatagramSocket for RelayGamePort<S> {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        lock(&self.inner).socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        lock(&self.inner).take(false, buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        lock(&self.inner).socket.local_addr()
    }
}

impl<S: DatagramSocket> DatagramSocket for RelayControlPort<S> {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        lock(&self.inner).socket.send_to(bytes, dest)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        lock(&self.inner).take(true, buf)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        lock(&self.inner).socket.local_addr()
    }
}

/// Splits `socket` into its game half and its relay-control half for the relay at `relay`
/// (control traffic from any port of the relay's IP: the control port and its allocation ports).
pub fn relay_split<S: DatagramSocket>(
    socket: S,
    relay: SocketAddr,
) -> (RelayGamePort<S>, RelayControlPort<S>) {
    let inner = Arc::new(Mutex::new(SplitInner {
        socket,
        relay_ip: relay.ip(),
        control: VecDeque::new(),
        game: VecDeque::new(),
        stats: RelaySplitStats::default(),
    }));
    (RelayGamePort { inner: inner.clone() }, RelayControlPort { inner })
}

/// Which path a [`JoinPath`] chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinRoute {
    /// Still looking the code up.
    Resolving,
    /// Datagrams go to the host's address; the relay path may still win.
    Direct,
    /// The direct address answered first: the relay is not used.
    DirectChosen,
    /// Datagrams go through the client's relay allocation on this port of the relay's IP.
    Relayed(u16),
    /// The path failed ([`JoinPath::error`] says why).
    Failed,
}

impl JoinRoute {
    /// Stable lowercase name (agent protocol, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolving => "resolving",
            Self::Direct => "direct",
            Self::DirectChosen => "direct_chosen",
            Self::Relayed(_) => "relayed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug)]
struct Route {
    /// The server address the endpoint talks to (the host's address from `Lookup`).
    server: Option<SocketAddr>,
    /// The allocation port's address once the relay path won.
    via: Option<SocketAddr>,
    /// The remote of the best pair whose connectivity check was answered (NAT traversal): the
    /// endpoint's datagrams go there until the relay wins.
    direct: Option<SocketAddr>,
    /// A datagram came from the direct address.
    heard_direct: bool,
    /// The address that datagram came from (the host's address or the checked pair's remote).
    heard_from: Option<SocketAddr>,
    /// The endpoint's last datagram to the server before a path was chosen, sent again at once on
    /// a newly checked pair (the handshake does not wait for its own retry).
    last_to_server: Option<Vec<u8>>,
}

/// The socket a joining client's endpoint uses: the game half of its split socket, with the
/// server address rewritten to the relay allocation once the relay path won.
pub struct JoinSocket<S> {
    game: RelayGamePort<S>,
    route: Arc<Mutex<Route>>,
}

impl<S> std::fmt::Debug for JoinSocket<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinSocket").finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> DatagramSocket for JoinSocket<S> {
    fn send_to(&mut self, bytes: &[u8], dest: SocketAddr) -> io::Result<()> {
        let to = {
            let mut route = lock(&self.route);
            match (route.server, route.via, route.direct) {
                (Some(server), Some(via), _) if dest == server => via,
                (Some(server), None, direct) if dest == server => {
                    if !route.heard_direct {
                        route.last_to_server = Some(bytes.to_vec());
                    }
                    direct.unwrap_or(server)
                }
                _ => dest,
            }
        };
        self.game.send_to(bytes, to)
    }

    fn recv_from(&mut self, buf: &mut [u8]) -> io::Result<Option<(usize, SocketAddr)>> {
        loop {
            let Some((n, from)) = self.game.recv_from(buf)? else {
                return Ok(None);
            };
            let mut route = lock(&self.route);
            let direct = route.direct;
            match (route.server, route.via) {
                (Some(server), Some(via)) if from == via => return Ok(Some((n, server))),
                // The relay path won: a late direct datagram would be a second path to the same
                // server, and the endpoint above knows one address.
                (Some(server), Some(_)) if from == server || Some(from) == direct => continue,
                (Some(server), None) if from == server || Some(from) == direct => {
                    route.heard_direct = true;
                    route.heard_from = Some(from);
                    route.last_to_server = None;
                    return Ok(Some((n, server)));
                }
                _ => return Ok(Some((n, from))),
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.game.local_addr()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alloc {
    /// Not started.
    Idle,
    /// `Allocate` sent (with the cookie once the relay gave one).
    Asking {
        cookie: Option<[u8; RELAY_COOKIE_BYTES]>,
    },
    /// `Allocated` on `port`; confirming with `Refresh` from the game socket.
    Confirming {
        port: u16,
        cookie: Option<[u8; RELAY_COOKIE_BYTES]>,
    },
    /// The allocation port answered the client's `Refresh`: the path works.
    Ready { port: u16 },
}

/// A client's join by code (spec 120 § 10.7): `Lookup`, direct attempt, relay allocation, choice.
pub struct JoinPath<S> {
    control: RelayControlPort<S>,
    game: Option<RelayGamePort<S>>,
    route: Arc<Mutex<Route>>,
    relay: SocketAddr,
    lookup: Vec<u8>,
    client_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
    phase: JoinRoute,
    lookups: u32,
    next_lookup_ms: u64,
    /// A protocol 1 `HostInfo` and when it came (waits [`VERSION_GRACE_MS`] for the protocol 2 one).
    v1_answer: Option<(JoinTicket, SocketAddr, [u8; RELAY_BIND_KEY_BYTES], u64)>,
    relay_protocol: Option<u8>,
    ticket: Option<JoinTicket>,
    host: Option<SocketAddr>,
    bind_key: [u8; RELAY_BIND_KEY_BYTES],
    direct_since_ms: u64,
    alloc: Alloc,
    alloc_attempts: u32,
    next_alloc_ms: u64,
    next_refresh_ms: u64,
    error: Option<JoinError>,
    full_cap: Option<u16>,
    /// Candidate sources of the game socket (its host addresses; a port mapping later).
    sources: Vec<Box<dyn CandidateSource>>,
    /// This side's candidates, gathered before the first request.
    local: Vec<Candidate>,
    /// The protocol 3 introduction request (`None` before gathering, or when the relay is older).
    intro: Option<Vec<u8>>,
    /// The relay answered the introduction `VersionTooNew`: `Lookup` only.
    intro_refused: bool,
    /// The connectivity checks, once the relay introduced both sides.
    agent: Option<CheckAgent>,
    /// This side as the relay saw it.
    observed: Option<SocketAddr>,
    /// The nominating check went out.
    nominate_sent: bool,
    /// Introductions sent again after the ticket came without one (its answer was lost).
    intro_resends: u32,
    next_intro_ms: u64,
    /// The join code, normalized.
    code: String,
    /// When the relay allocation starts, after the direct attempt began ([`RELAY_RACE_START_MS`]).
    race_start_ms: u64,
    /// When the relay path wins ([`RELAY_FALLBACK_MS`]).
    fallback_ms: u64,
}

impl<S> std::fmt::Debug for JoinPath<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinPath")
            .field("relay", &self.relay)
            .field("phase", &self.phase)
            .field("relay_protocol", &self.relay_protocol)
            .finish_non_exhaustive()
    }
}

impl<S: DatagramSocket> JoinPath<S> {
    /// A join of `code` through `relay` on `socket` (the socket the game will use).
    /// `client_key` is this client's secret allocation key: random, never shared.
    ///
    /// # Errors
    ///
    /// [`JoinError::BadCode`] when `code` is not a join code.
    pub fn new(
        socket: S,
        relay: SocketAddr,
        code: &str,
        client_key: [u8; RELAY_ALLOCATION_KEY_BYTES],
    ) -> Result<Self, JoinError> {
        let code = normalize_join_code(code).ok_or_else(|| JoinError::BadCode(code.to_owned()))?;
        let lookup = encode_lookup(&code).ok_or_else(|| JoinError::BadCode(code.clone()))?;
        let (game, control) = relay_split(socket, relay);
        Ok(Self {
            control,
            game: Some(game),
            route: Arc::new(Mutex::new(Route {
                server: None,
                via: None,
                direct: None,
                heard_direct: false,
                heard_from: None,
                last_to_server: None,
            })),
            relay,
            lookup,
            client_key,
            phase: JoinRoute::Resolving,
            lookups: 0,
            next_lookup_ms: 0,
            v1_answer: None,
            relay_protocol: None,
            ticket: None,
            host: None,
            bind_key: [0; RELAY_BIND_KEY_BYTES],
            direct_since_ms: 0,
            alloc: Alloc::Idle,
            alloc_attempts: 0,
            next_alloc_ms: 0,
            next_refresh_ms: 0,
            error: None,
            full_cap: None,
            sources: vec![Box::new(HostCandidates { toward: relay })],
            local: Vec::new(),
            intro: None,
            intro_refused: false,
            agent: None,
            observed: None,
            nominate_sent: false,
            intro_resends: 0,
            next_intro_ms: 0,
            code,
            race_start_ms: RELAY_RACE_START_MS,
            fallback_ms: RELAY_FALLBACK_MS,
        })
    }

    /// The relay at once (a broker that knows no direct pair can work, `lobby::hub::join`): no
    /// introduction and no connectivity checks, the allocation starts with the direct attempt and
    /// wins as soon as it is confirmed.
    pub fn relay_first(mut self) -> Self {
        self.intro_refused = true;
        self.race_start_ms = 0;
        self.fallback_ms = 0;
        self
    }

    /// Adds a source of candidates before the first [`Self::update`] (a port mapping).
    pub fn add_candidate_source(&mut self, source: Box<dyn CandidateSource>) {
        self.sources.push(source);
    }

    /// This side's candidates (gathered at the first [`Self::update`]).
    pub fn candidates(&self) -> &[Candidate] {
        &self.local
    }

    /// The connectivity checks, once the relay introduced both sides (relay protocol 3).
    pub fn checks(&self) -> Option<&CheckAgent> {
        self.agent.as_ref()
    }

    /// This side's address as the relay saw it (its server-reflexive candidate).
    pub fn observed(&self) -> Option<SocketAddr> {
        self.observed
    }

    /// The direct pair the endpoint's datagrams go to: the remote of the best checked pair, or
    /// the host's address before any check was answered (`None` before the ticket and once the
    /// relay won).
    pub fn direct_remote(&self) -> Option<SocketAddr> {
        let route = lock(&self.route);
        if route.via.is_some() {
            return None;
        }
        route.heard_from.or(route.direct).or(route.server)
    }

    /// The path so far.
    pub fn route(&self) -> JoinRoute {
        self.phase
    }

    /// The ticket, once the relay answered `Lookup`.
    pub fn ticket(&self) -> Option<&JoinTicket> {
        self.ticket.as_ref()
    }

    /// The relay's protocol as its answers showed it (`None` before the first answer).
    pub fn relay_protocol(&self) -> Option<u8> {
        self.relay_protocol
    }

    /// Why the path failed.
    pub fn error(&self) -> Option<&JoinError> {
        self.error.as_ref()
    }

    /// The relay's address.
    pub fn relay(&self) -> SocketAddr {
        self.relay
    }

    /// The socket for the endpoint, once (after the ticket arrived): connect it to the ticket's
    /// address. `None` before the ticket or when already taken.
    pub fn take_socket(&mut self) -> Option<JoinSocket<S>> {
        self.ticket.as_ref()?;
        let game = self.game.take()?;
        Some(JoinSocket {
            game,
            route: self.route.clone(),
        })
    }

    fn fail(&mut self, error: JoinError) {
        tracing::info!(target: "regia_net", relay = %self.relay, code = error.code(), "join path failed: {error}");
        self.error = Some(error);
        self.phase = JoinRoute::Failed;
    }

    fn send(&mut self, bytes: &[u8], to: SocketAddr) {
        if let Err(e) = self.control.send_to(bytes, to) {
            tracing::debug!(error = %e, %to, "join path: relay send error");
        }
    }

    fn accept_ticket(
        &mut self,
        ticket: JoinTicket,
        host: SocketAddr,
        key: [u8; RELAY_BIND_KEY_BYTES],
        protocol: u8,
        now_ms: u64,
    ) {
        lock(&self.route).server = Some(ticket.addr);
        self.ticket = Some(ticket);
        self.host = Some(host);
        self.bind_key = key;
        self.relay_protocol = Some(protocol);
        self.phase = JoinRoute::Direct;
        self.direct_since_ms = now_ms;
    }

    /// Reads the relay's answers and sends what is due. Call it every frame before the
    /// endpoint's own receive. Returns the route.
    pub fn update(&mut self, now_ms: u64) -> JoinRoute {
        let mut buf = [0u8; MAX_DATAGRAM_BYTES];
        for _ in 0..64 {
            let (n, from) = match self.control.recv_from(&mut buf) {
                Ok(Some(got)) => got,
                Ok(None) => break,
                Err(e) => {
                    tracing::debug!(error = %e, "join path: receive error");
                    break;
                }
            };
            if is_check(&buf[..n]) {
                self.on_check(n, &buf, from, now_ms);
                continue;
            }
            let version = relay_version(&buf[..n]).unwrap_or(1);
            let Ok(Some(reply)) = RelayReply::decode(&buf[..n]) else {
                continue;
            };
            self.on_reply(reply, from, version, now_ms);
        }
        match self.phase {
            JoinRoute::Resolving => self.resolve_step(now_ms),
            JoinRoute::Direct => {
                self.check_step(now_ms);
                if self.phase == JoinRoute::Direct {
                    self.race_step(now_ms);
                }
            }
            JoinRoute::DirectChosen => self.nominate_step(now_ms),
            JoinRoute::Relayed(port) => {
                if now_ms >= self.next_refresh_ms {
                    let to = SocketAddr::new(self.relay.ip(), port);
                    self.send(&encode_refresh(&self.client_key, None), to);
                    self.next_refresh_ms = now_ms.saturating_add(RELAY_REFRESH_INTERVAL_MS);
                }
            }
            JoinRoute::Failed => {}
        }
        self.phase
    }

    fn on_check(&mut self, n: usize, buf: &[u8], from: SocketAddr, now_ms: u64) {
        let (Some(agent), Some(check)) = (self.agent.as_mut(), decode_check(&buf[..n])) else {
            return;
        };
        if matches!(self.phase, JoinRoute::Relayed(_) | JoinRoute::Failed) {
            return;
        }
        if let Some((bytes, to)) = agent.on_check(&check, from, now_ms)
            && let Err(e) = self.control.send_to(&bytes, to)
        {
            tracing::debug!(error = %e, %to, "join path: check answer send error");
        }
    }

    /// The paced checks, and the pair the endpoint uses: the best one whose check was answered.
    /// A ticket that came without an introduction (its answer was lost, `Lookup`'s was not) asks
    /// for the introduction again, a few times, until the relay race starts.
    fn check_step(&mut self, now_ms: u64) {
        if self.agent.is_none()
            && !self.intro_refused
            && self.intro_resends < INTRO_RESENDS
            && now_ms >= self.next_intro_ms
            && now_ms.saturating_sub(self.direct_since_ms) < self.race_start_ms
            && let Some(intro) = self.intro.clone()
        {
            self.send(&intro, self.relay);
            self.intro_resends += 1;
            self.next_intro_ms = now_ms.saturating_add(INTRO_RESEND_MS);
        }
        let Some(agent) = self.agent.as_mut() else {
            return;
        };
        if let Some((bytes, to)) = agent.poll(now_ms)
            && let Err(e) = self.control.send_to(&bytes, to)
        {
            tracing::debug!(error = %e, %to, "join path: check send error");
        }
        let Some(best) = agent.best_valid().map(|p| p.remote.addr) else {
            return;
        };
        let replay = {
            let mut route = lock(&self.route);
            if route.heard_direct || route.via.is_some() || route.direct == Some(best) {
                None
            } else {
                route.direct = Some(best);
                route.last_to_server.clone()
            }
        };
        tracing::debug!(target: "regia_net", %best, "join path: a checked pair carries the handshake");
        if let Some(bytes) = replay
            && let Err(e) = self.control.send_to(&bytes, best)
        {
            tracing::debug!(error = %e, to = %best, "join path: handshake replay send error");
        }
    }

    /// After the handshake completed on a direct pair: nominate it, one check that tells the host.
    fn nominate_step(&mut self, now_ms: u64) {
        if self.nominate_sent {
            return;
        }
        let heard = lock(&self.route).heard_from;
        let (Some(agent), Some(remote)) = (self.agent.as_mut(), heard) else {
            self.nominate_sent = true;
            return;
        };
        agent.nominate(remote, now_ms);
        if agent.pairs().iter().all(|p| p.remote.addr != remote) {
            self.nominate_sent = true;
            return;
        }
        if let Some((bytes, to)) = agent.poll(now_ms) {
            if to == remote {
                self.nominate_sent = true;
            }
            if let Err(e) = self.control.send_to(&bytes, to) {
                tracing::debug!(error = %e, %to, "join path: nominating check send error");
            }
        }
    }

    /// The relay introduced both sides (protocol 3): start the checks.
    fn start_checks(&mut self, intro: &crate::relay::Introduction, now_ms: u64) {
        if self.agent.is_some() {
            return;
        }
        let mut remote = intro.candidates.clone();
        remote.push(Candidate::new(CandidateKind::ServerReflexive, intro.peer));
        self.observed = Some(intro.observed);
        self.agent = Some(CheckAgent::new(
            Role::Controlling,
            intro.session,
            &self.local,
            &remote,
            now_ms,
        ));
    }

    fn on_reply(&mut self, reply: RelayReply, from: SocketAddr, version: u8, now_ms: u64) {
        let from_control = from == self.relay;
        match reply {
            RelayReply::HostInfo {
                addr,
                token,
                bind_key,
            } if from_control => {
                if self.phase != JoinRoute::Resolving {
                    return;
                }
                let ticket = match JoinTicket::decode(&token) {
                    Ok(t) => JoinTicket {
                        addr: t.connect_addr(addr),
                        server_key: t.server_key,
                    },
                    Err(e) => return self.fail(JoinError::BadTicket(e.to_string())),
                };
                if version >= 2 {
                    self.accept_ticket(ticket, addr, bind_key, version, now_ms);
                } else if self.v1_answer.is_none() {
                    self.v1_answer = Some((ticket, addr, bind_key, now_ms));
                }
            }
            RelayReply::Introduced(intro) if from_control && version >= RELAY_INTRO_MIN_VERSION => {
                if self.phase == JoinRoute::Resolving {
                    let ticket = match JoinTicket::decode(&intro.token) {
                        Ok(t) => JoinTicket {
                            addr: t.connect_addr(intro.peer),
                            server_key: t.server_key,
                        },
                        Err(e) => return self.fail(JoinError::BadTicket(e.to_string())),
                    };
                    self.accept_ticket(ticket, intro.peer, intro.bind_key, version, now_ms);
                }
                if self.phase == JoinRoute::Direct {
                    self.start_checks(&intro, now_ms);
                }
            }
            RelayReply::VersionTooNew { .. } if from_control && self.phase == JoinRoute::Direct => {
                self.intro_refused = true;
            }
            RelayReply::VersionTooNew { .. } if from_control && self.phase == JoinRoute::Resolving => {
                // A protocol 2 relay: no introductions, `Lookup` at once.
                if !self.intro_refused {
                    self.intro_refused = true;
                    self.next_lookup_ms = now_ms;
                }
            }
            RelayReply::Error(e) if from_control && self.phase == JoinRoute::Resolving => {
                self.fail(JoinError::Refused(relay_error_name(e)));
            }
            RelayReply::Error(e) if from_control && matches!(self.alloc, Alloc::Asking { .. }) => {
                if !lock(&self.route).heard_direct {
                    self.fail(JoinError::Refused(relay_error_name(e)));
                }
            }
            RelayReply::Challenge(cookie) => match self.alloc {
                Alloc::Asking { .. } if from_control => {
                    self.alloc = Alloc::Asking { cookie: Some(cookie) };
                    self.next_alloc_ms = now_ms;
                }
                Alloc::Confirming { port, .. } if from == SocketAddr::new(self.relay.ip(), port) => {
                    self.alloc = Alloc::Confirming {
                        port,
                        cookie: Some(cookie),
                    };
                    self.next_alloc_ms = now_ms;
                }
                _ => {}
            },
            RelayReply::Allocated { port, .. } => match self.alloc {
                Alloc::Asking { .. } if from_control => {
                    self.alloc = Alloc::Confirming { port, cookie: None };
                    self.alloc_attempts = 0;
                    self.next_alloc_ms = now_ms;
                }
                Alloc::Confirming { port: p, .. }
                    if p == port && from == SocketAddr::new(self.relay.ip(), port) =>
                {
                    self.alloc = Alloc::Ready { port };
                }
                _ => {}
            },
            RelayReply::Full { cap } if from_control => {
                if matches!(self.alloc, Alloc::Asking { .. }) {
                    self.full_cap = Some(cap);
                }
            }
            RelayReply::VersionTooOld { .. } if from_control && self.phase == JoinRoute::Direct => {
                self.relay_protocol = Some(1);
            }
            _ => {}
        }
    }

    fn resolve_step(&mut self, now_ms: u64) {
        if let Some((_, _, _, at)) = &self.v1_answer
            && now_ms >= at.saturating_add(VERSION_GRACE_MS)
            && let Some((ticket, host, key, _)) = self.v1_answer.take()
        {
            self.accept_ticket(ticket, host, key, 1, now_ms);
            return;
        }
        if now_ms < self.next_lookup_ms {
            return;
        }
        if self.lookups >= JOIN_LOOKUP_ATTEMPTS {
            if let Some((ticket, host, key, _)) = self.v1_answer.take() {
                self.accept_ticket(ticket, host, key, 1, now_ms);
            } else {
                self.fail(JoinError::Timeout);
            }
            return;
        }
        if self.lookups == 0 && self.intro.is_none() {
            let base = self.control.local_addr().unwrap_or(self.relay);
            self.local = gather(&mut self.sources, base, now_ms);
            self.intro = encode_introduce(&self.code, &self.local);
        }
        // The protocol 3 introduction first; from the second attempt (or at once after a
        // `VersionTooNew`) the protocol 2 `Lookup` too, and the protocol 1 one, so an older relay
        // (which answers `VersionTooNew`, or drops the versioned framing) still answers.
        let intro = if self.intro_refused {
            None
        } else {
            self.intro.clone()
        };
        if let Some(intro) = &intro {
            self.send(intro, self.relay);
        }
        if intro.is_none() || self.lookups > 0 {
            let v2 = versioned(&self.lookup);
            self.send(&v2, self.relay);
        }
        if self.lookups > 0 && self.v1_answer.is_none() {
            let v1 = self.lookup.clone();
            self.send(&v1, self.relay);
        }
        self.lookups += 1;
        self.next_lookup_ms = now_ms.saturating_add(JOIN_RETRY_MS);
    }

    fn race_step(&mut self, now_ms: u64) {
        let heard_direct = lock(&self.route).heard_direct;
        if heard_direct {
            self.phase = JoinRoute::DirectChosen;
            self.nominate_step(now_ms);
            return;
        }
        let elapsed = now_ms.saturating_sub(self.direct_since_ms);
        if let Alloc::Ready { port } = self.alloc
            && elapsed >= self.fallback_ms
        {
            let via = SocketAddr::new(self.relay.ip(), port);
            lock(&self.route).via = Some(via);
            tracing::info!(target: "regia_net", relay = %self.relay, %via, "join path: the relay path won");
            self.phase = JoinRoute::Relayed(port);
            self.next_refresh_ms = now_ms.saturating_add(RELAY_REFRESH_INTERVAL_MS);
            return;
        }
        if elapsed >= self.fallback_ms.max(RELAY_RACE_START_MS) {
            if self.relay_protocol.is_some_and(|p| p < 2) {
                return self.fail(JoinError::RelayTooOld);
            }
            if let Some(cap) = self.full_cap {
                return self.fail(JoinError::RelayFull(cap));
            }
        }
        if elapsed < self.race_start_ms || matches!(self.alloc, Alloc::Ready { .. }) {
            return;
        }
        if self.relay_protocol.is_some_and(|p| p < 2) || self.full_cap.is_some() {
            return;
        }
        if self.alloc == Alloc::Idle {
            self.alloc = Alloc::Asking { cookie: None };
            self.next_alloc_ms = now_ms;
        }
        if now_ms < self.next_alloc_ms {
            return;
        }
        if self.alloc_attempts >= RELAY_ALLOCATE_ATTEMPTS {
            return self.fail(JoinError::RelayNoAllocation);
        }
        let (Some(host), alloc) = (self.host, self.alloc) else {
            return;
        };
        match alloc {
            Alloc::Asking { cookie } => {
                let bytes = encode_allocate(host, &self.bind_key, &self.client_key, cookie.as_ref());
                self.send(&bytes, self.relay);
            }
            Alloc::Confirming { port, cookie } => {
                let bytes = encode_refresh(&self.client_key, cookie.as_ref());
                self.send(&bytes, SocketAddr::new(self.relay.ip(), port));
            }
            Alloc::Idle | Alloc::Ready { .. } => {}
        }
        self.alloc_attempts += 1;
        self.next_alloc_ms = now_ms.saturating_add(JOIN_RETRY_MS);
    }
}

/// A fresh random allocation key (`relay::RELAY_ALLOCATION_KEY_BYTES` bytes).
pub fn fresh_allocation_key() -> [u8; RELAY_ALLOCATION_KEY_BYTES] {
    let secret = crate::handshake::fresh_secret();
    let mut key = [0u8; RELAY_ALLOCATION_KEY_BYTES];
    key.copy_from_slice(&secret[..RELAY_ALLOCATION_KEY_BYTES]);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{RelayServer, encode_register};
    use crate::socket::LoopbackNet;

    fn addr(a: [u8; 4], port: u16) -> SocketAddr {
        SocketAddr::from((a, port))
    }

    /// Relay control from the relay's IP (any port) goes to the control half; game datagrams,
    /// and relay-looking bytes from anyone else, go to the game half; each half's sends leave from
    /// the one socket.
    #[test]
    fn a_split_socket_routes_relay_control_by_magic_and_source() {
        let net = LoopbackNet::new();
        let relay = addr([192, 0, 2, 1], 7800);
        let (mut game, mut control) = relay_split(net.bind(addr([10, 0, 0, 2], 7777)).expect("bind"), relay);
        let mut relay_main = net.bind(relay).expect("relay");
        let mut relay_port = net.bind(addr([192, 0, 2, 1], 50_000)).expect("port");
        let mut peer = net.bind(addr([10, 0, 0, 3], 5000)).expect("peer");
        let me = addr([10, 0, 0, 2], 7777);
        let register = encode_register(60, b"t").expect("fits");
        relay_main.send_to(&register, me).expect("send");
        relay_port.send_to(&versioned(&register), me).expect("send");
        relay_port
            .send_to(b"RGN1 game bytes through the relay", me)
            .expect("send");
        peer.send_to(&register, me).expect("send");
        let mut buf = [0u8; 256];
        let mut control_from = Vec::new();
        while let Some((_, from)) = control.recv_from(&mut buf).expect("io") {
            control_from.push(from);
        }
        let mut game_from = Vec::new();
        while let Some((n, from)) = game.recv_from(&mut buf).expect("io") {
            game_from.push((from, buf[..4].to_vec(), n));
        }
        assert_eq!(control_from, vec![relay, addr([192, 0, 2, 1], 50_000)]);
        assert_eq!(game_from.len(), 2, "{game_from:?}");
        assert_eq!(game_from[0].0, addr([192, 0, 2, 1], 50_000), "relayed game bytes");
        assert_eq!(game_from[0].1, b"RGN1".to_vec());
        assert_eq!(
            game_from[1].0,
            addr([10, 0, 0, 3], 5000),
            "relay magic from a peer is a game datagram"
        );
        let stats = control.stats();
        assert_eq!((stats.control_in, stats.game_in, stats.dropped), (2, 2, 0));
        control.send_to(b"x", relay).expect("send");
        game.send_to(b"y", relay).expect("send");
        let mut got = Vec::new();
        while let Some((n, from)) = relay_main.recv_from(&mut buf).expect("io") {
            got.push((buf[..n].to_vec(), from));
        }
        assert_eq!(
            got,
            vec![(b"x".to_vec(), me), (b"y".to_vec(), me)],
            "one source address for both halves"
        );
    }

    /// Lookup that no relay answers ends with `join_timeout` after every attempt; a bad code is
    /// refused at once; an unknown code is `join_refused`.
    #[test]
    fn a_join_path_times_out_and_refuses_with_codes() {
        let net = LoopbackNet::new();
        let relay = addr([192, 0, 2, 1], 7800);
        assert_eq!(
            JoinPath::new(
                net.bind(addr([10, 0, 0, 5], 1)).expect("bind"),
                relay,
                "no",
                [0; 16]
            )
            .map(|_| ())
            .map_err(|e| e.code()),
            Err("join_code_invalid")
        );
        let mut path = JoinPath::new(
            net.bind(addr([10, 0, 0, 5], 2)).expect("bind"),
            relay,
            "ABCDEF",
            [0; 16],
        )
        .expect("code");
        let mut now = 0;
        while path.update(now) == JoinRoute::Resolving {
            now += JOIN_RETRY_MS;
            assert!(now < 60_000, "bounded");
        }
        assert_eq!(path.error().map(JoinError::code), Some("join_timeout"));
        assert!(path.take_socket().is_none(), "no ticket, no socket");

        let mut server = RelayServer::new(net.bind(relay).expect("relay"), 1);
        let mut unknown = JoinPath::new(
            net.bind(addr([10, 0, 0, 5], 3)).expect("bind"),
            relay,
            "ZZZZZZ",
            [0; 16],
        )
        .expect("code");
        let mut now = 0;
        while unknown.update(now) == JoinRoute::Resolving && now < 10_000 {
            server.update(now);
            now += 20;
        }
        assert_eq!(unknown.error().map(JoinError::code), Some("join_refused"));
        assert_eq!(unknown.route().as_str(), "failed");
    }
}
