// SPDX-License-Identifier: MPL-2.0
//! NAT traversal from the game socket (spec 120 § 10.7, ADR-8340): candidates, paced
//! connectivity checks and nomination, in the shape of ICE (RFC 8445).
//!
//! Each side gathers **candidates**, addresses the other side might reach it at: host candidates
//! (the game socket's own addresses), server-reflexive ones (the address the relay saw, the STUN
//! role of RFC 8489), mapped ones (a port mapping, [`CandidateSource`]) and peer-reflexive ones
//! learned from a check that came from an address nobody announced (RFC 8445 § 7.3.1.3). The
//! relay's introduction (`relay::RelayReply::Introduced`) hands each side the other's list and a
//! session key.
//!
//! A [`CheckAgent`] then sends **connectivity checks** (`RGNC` datagrams, [`encode_check`]) from
//! the game socket to every remote candidate, highest pair priority first (RFC 8445 § 6.1.2.3),
//! paced at one check every [`NAT_CHECK_PACE_MS`] per session (Ta, RFC 8445 § 14.2), with
//! [`CheckPacer`] keeping every session of one process at least [`NAT_CHECK_FLOOR_MS`] apart. A
//! check from either side opens that side's NAT toward the other (the simultaneous-open technique
//! of Ford, Srisuresh and Kegel, USENIX 2005); an answered check makes its pair valid. The
//! controlling side (the joining client) points the game's sealed handshake at the best valid
//! pair; the pair whose handshake completes is nominated. The relay pair runs in parallel from
//! 500 ms and wins at 1.5 s when nothing else completed (`server_host::JoinPath`).
//!
//! Every check carries the session key the relay gave both sides and nothing else secret: a
//! check only proves that the sender read the relay's introduction. What authenticates the path
//! is the sealed handshake that follows (the server key pinned by the join ticket), so a forged
//! check can at worst make one pair look valid, and that pair then fails its handshake.
//!
//! Anti-amplification: a check request is padded to [`NAT_CHECK_REQUEST_BYTES`], which is more
//! than its answer; a session sends at most [`NAT_CHECK_ATTEMPTS`] checks per pair and
//! [`NAT_MAX_REMOTE_CANDIDATES`] pairs, so a hostile candidate list costs a bounded, paced trickle.
//!
//! Sans-IO and sans-clock: the agent takes the caller's milliseconds and returns datagrams.

use std::net::{IpAddr, SocketAddr};

use crate::handshake::{decode_addr, encode_addr};
use crate::wire::{WireError, WireReader, WireWriter, constant_time_eq};

/// Prefix of every connectivity check (`RGNC`). Endpoint datagrams start `RGN1` and relay control
/// `RGNR`, so the three never collide.
pub const NAT_CHECK_MAGIC: [u8; 4] = *b"RGNC";
/// Ta, the pacing interval of one session's checks, milliseconds (RFC 8445 § 14.2 default).
pub const NAT_CHECK_PACE_MS: u64 = 50;
/// Least time between two checks of any sessions of one process, milliseconds (RFC 8445 § 14.2:
/// all agents of an implementation together no more often than once every 5 ms).
pub const NAT_CHECK_FLOOR_MS: u64 = 5;
/// Least retransmission interval of one pair's check, milliseconds (RFC 8445 § 14.3: RTO is at
/// least 500 ms).
pub const NAT_CHECK_RTO_MS: u64 = 500;
/// Checks one pair sends before it fails.
pub const NAT_CHECK_ATTEMPTS: u32 = 4;
/// Candidates one side announces at most (and a relay keeps for a host).
pub const NAT_MAX_CANDIDATES: usize = 8;
/// Remote candidates one session checks at most (announced ones plus peer-reflexive ones).
pub const NAT_MAX_REMOTE_CANDIDATES: usize = 12;
/// Bytes of a session key (the relay's introduction hands it to both sides).
pub const NAT_SESSION_KEY_BYTES: usize = 16;
/// Least bytes of a check request: its answer (the request's fields plus the mapped address) is
/// smaller, so a forged source never gets more back than it sent.
pub const NAT_CHECK_REQUEST_BYTES: usize = 64;
/// A session of a host that never nominated a pair ends after this long, milliseconds.
pub const NAT_SESSION_LIFETIME_MS: u64 = 10_000;
/// Sessions a host runs at once; the oldest ends when another starts.
pub const NAT_MAX_SESSIONS: usize = 64;

/// The kind of a candidate (RFC 8445 § 5.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum CandidateKind {
    /// An address of the game socket itself.
    Host = 1,
    /// A port mapping of the game socket (PCP, NAT-PMP or UPnP IGD; spec 120 § 10.7).
    Mapped = 2,
    /// The address the relay saw the game socket at.
    ServerReflexive = 3,
    /// An address a check came from that nobody announced.
    PeerReflexive = 4,
    /// A relay allocation.
    Relayed = 5,
}

impl CandidateKind {
    fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Host,
            2 => Self::Mapped,
            3 => Self::ServerReflexive,
            4 => Self::PeerReflexive,
            5 => Self::Relayed,
            _ => return None,
        })
    }

    /// Type preference (RFC 8445 § 5.1.2.2: host 126, peer-reflexive 110, server-reflexive 100,
    /// relayed 0). A mapped candidate is a server-reflexive one the NAT promised to keep: 105.
    pub fn type_preference(self) -> u32 {
        match self {
            Self::Host => 126,
            Self::PeerReflexive => 110,
            Self::Mapped => 105,
            Self::ServerReflexive => 100,
            Self::Relayed => 0,
        }
    }

    /// Stable lowercase name (agent protocol, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Mapped => "mapped",
            Self::ServerReflexive => "srflx",
            Self::PeerReflexive => "prflx",
            Self::Relayed => "relay",
        }
    }
}

/// One candidate: a kind, an address and its priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Candidate {
    /// What kind of address this is.
    pub kind: CandidateKind,
    /// The address.
    pub addr: SocketAddr,
    /// The priority (RFC 8445 § 5.1.2.1).
    pub priority: u32,
}

impl Candidate {
    /// A candidate with the priority RFC 8445 § 5.1.2.1 gives it: `2^24 × type preference +
    /// 2^8 × local preference + (256 − component)`, one component. The local preference ranks
    /// candidates of one kind (IPv6 above IPv4, as RFC 8421 suggests).
    pub fn new(kind: CandidateKind, addr: SocketAddr) -> Self {
        let local: u32 = if addr.is_ipv6() { 65_535 } else { 65_534 };
        Self {
            kind,
            addr,
            priority: (kind.type_preference() << 24) + (local << 8) + 255,
        }
    }
}

/// Pair priority (RFC 8445 § 6.1.2.3): `2^32 × min(G, D) + 2 × max(G, D) + (G > D ? 1 : 0)`, `G`
/// the controlling side's candidate priority and `D` the controlled side's.
pub fn pair_priority(controlling: u32, controlled: u32) -> u64 {
    let (g, d) = (u64::from(controlling), u64::from(controlled));
    (g.min(d) << 32) + 2 * g.max(d) + u64::from(g > d)
}

/// A source of candidates for one game socket: the host addresses of the socket, and from
/// net-nat-hosting-flow a port mapping (PCP, NAT-PMP, UPnP IGD raced, spec 120 § 10.7).
pub trait CandidateSource: Send {
    /// The candidates this source has for the game socket bound at `base`, now.
    fn candidates(&mut self, base: SocketAddr, now_ms: u64) -> Vec<Candidate>;
}

/// The game socket's host candidates: its bound address when it names one, otherwise the local
/// address the system would send to `toward` from (a connected UDP socket's address; no packet
/// is sent), with the game socket's port.
#[derive(Debug, Clone, Copy)]
pub struct HostCandidates {
    /// An address to find the outgoing interface toward (the relay).
    pub toward: SocketAddr,
}

impl CandidateSource for HostCandidates {
    fn candidates(&mut self, base: SocketAddr, _now_ms: u64) -> Vec<Candidate> {
        host_candidate_addrs(base, self.toward)
            .into_iter()
            .map(|a| Candidate::new(CandidateKind::Host, a))
            .collect()
    }
}

/// The host candidate addresses of a socket bound at `base` (see [`HostCandidates`]). Port 0 or
/// an unroutable answer gives none.
pub fn host_candidate_addrs(base: SocketAddr, toward: SocketAddr) -> Vec<SocketAddr> {
    if base.port() == 0 {
        return Vec::new();
    }
    if !base.ip().is_unspecified() {
        return vec![base];
    }
    let probe_bind: SocketAddr = match toward.ip() {
        IpAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        IpAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
    };
    let Ok(probe) = std::net::UdpSocket::bind(probe_bind) else {
        return Vec::new();
    };
    // `connect` on UDP only picks the route; nothing leaves the machine.
    if probe.connect(toward).is_err() {
        return Vec::new();
    }
    match probe.local_addr() {
        Ok(a) if !a.ip().is_unspecified() => vec![SocketAddr::new(a.ip(), base.port())],
        _ => Vec::new(),
    }
}

/// Gathers the candidates of a game socket bound at `base` from `sources`, deduplicated by
/// address (the first kind wins), at most [`NAT_MAX_CANDIDATES`], highest priority first.
pub fn gather(sources: &mut [Box<dyn CandidateSource>], base: SocketAddr, now_ms: u64) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for source in sources.iter_mut() {
        for c in source.candidates(base, now_ms) {
            if !out.iter().any(|o| o.addr == c.addr) {
                out.push(c);
            }
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.priority));
    out.truncate(NAT_MAX_CANDIDATES);
    out
}

/// Writes a candidate list: `count u8 | (kind u8, priority u32, addr)*`, at most
/// [`NAT_MAX_CANDIDATES`].
pub fn encode_candidates(w: &mut WireWriter, candidates: &[Candidate]) {
    let n = candidates.len().min(NAT_MAX_CANDIDATES);
    w.u8(n as u8);
    for c in &candidates[..n] {
        w.u8(c.kind as u8);
        w.u32(c.priority);
        encode_addr(w, c.addr);
    }
}

/// Reads [`encode_candidates`]' bytes. Unknown kinds fail; more than [`NAT_MAX_CANDIDATES`]
/// fails.
///
/// # Errors
///
/// Truncated or malformed input.
pub fn decode_candidates(r: &mut WireReader<'_>) -> Result<Vec<Candidate>, WireError> {
    let n = usize::from(r.u8()?);
    if n > NAT_MAX_CANDIDATES {
        return Err(WireError::Invalid("candidate count"));
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = CandidateKind::from_u8(r.u8()?).ok_or(WireError::Invalid("candidate kind"))?;
        let priority = r.u32()?;
        let addr = decode_addr(r)?;
        out.push(Candidate { kind, addr, priority });
    }
    Ok(out)
}

/// A connectivity check, decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Check {
    /// "Can you hear me?" from the session `session`.
    Request {
        /// The session key.
        session: [u8; NAT_SESSION_KEY_BYTES],
        /// The transaction id, echoed by the answer.
        txid: u64,
        /// The sender is the controlling side and nominates this pair (USE-CANDIDATE).
        nominate: bool,
        /// The sender's candidate priority for this pair, used for a peer-reflexive candidate.
        priority: u32,
    },
    /// "I heard you, from `mapped`."
    Response {
        /// The session key.
        session: [u8; NAT_SESSION_KEY_BYTES],
        /// The request's transaction id.
        txid: u64,
        /// The request's source as the answering side saw it (XOR-MAPPED-ADDRESS's role).
        mapped: SocketAddr,
    },
}

const CHECK_REQUEST: u8 = 1;
const CHECK_RESPONSE: u8 = 2;

/// Whether `datagram` is a connectivity check (starts with [`NAT_CHECK_MAGIC`]).
pub fn is_check(datagram: &[u8]) -> bool {
    datagram.len() >= 5 && datagram[..4] == NAT_CHECK_MAGIC
}

/// Encodes a check. A request is padded to [`NAT_CHECK_REQUEST_BYTES`].
pub fn encode_check(check: &Check) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(NAT_CHECK_REQUEST_BYTES);
    w.raw(&NAT_CHECK_MAGIC);
    match *check {
        Check::Request {
            session,
            txid,
            nominate,
            priority,
        } => {
            w.u8(CHECK_REQUEST);
            w.raw(&session);
            w.u64(txid);
            w.u8(u8::from(nominate));
            w.u32(priority);
            let mut bytes = w.finish();
            bytes.resize(NAT_CHECK_REQUEST_BYTES, 0);
            bytes
        }
        Check::Response {
            session,
            txid,
            mapped,
        } => {
            w.u8(CHECK_RESPONSE);
            w.raw(&session);
            w.u64(txid);
            encode_addr(&mut w, mapped);
            w.finish()
        }
    }
}

/// Decodes a check; `None` for anything else, a short request, or malformed bytes.
pub fn decode_check(datagram: &[u8]) -> Option<Check> {
    if !is_check(datagram) {
        return None;
    }
    let mut r = WireReader::new(&datagram[4..]);
    match r.u8().ok()? {
        CHECK_REQUEST if datagram.len() >= NAT_CHECK_REQUEST_BYTES => Some(Check::Request {
            session: r.array().ok()?,
            txid: r.u64().ok()?,
            nominate: r.u8().ok()? == 1,
            priority: r.u32().ok()?,
        }),
        CHECK_RESPONSE => Some(Check::Response {
            session: r.array().ok()?,
            txid: r.u64().ok()?,
            mapped: decode_addr(&mut r).ok()?,
        }),
        _ => None,
    }
}

/// Which side of a session this agent is (RFC 8445 § 6.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The joining client: it nominates.
    Controlling,
    /// The host.
    Controlled,
}

/// The state of one candidate pair (RFC 8445 § 6.1.2.6, without Frozen: every pair of a session
/// shares the one game socket as its base, so there is one foundation and nothing to unfreeze).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    /// Not checked yet.
    Waiting,
    /// A check is out.
    InProgress,
    /// A check was answered: the pair works both ways.
    Succeeded,
    /// Every attempt went unanswered.
    Failed,
}

impl PairState {
    /// Stable lowercase name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::InProgress => "in_progress",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// One candidate pair: the game socket and one remote candidate (RFC 8445 § 6.1.2.4: with one
/// base, a server-reflexive local candidate is replaced by its base, so the pairs of a session are
/// its remote candidates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pair {
    /// The remote candidate.
    pub remote: Candidate,
    /// The pair priority.
    pub priority: u64,
    /// The pair's state.
    pub state: PairState,
    /// Checks sent on this pair.
    pub sent: u32,
    txid: u64,
    next_send_ms: u64,
    /// A triggered check is due (RFC 8445 § 7.3.1.4): it goes before the ordinary ones.
    triggered: bool,
}

/// A check agent's counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CheckCounts {
    /// Check requests sent (first sends and retransmissions).
    pub requests_sent: u64,
    /// Check requests answered.
    pub requests_answered: u64,
    /// Answers received to this agent's own checks.
    pub responses: u64,
    /// Checks dropped: wrong session key, an answer to no check of ours, malformed.
    pub dropped: u64,
    /// Peer-reflexive candidates learned.
    pub peer_reflexive: u64,
}

/// One side of one session's connectivity checks.
#[derive(Debug, Clone)]
pub struct CheckAgent {
    role: Role,
    session: [u8; NAT_SESSION_KEY_BYTES],
    /// The priority of this side's base candidate (its host candidate), used for every pair.
    local_priority: u32,
    pairs: Vec<Pair>,
    next_txid: u64,
    next_check_ms: u64,
    /// The address this side's checks were last seen from (server- or peer-reflexive).
    mapped: Option<SocketAddr>,
    /// A pair the other side nominated (controlled) or this side nominated (controlling).
    nominated: Option<SocketAddr>,
    started_ms: u64,
    counts: CheckCounts,
}

impl CheckAgent {
    /// An agent for `session` checking `remote` (the other side's candidates), started at
    /// `now_ms`. `local` is this side's candidate list (only its best priority is used: the
    /// pairs share one base).
    pub fn new(
        role: Role,
        session: [u8; NAT_SESSION_KEY_BYTES],
        local: &[Candidate],
        remote: &[Candidate],
        now_ms: u64,
    ) -> Self {
        let local_priority = local
            .iter()
            .filter(|c| c.kind == CandidateKind::Host)
            .map(|c| c.priority)
            .max()
            .unwrap_or((CandidateKind::Host.type_preference() << 24) + (65_534 << 8) + 255);
        let mut agent = Self {
            role,
            session,
            local_priority,
            pairs: Vec::new(),
            next_txid: u64::from_le_bytes(session[..8].try_into().unwrap_or([0; 8])) | 1,
            next_check_ms: now_ms,
            mapped: None,
            nominated: None,
            started_ms: now_ms,
            counts: CheckCounts::default(),
        };
        for c in remote {
            agent.add_remote(*c, false, now_ms);
        }
        agent
    }

    /// The session key.
    pub fn session(&self) -> &[u8; NAT_SESSION_KEY_BYTES] {
        &self.session
    }

    /// When the session started.
    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }

    /// The pairs, highest priority first.
    pub fn pairs(&self) -> &[Pair] {
        &self.pairs
    }

    /// The counters.
    pub fn counts(&self) -> CheckCounts {
        self.counts
    }

    /// This side's address as the other side last saw it.
    pub fn mapped(&self) -> Option<SocketAddr> {
        self.mapped
    }

    /// The nominated pair's remote address.
    pub fn nominated(&self) -> Option<SocketAddr> {
        self.nominated
    }

    /// The best pair whose check was answered.
    pub fn best_valid(&self) -> Option<&Pair> {
        self.pairs.iter().find(|p| p.state == PairState::Succeeded)
    }

    /// Whether every pair failed (nothing left to try).
    pub fn exhausted(&self) -> bool {
        self.pairs.iter().all(|p| p.state == PairState::Failed)
    }

    fn pair_priority_with(&self, remote: u32) -> u64 {
        match self.role {
            Role::Controlling => pair_priority(self.local_priority, remote),
            Role::Controlled => pair_priority(remote, self.local_priority),
        }
    }

    /// Adds a remote candidate (an announced one, or a peer-reflexive one with `triggered`).
    /// Returns whether it is new. At most [`NAT_MAX_REMOTE_CANDIDATES`].
    fn add_remote(&mut self, c: Candidate, triggered: bool, now_ms: u64) -> bool {
        if c.kind == CandidateKind::Relayed || c.addr.port() == 0 || c.addr.ip().is_unspecified() {
            return false;
        }
        if let Some(p) = self.pairs.iter_mut().find(|p| p.remote.addr == c.addr) {
            if triggered && p.state != PairState::Succeeded {
                p.triggered = true;
                p.state = PairState::Waiting;
                p.sent = 0;
                p.next_send_ms = now_ms;
            }
            return false;
        }
        if self.pairs.len() >= NAT_MAX_REMOTE_CANDIDATES {
            return false;
        }
        let priority = self.pair_priority_with(c.priority);
        self.pairs.push(Pair {
            remote: c,
            priority,
            state: PairState::Waiting,
            sent: 0,
            txid: 0,
            next_send_ms: now_ms,
            triggered,
        });
        self.pairs.sort_by_key(|p| std::cmp::Reverse(p.priority));
        true
    }

    /// Handles a check that came from `from`; returns the datagram to answer with, if any.
    pub fn on_check(
        &mut self,
        check: &Check,
        from: SocketAddr,
        now_ms: u64,
    ) -> Option<(Vec<u8>, SocketAddr)> {
        match *check {
            Check::Request {
                session,
                txid,
                nominate,
                priority,
            } => {
                if !constant_time_eq(&session, &self.session) {
                    self.counts.dropped += 1;
                    return None;
                }
                // A source nobody announced is a peer-reflexive candidate: check it back at once
                // (a triggered check, RFC 8445 § 7.3.1.4); a known one gets a triggered check too.
                let prflx = Candidate {
                    kind: CandidateKind::PeerReflexive,
                    addr: from,
                    priority,
                };
                if self.add_remote(prflx, true, now_ms) {
                    self.counts.peer_reflexive += 1;
                } else if let Some(p) = self
                    .pairs
                    .iter_mut()
                    .find(|p| p.remote.addr == from && p.state != PairState::Succeeded)
                {
                    p.triggered = true;
                    p.next_send_ms = now_ms.min(p.next_send_ms);
                }
                if nominate && self.role == Role::Controlled {
                    self.nominated = Some(from);
                }
                self.counts.requests_answered += 1;
                let answer = Check::Response {
                    session: self.session,
                    txid,
                    mapped: from,
                };
                Some((encode_check(&answer), from))
            }
            Check::Response {
                session,
                txid,
                mapped,
            } => {
                if !constant_time_eq(&session, &self.session) {
                    self.counts.dropped += 1;
                    return None;
                }
                // The answer must come from where the check went (RFC 8445 § 7.2.5.2.1).
                let Some(p) = self
                    .pairs
                    .iter_mut()
                    .find(|p| p.remote.addr == from && p.txid == txid && p.sent > 0)
                else {
                    self.counts.dropped += 1;
                    return None;
                };
                p.state = PairState::Succeeded;
                p.triggered = false;
                self.counts.responses += 1;
                self.mapped = Some(mapped);
                None
            }
        }
    }

    /// Marks the pair toward `remote` nominated (the controlling side, once its sealed handshake
    /// completed there) and queues one nominating check so the other side learns it.
    pub fn nominate(&mut self, remote: SocketAddr, now_ms: u64) {
        if self.nominated == Some(remote) {
            return;
        }
        self.nominated = Some(remote);
        if let Some(p) = self.pairs.iter_mut().find(|p| p.remote.addr == remote) {
            p.triggered = true;
            p.next_send_ms = now_ms;
            p.sent = 0;
        }
    }

    /// When the next check may go (the pacing slot), for callers that sleep.
    pub fn next_check_ms(&self) -> u64 {
        self.next_check_ms
    }

    /// The next check due at `now_ms`, if this session's pacing slot is open: a triggered check
    /// first, then a retransmission whose RTO passed or the best waiting pair. One check per
    /// [`NAT_CHECK_PACE_MS`]. Call it again only after another [`NAT_CHECK_PACE_MS`].
    pub fn poll(&mut self, now_ms: u64) -> Option<(Vec<u8>, SocketAddr)> {
        if now_ms < self.next_check_ms {
            return None;
        }
        let nominated = self.nominated;
        let role = self.role;
        let rto = NAT_CHECK_RTO_MS.max(NAT_CHECK_PACE_MS * self.pairs.len() as u64);
        // Fail pairs whose last attempt went unanswered for a whole RTO.
        for p in &mut self.pairs {
            if p.state == PairState::InProgress && p.sent >= NAT_CHECK_ATTEMPTS && now_ms >= p.next_send_ms {
                p.state = PairState::Failed;
                p.triggered = false;
            }
        }
        let due = |p: &Pair| match p.state {
            PairState::Waiting => now_ms >= p.next_send_ms,
            PairState::InProgress => now_ms >= p.next_send_ms && p.sent < NAT_CHECK_ATTEMPTS,
            // A succeeded pair is checked again only to carry a nomination.
            PairState::Succeeded => p.triggered && p.sent == 0,
            PairState::Failed => false,
        };
        let index = self
            .pairs
            .iter()
            .position(|p| p.triggered && due(p))
            .or_else(|| self.pairs.iter().position(|p| !p.triggered && due(p)))?;
        let txid = self.next_txid;
        self.next_txid = self.next_txid.wrapping_add(2);
        let p = &mut self.pairs[index];
        let nominate = role == Role::Controlling && nominated == Some(p.remote.addr);
        if p.state == PairState::Succeeded {
            p.triggered = false;
            p.sent = 1;
        } else {
            p.state = PairState::InProgress;
            p.sent += 1;
            p.triggered = false;
        }
        p.txid = txid;
        p.next_send_ms = now_ms.saturating_add(rto);
        let to = p.remote.addr;
        self.next_check_ms = now_ms.saturating_add(NAT_CHECK_PACE_MS);
        self.counts.requests_sent += 1;
        let check = Check::Request {
            session: self.session,
            txid,
            nominate,
            priority: Candidate::new(CandidateKind::PeerReflexive, to).priority,
        };
        Some((encode_check(&check), to))
    }
}

/// The floor every session of one process shares: checks of all sessions together at most one
/// per [`NAT_CHECK_FLOOR_MS`] (RFC 8445 § 14.2). A burst after a quiet spell is not saved up.
#[derive(Debug, Clone, Copy, Default)]
pub struct CheckPacer {
    next_ms: u64,
}

impl CheckPacer {
    /// Whether a check may go at `now_ms`; takes the slot when it may.
    pub fn take(&mut self, now_ms: u64) -> bool {
        if now_ms < self.next_ms {
            return false;
        }
        self.next_ms = now_ms.saturating_add(NAT_CHECK_FLOOR_MS);
        true
    }
}

/// One candidate as `net.relay_status` reports it.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CandidateView {
    /// `host`, `mapped`, `srflx`, `prflx` or `relay`.
    pub kind: String,
    /// The address.
    pub addr: String,
}

impl From<&Candidate> for CandidateView {
    fn from(c: &Candidate) -> Self {
        Self {
            kind: c.kind.as_str().to_owned(),
            addr: c.addr.to_string(),
        }
    }
}

/// A host's NAT traversal counters as `net.relay_status` reports them (spec 120 § 10.7).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct NatReport {
    /// The candidates the host announced to the relay.
    pub candidates: Vec<CandidateView>,
    /// Introductions (sessions) the relay delivered.
    pub sessions: u64,
    /// Sessions running now.
    pub active_sessions: u64,
    /// Check requests the host sent.
    pub checks_sent: u64,
    /// Client checks the host answered.
    pub checks_answered: u64,
    /// Checks dropped (no such session, malformed).
    pub checks_dropped: u64,
    /// Sessions whose client nominated a direct pair.
    pub nominated: u64,
    /// Sessions that ended or were replaced before a nomination (the relay path, or nothing).
    pub not_nominated: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(last: u8, port: u16) -> SocketAddr {
        SocketAddr::from(([198, 51, 100, last], port))
    }

    /// RFC 8445 § 5.1.2.1 and § 6.1.2.3 by hand: a host candidate outranks a server-reflexive one,
    /// and the pair formula orders by the smaller priority first.
    #[test]
    fn priorities_follow_rfc_8445() {
        let host = Candidate::new(CandidateKind::Host, a(1, 1));
        let srflx = Candidate::new(CandidateKind::ServerReflexive, a(1, 2));
        assert_eq!(host.priority, (126 << 24) + (65_534 << 8) + 255);
        assert!(host.priority > srflx.priority);
        assert_eq!(pair_priority(10, 20), (10u64 << 32) + 40);
        assert_eq!(pair_priority(20, 10), (10u64 << 32) + 41);
        assert!(pair_priority(host.priority, host.priority) > pair_priority(host.priority, srflx.priority));
    }

    #[test]
    fn checks_and_candidates_round_trip_and_requests_outweigh_answers() {
        let session = [7u8; NAT_SESSION_KEY_BYTES];
        let req = Check::Request {
            session,
            txid: 9,
            nominate: true,
            priority: 5,
        };
        let bytes = encode_check(&req);
        assert_eq!(bytes.len(), NAT_CHECK_REQUEST_BYTES);
        assert_eq!(decode_check(&bytes), Some(req));
        assert_eq!(decode_check(&bytes[..40]), None, "a short request is dropped");
        let v6 = SocketAddr::from(([0x2001, 0xdb8, 0, 0, 0, 0, 0, 1], 9));
        let resp = Check::Response {
            session,
            txid: 9,
            mapped: v6,
        };
        let answer = encode_check(&resp);
        assert!(
            answer.len() <= bytes.len(),
            "the answer is never larger than the request"
        );
        assert_eq!(decode_check(&answer), Some(resp));
        let list: Vec<Candidate> = (0..NAT_MAX_CANDIDATES as u8)
            .map(|i| Candidate::new(CandidateKind::Host, a(i, 1000)))
            .collect();
        let mut w = WireWriter::new();
        encode_candidates(&mut w, &list);
        let bytes = w.finish();
        assert_eq!(
            decode_candidates(&mut WireReader::new(&bytes)).expect("decodes"),
            list
        );
        let mut too_many = bytes.clone();
        too_many[0] = NAT_MAX_CANDIDATES as u8 + 1;
        assert!(decode_candidates(&mut WireReader::new(&too_many)).is_err());
    }

    /// Ta: one session sends at most one check per 50 ms however often it is polled and however
    /// many pairs it has; each pair stops after its attempts; a wrong key is never answered.
    #[test]
    fn one_session_is_paced_at_ta_and_gives_up() {
        let remote: Vec<Candidate> = (1..=8)
            .map(|i| Candidate::new(CandidateKind::Host, a(i, 4000)))
            .collect();
        let mut agent = CheckAgent::new(Role::Controlling, [1; 16], &[], &remote, 0);
        let mut sends = Vec::new();
        for now in 0..10_000u64 {
            if let Some((bytes, to)) = agent.poll(now) {
                assert!(is_check(&bytes));
                sends.push((now, to));
            }
        }
        for w in sends.windows(2) {
            assert!(w[1].0 - w[0].0 >= NAT_CHECK_PACE_MS, "{w:?}");
        }
        assert_eq!(
            sends.len(),
            8 * NAT_CHECK_ATTEMPTS as usize,
            "every pair tries its attempts"
        );
        assert!(agent.exhausted());
        let forged = encode_check(&Check::Request {
            session: [2; 16],
            txid: 1,
            nominate: false,
            priority: 1,
        });
        assert!(
            agent
                .on_check(&decode_check(&forged).expect("check"), a(9, 1), 10_000)
                .is_none()
        );
        assert_eq!(agent.counts().dropped, 1);
    }

    /// The process-wide floor: 40 sessions polled every millisecond send no two checks less than
    /// 5 ms apart.
    #[test]
    fn every_session_of_a_process_shares_the_5_ms_floor() {
        let mut pacer = CheckPacer::default();
        let mut agents: Vec<CheckAgent> = (0..40u8)
            .map(|i| {
                CheckAgent::new(
                    Role::Controlled,
                    [i; 16],
                    &[],
                    &[Candidate::new(CandidateKind::ServerReflexive, a(i, 5000))],
                    0,
                )
            })
            .collect();
        let mut times = Vec::new();
        for now in 0..3_000u64 {
            for agent in &mut agents {
                if agent.next_check_ms() <= now && pacer.take(now) {
                    if agent.poll(now).is_some() {
                        times.push(now);
                    }
                    break;
                }
            }
        }
        assert!(times.len() >= 40, "every session checked: {}", times.len());
        for w in times.windows(2) {
            assert!(w[1] - w[0] >= NAT_CHECK_FLOOR_MS, "{w:?}");
        }
    }

    /// Two agents that can reach each other: the controlled side learns the controlling side's
    /// source as a peer-reflexive candidate, checks it back, and both pairs succeed.
    #[test]
    fn two_agents_validate_a_pair_through_a_peer_reflexive_candidate() {
        let key = [3u8; 16];
        let host_addr = a(1, 7777);
        let client_seen = a(2, 40_001);
        let mut client = CheckAgent::new(
            Role::Controlling,
            key,
            &[],
            &[Candidate::new(CandidateKind::ServerReflexive, host_addr)],
            0,
        );
        let mut host = CheckAgent::new(Role::Controlled, key, &[], &[], 0);
        let (req, to) = client.poll(0).expect("a check");
        assert_eq!(to, host_addr);
        let (answer, back) = host
            .on_check(&decode_check(&req).expect("check"), client_seen, 0)
            .expect("answered");
        assert_eq!(back, client_seen);
        assert_eq!(host.counts().peer_reflexive, 1);
        client.on_check(&decode_check(&answer).expect("check"), host_addr, 1);
        assert_eq!(client.best_valid().map(|p| p.remote.addr), Some(host_addr));
        assert_eq!(client.mapped(), Some(client_seen));
        let (triggered, to) = host.poll(1).expect("a triggered check");
        assert_eq!(to, client_seen);
        let (answer, _) = client
            .on_check(&decode_check(&triggered).expect("check"), host_addr, 2)
            .expect("answered");
        host.on_check(&decode_check(&answer).expect("check"), client_seen, 2);
        assert_eq!(host.best_valid().map(|p| p.remote.addr), Some(client_seen));
        client.nominate(host_addr, 100);
        let (nom, _) = client.poll(100).expect("the nominating check");
        host.on_check(&decode_check(&nom).expect("check"), client_seen, 100);
        assert_eq!(host.nominated(), Some(client_seen));
    }
}
