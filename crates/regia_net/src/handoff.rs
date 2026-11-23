// SPDX-License-Identifier: MPL-2.0
//! Host migration for replication games, part 2: the hand-off (spec 120 § 10.5; ADR-8901).
//!
//! [`crate::migration`] keeps one or two warm successors: clients whose shadow world trails the
//! host by a tick or two. This module is what happens when the host is lost:
//!
//! 1. **Before the loss** the host tells everybody where the match goes next. Each successor
//!    creates the key pair of the server it may become and announces the public half
//!    ([`HandoffClient::poll_out`]). The host ([`HandoffHost`]) answers with a **grant** to each
//!    successor (a secret of its own and the players it would have to take over) and a **roster**
//!    to every client: the successors in order, each with the address it will serve on, its
//!    public key and this client's **ticket** for it. All of it rides the sealed link, reliable.
//! 2. **Host lost.** A client that has not heard the host for [`HandoffConfig::host_lost_ms`]
//!    (1.5 s), or whose connection ended, asks [`HandoffClient::poll`] what to do:
//!    - the first successor **arms** ([`HandoffAction::Arm`]): it opens a [`NetServer`] under the
//!      announced key on the announced port, and [`PromotionPlan::apply`] holds every player of
//!      the lost host as a dormant peer of that server under its old `PeerId`. It keeps its own
//!      connection to the host and its shadow world. When a majority of the players has resumed
//!      to it (or its own connection to the host ended) it **commits**
//!      ([`HandoffAction::Commit`]): the game takes the shadow world out of the journal
//!      ([`crate::migration::JournalSuccessor::promote`]) and the server starts to tick. When it
//!      hears the host again first it **disarms** ([`HandoffAction::Disarm`]): the silence was
//!      its own outage and nothing happened;
//!    - every other client **resumes** ([`HandoffAction::Resume`]): a new connection to the
//!      successor's address with the ticket as its resume ticket (spec 120 § 10.3) and the
//!      successor's key pinned, so it comes back as the same peer and no other process can take
//!      the match over. The successor's cookie exchange validates the client's path as for any
//!      join. The attempt runs on a second socket and the connection to the host is kept until
//!      the successor has accepted.
//! 3. **The first successor does not answer** within [`HandoffConfig::takeover_ms`] (3 s): the
//!    second successor promotes and everybody else resumes to it.
//! 4. **The host is heard again** before a successor accepted (the silence was this client's own
//!    outage): [`HandoffAction::StayWithHost`]. The attempt is dropped and nothing was lost. A
//!    successor that is not in the same outage never promoted, so there is one match.
//! 5. **Nobody answers**: [`HandoffAction::ReturnToHost`]. The attempt is dropped; the connection
//!    to the host carries on under the transport's own timeout and resume (spec 120 § 10.3).
//!
//! The lost host's own player has no `PeerId` of its own (it was the server). The promoted
//! successor gives it one ([`PromotionPlan::host_seat`]) and holds it dormant like the others; a
//! host that was only cut off comes back as a client with [`HandoffHost::comeback`].
//!
//! Tickets: the host derives one secret per successor (from its own random master secret, the
//! successor's id and announced key), and a client's ticket for a successor is a keyed BLAKE3 of
//! that secret and the client's id. A successor can therefore check every ticket for its own
//! server and cannot forge one for the other successor's.
//!
//! The module is sans-IO and in the ECS-free core. [`HandoffHost::pump_server`] does the host's
//! side on a [`NetServer`]; the client's side is a handful of calls in the game's network loop
//! (`book/user-guide/networking.md`, "Host migration: the hand-off").

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::client::ClientConfig;
use crate::connection::{Channel, SendError};
use crate::crypto::{ServerIdentity, ServerKey};
use crate::handshake::{PeerRole, RESUME_TICKET_BYTES, decode_addr, encode_addr};
use crate::migration::JournalHost;
use crate::payload::NetGame;
use crate::server::{AdoptError, NetServer, ServerConfig};
use crate::session::PeerId;
use crate::socket::DatagramSocket;
use crate::status::NetAgentState;
use crate::wire::{WireError, WireReader, WireWriter};

/// Default [`HandoffConfig::host_lost_ms`]: the transport's own interruption threshold.
pub const HANDOFF_HOST_LOST_MS: u64 = 1_500;
/// Default [`HandoffConfig::takeover_ms`] (spec 120 § 10.5: the second successor takes over when
/// the first does not answer within 3 s).
pub const HANDOFF_TAKEOVER_MS: u64 = 3_000;
/// Default [`HandoffConfig::quorum_wait_ms`].
pub const HANDOFF_QUORUM_WAIT_MS: u64 = 10_000;
/// Players a grant lists at most.
pub const MAX_HANDOFF_PEERS: usize = 1_024;
/// Successors a roster lists at most (the journal keeps two).
pub const MAX_HANDOFF_SUCCESSORS: usize = crate::migration::MAX_SUCCESSORS;
/// The resume grace a promoted server gets when the game's own configuration has none: 60 s.
pub const HANDOFF_RESUME_GRACE_MS: u64 = 60_000;
/// A resume attempt resends its handshake packets at least this often, milliseconds.
pub const HANDOFF_HANDSHAKE_RESEND_MS: u64 = 100;

const KIND_ANNOUNCE: u8 = 0x48;
const KIND_GRANT: u8 = 0x49;
const KIND_ROSTER: u8 = 0x4A;
const DOMAIN_SUCCESSOR_SECRET: u8 = 0x49;
const DOMAIN_TICKET: u8 = 0x4A;

/// Whether `bytes` is a hand-off payload (by its first byte), so a game can route its reliable
/// messages. The kinds do not overlap the journal's ([`crate::migration::is_journal_payload`]).
pub fn is_handoff_payload(bytes: &[u8]) -> bool {
    matches!(bytes.first(), Some(&(KIND_ANNOUNCE..=KIND_ROSTER)))
}

/// Why a hand-off call or payload was refused. [`Self::code`] is the agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HandoffError {
    /// The payload does not decode.
    #[error("malformed hand-off payload: {0}")]
    Malformed(#[from] WireError),
    /// The first byte is not a hand-off kind this side takes.
    #[error("hand-off payload kind {0:#04x} is not for this side")]
    WrongKind(u8),
    /// An announced key of all zeros.
    #[error("announced key is all zeros")]
    ZeroKey,
    /// An announcement from a peer the host does not list as a player.
    #[error("announcement from a peer that is not a player")]
    NotAPlayer,
    /// A roster with more successors than [`MAX_HANDOFF_SUCCESSORS`].
    #[error("roster lists {0} successors")]
    TooManySuccessors(usize),
    /// A grant with more players than [`MAX_HANDOFF_PEERS`].
    #[error("grant lists {0} players")]
    TooManyPeers(usize),
    /// A roster or grant older than the one held.
    #[error("older than the roster or grant held")]
    Stale,
    /// A roster whose successors repeat a peer, or a grant whose players do.
    #[error("a peer is listed twice")]
    Duplicate,
    /// A ticket of all zeros in a roster.
    #[error("roster ticket is all zeros")]
    ZeroTicket,
    /// A threshold of zero or over a minute in a roster.
    #[error("roster threshold out of range")]
    BadThreshold,
}

impl HandoffError {
    /// Lowercase code (agent protocol, logs).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "handoff_malformed",
            Self::WrongKind(_) => "handoff_wrong_kind",
            Self::ZeroKey => "handoff_zero_key",
            Self::NotAPlayer => "handoff_not_a_player",
            Self::TooManySuccessors(_) => "handoff_too_many_successors",
            Self::TooManyPeers(_) => "handoff_too_many_peers",
            Self::Stale => "handoff_stale",
            Self::Duplicate => "handoff_duplicate_peer",
            Self::ZeroTicket => "handoff_zero_ticket",
            Self::BadThreshold => "handoff_bad_threshold",
        }
    }
}

/// Hand-off thresholds. The host's values travel in the roster, so every client of a match uses
/// the same ones; a client's own values apply only before its first roster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HandoffConfig {
    /// Silence from the host after which a client treats it as lost, milliseconds.
    pub host_lost_ms: u64,
    /// How long a successor has to answer before the next one takes over, milliseconds.
    pub takeover_ms: u64,
    /// How long a successor that took the match over without a majority of the players (it
    /// announced no port, or its own connection to the host ended first) waits for one before it
    /// reports [`HandoffEvent::NoQuorum`], milliseconds.
    pub quorum_wait_ms: u64,
}

impl Default for HandoffConfig {
    fn default() -> Self {
        Self {
            host_lost_ms: HANDOFF_HOST_LOST_MS,
            takeover_ms: HANDOFF_TAKEOVER_MS,
            quorum_wait_ms: HANDOFF_QUORUM_WAIT_MS,
        }
    }
}

impl HandoffConfig {
    /// The configuration with every value in range (250 ms to 60 s).
    pub fn sanitized(mut self) -> Self {
        self.host_lost_ms = self.host_lost_ms.clamp(250, 60_000);
        self.takeover_ms = self.takeover_ms.clamp(250, 60_000);
        self.quorum_wait_ms = self.quorum_wait_ms.clamp(250, 600_000);
        self
    }
}

fn successor_secret(master: &[u8; 32], peer: PeerId, key: &ServerKey) -> [u8; 32] {
    let mut w = WireWriter::with_capacity(37);
    w.u8(DOMAIN_SUCCESSOR_SECRET);
    w.u32(peer.0);
    w.raw(&key.0);
    *blake3::keyed_hash(master, w.as_slice()).as_bytes()
}

/// The resume ticket of `peer` on the server of the successor that holds `secret`.
fn ticket_for(secret: &[u8; 32], peer: PeerId) -> [u8; RESUME_TICKET_BYTES] {
    let mut w = WireWriter::with_capacity(5);
    w.u8(DOMAIN_TICKET);
    w.u32(peer.0);
    let mac = blake3::keyed_hash(secret, w.as_slice());
    let mut out = [0u8; RESUME_TICKET_BYTES];
    out.copy_from_slice(&mac.as_bytes()[..RESUME_TICKET_BYTES]);
    if out.iter().all(|b| *b == 0) {
        // All zeros is the wire's "no ticket".
        out[0] = 1;
    }
    out
}

/// One successor in a client's roster.
#[derive(Clone, PartialEq, Eq)]
pub struct RosterEntry {
    /// The successor's peer id on the host.
    pub peer: PeerId,
    /// The address it serves on after a promotion.
    pub addr: SocketAddr,
    /// The public key of the server it becomes.
    pub key: ServerKey,
    /// This client's resume ticket for that server.
    pub ticket: [u8; RESUME_TICKET_BYTES],
}

impl std::fmt::Debug for RosterEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ticket is a credential: not printed.
        f.debug_struct("RosterEntry")
            .field("peer", &self.peer)
            .field("addr", &self.addr)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

/// What the host tells one client about the hand-off.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    /// The host's generation: 1 for the host that started the match, one more per hand-off.
    pub epoch: u32,
    /// Counts up with every change of the successors.
    pub version: u32,
    /// The host's [`HandoffConfig::host_lost_ms`].
    pub host_lost_ms: u64,
    /// The host's [`HandoffConfig::takeover_ms`].
    pub takeover_ms: u64,
    /// The successors, first successor first.
    pub successors: Vec<RosterEntry>,
}

impl Roster {
    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(16 + self.successors.len() * 72);
        w.u8(KIND_ROSTER);
        w.u32(self.epoch);
        w.u32(self.version);
        w.u32(self.host_lost_ms.min(u64::from(u32::MAX)) as u32);
        w.u32(self.takeover_ms.min(u64::from(u32::MAX)) as u32);
        w.u8(self.successors.len().min(usize::from(u8::MAX)) as u8);
        for s in &self.successors {
            w.u32(s.peer.0);
            encode_addr(&mut w, s.addr);
            w.raw(&s.key.0);
            w.raw(&s.ticket);
        }
        w.finish()
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, HandoffError> {
        let epoch = r.u32()?;
        let version = r.u32()?;
        let host_lost_ms = u64::from(r.u32()?);
        let takeover_ms = u64::from(r.u32()?);
        if !(1..=60_000).contains(&host_lost_ms) || !(1..=60_000).contains(&takeover_ms) {
            return Err(HandoffError::BadThreshold);
        }
        let n = usize::from(r.u8()?);
        if n > MAX_HANDOFF_SUCCESSORS {
            return Err(HandoffError::TooManySuccessors(n));
        }
        let mut successors: Vec<RosterEntry> = Vec::with_capacity(n);
        for _ in 0..n {
            let peer = PeerId(r.u32()?);
            let addr = decode_addr(r)?;
            let key = ServerKey(r.array()?);
            let ticket: [u8; RESUME_TICKET_BYTES] = r.array()?;
            if key.0.iter().all(|b| *b == 0) {
                return Err(HandoffError::ZeroKey);
            }
            if ticket.iter().all(|b| *b == 0) {
                return Err(HandoffError::ZeroTicket);
            }
            if successors.iter().any(|s| s.peer == peer) {
                return Err(HandoffError::Duplicate);
            }
            successors.push(RosterEntry {
                peer,
                addr,
                key,
                ticket,
            });
        }
        r.finish()?;
        Ok(Self {
            epoch,
            version,
            host_lost_ms,
            takeover_ms,
            successors,
        })
    }
}

/// What the host gives a successor: the secret its tickets are made of and the players it would
/// take over.
#[derive(Clone, PartialEq, Eq)]
struct Grant {
    epoch: u32,
    version: u32,
    /// The key this grant was made for (the successor's announcement).
    key: ServerKey,
    secret: [u8; 32],
    /// Every player of the host but the successor itself.
    peers: Vec<PeerId>,
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The secret is not printed.
        f.debug_struct("Grant")
            .field("epoch", &self.epoch)
            .field("version", &self.version)
            .field("peers", &self.peers.len())
            .finish_non_exhaustive()
    }
}

impl Grant {
    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(80 + self.peers.len() * 4);
        w.u8(KIND_GRANT);
        w.u32(self.epoch);
        w.u32(self.version);
        w.raw(&self.key.0);
        w.raw(&self.secret);
        w.u16(self.peers.len().min(usize::from(u16::MAX)) as u16);
        for p in &self.peers {
            w.u32(p.0);
        }
        w.finish()
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, HandoffError> {
        let epoch = r.u32()?;
        let version = r.u32()?;
        let key = ServerKey(r.array()?);
        let secret: [u8; 32] = r.array()?;
        let n = usize::from(r.u16()?);
        if n > MAX_HANDOFF_PEERS {
            return Err(HandoffError::TooManyPeers(n));
        }
        // The length is checked against the bytes left before anything is stored.
        if r.remaining() != n * 4 {
            return Err(HandoffError::Malformed(WireError::Truncated {
                at: r.position(),
                needed: (n * 4).saturating_sub(r.remaining()),
            }));
        }
        let mut seen = BTreeSet::new();
        let mut peers = Vec::with_capacity(n);
        for _ in 0..n {
            let peer = PeerId(r.u32()?);
            if !seen.insert(peer) {
                return Err(HandoffError::Duplicate);
            }
            peers.push(peer);
        }
        r.finish()?;
        Ok(Self {
            epoch,
            version,
            key,
            secret,
            peers,
        })
    }
}

/// A successor's announcement: the key and port of the server it would become.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Announce {
    key: ServerKey,
    /// 0: the address the host sees this client at (it serves on its client socket).
    port: u16,
}

impl Announce {
    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::with_capacity(35);
        w.u8(KIND_ANNOUNCE);
        w.raw(&self.key.0);
        w.u16(self.port);
        w.finish()
    }

    fn decode(r: &mut WireReader<'_>) -> Result<Self, HandoffError> {
        let key = ServerKey(r.array()?);
        let port = r.u16()?;
        r.finish()?;
        if key.0.iter().all(|b| *b == 0) {
            return Err(HandoffError::ZeroKey);
        }
        Ok(Self { key, port })
    }
}

/// One payload for one peer; send it on a reliable channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffOut {
    /// The peer to send to.
    pub peer: PeerId,
    /// The payload.
    pub bytes: Vec<u8>,
}

/// A successor the host can hand the match to: it announced its key and the host knows its
/// address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ready {
    peer: PeerId,
    addr: SocketAddr,
    key: ServerKey,
}

/// The host's side of the hand-off: collects the successors' announcements and keeps every
/// client's roster and every successor's grant up to date.
pub struct HandoffHost {
    config: HandoffConfig,
    master: [u8; 32],
    epoch: u32,
    roster_version: u32,
    grant_version: u32,
    peers: BTreeMap<PeerId, SocketAddr>,
    announces: BTreeMap<PeerId, Announce>,
    /// The journal's successors in its order.
    successors: Vec<PeerId>,
    ready: Vec<Ready>,
    sent_roster: BTreeMap<PeerId, u32>,
    sent_grant: BTreeMap<PeerId, u32>,
    rosters_sent: u64,
    grants_sent: u64,
    refused: u64,
    last_refusal: Option<&'static str>,
}

impl std::fmt::Debug for HandoffHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoffHost")
            .field("epoch", &self.epoch)
            .field("roster_version", &self.roster_version)
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

impl HandoffHost {
    /// A host with a fresh random master secret.
    pub fn new(config: HandoffConfig) -> Self {
        Self::with_secret(config, crate::handshake::fresh_secret())
    }

    /// A host with a fixed master secret (deterministic tests).
    pub fn with_secret(config: HandoffConfig, master: [u8; 32]) -> Self {
        Self {
            config: config.sanitized(),
            master,
            epoch: 1,
            roster_version: 0,
            grant_version: 0,
            peers: BTreeMap::new(),
            announces: BTreeMap::new(),
            successors: Vec::new(),
            ready: Vec::new(),
            sent_roster: BTreeMap::new(),
            sent_grant: BTreeMap::new(),
            rosters_sent: 0,
            grants_sent: 0,
            refused: 0,
            last_refusal: None,
        }
    }

    /// Sets the host's generation: a promoted successor passes [`PromotionPlan::epoch`], so its
    /// rosters replace the lost host's on every client.
    pub fn with_epoch(mut self, epoch: u32) -> Self {
        self.epoch = epoch.max(1);
        self
    }

    /// The thresholds in force.
    pub fn config(&self) -> &HandoffConfig {
        &self.config
    }

    /// The host's generation.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// The successors every client has been (or is being) told about, first successor first.
    pub fn ready_successors(&self) -> Vec<PeerId> {
        self.ready.iter().map(|r| r.peer).collect()
    }

    /// Sets the players and the address the host sees each at. A player that left takes its
    /// announcement with it.
    pub fn set_peers(&mut self, peers: &[(PeerId, SocketAddr)]) {
        let next: BTreeMap<PeerId, SocketAddr> = peers
            .iter()
            .filter(|(p, _)| *p != PeerId::SERVER)
            .take(MAX_HANDOFF_PEERS)
            .copied()
            .collect();
        let same_ids = next.len() == self.peers.len() && next.keys().all(|p| self.peers.contains_key(p));
        self.peers = next;
        if !same_ids {
            self.announces.retain(|p, _| self.peers.contains_key(p));
            self.sent_roster.retain(|p, _| self.peers.contains_key(p));
            self.sent_grant.retain(|p, _| self.peers.contains_key(p));
            // Every grant lists the players.
            self.grant_version += 1;
        }
        self.refresh();
    }

    /// Sets the journal's successors in its order (`JournalHost::successors`), which is the
    /// order of the roster. The order is not changed by which shadow is warm: it moves only when
    /// the set of successors does, because every change of the order is a moment in which two
    /// clients can hold different rosters.
    pub fn set_successors(&mut self, successors: &[PeerId]) {
        if self.successors != successors {
            self.successors = successors.iter().take(MAX_HANDOFF_SUCCESSORS).copied().collect();
            self.refresh();
        }
    }

    fn refresh(&mut self) {
        let ready: Vec<Ready> = self
            .successors
            .iter()
            .filter_map(|peer| {
                let addr = *self.peers.get(peer)?;
                let a = self.announces.get(peer)?;
                let addr = if a.port == 0 {
                    addr
                } else {
                    SocketAddr::new(addr.ip(), a.port)
                };
                Some(Ready {
                    peer: *peer,
                    addr,
                    key: a.key,
                })
            })
            .collect();
        if ready != self.ready {
            self.ready = ready;
            self.roster_version += 1;
            self.grant_version += 1;
        }
    }

    /// Takes a hand-off payload a client sent (a successor's announcement).
    pub fn on_payload(&mut self, peer: PeerId, bytes: &[u8]) -> Result<(), HandoffError> {
        let result = self.take(peer, bytes);
        if let Err(e) = &result {
            self.refused += 1;
            self.last_refusal = Some(e.code());
        }
        result
    }

    fn take(&mut self, peer: PeerId, bytes: &[u8]) -> Result<(), HandoffError> {
        let mut r = WireReader::new(bytes);
        match r.u8()? {
            KIND_ANNOUNCE => {
                let announce = Announce::decode(&mut r)?;
                if !self.peers.contains_key(&peer) {
                    return Err(HandoffError::NotAPlayer);
                }
                if self.announces.insert(peer, announce) != Some(announce) {
                    self.refresh();
                }
                Ok(())
            }
            other => Err(HandoffError::WrongKind(other)),
        }
    }

    fn roster_for(&self, peer: PeerId) -> Roster {
        Roster {
            epoch: self.epoch,
            version: self.roster_version,
            host_lost_ms: self.config.host_lost_ms,
            takeover_ms: self.config.takeover_ms,
            successors: self
                .ready
                .iter()
                .map(|r| RosterEntry {
                    peer: r.peer,
                    addr: r.addr,
                    key: r.key,
                    ticket: ticket_for(&successor_secret(&self.master, r.peer, &r.key), peer),
                })
                .collect(),
        }
    }

    /// The payloads to send now: a roster for every player that does not hold the current one
    /// and a grant for every successor that does not. Send each on a reliable channel; call
    /// [`Self::resend`] for a peer whose send failed.
    pub fn poll(&mut self) -> Vec<HandoffOut> {
        let mut outs = Vec::new();
        if self.roster_version == 0 {
            return outs;
        }
        let ready = self.ready.clone();
        for r in &ready {
            if self.sent_grant.get(&r.peer) != Some(&self.grant_version) {
                let grant = Grant {
                    epoch: self.epoch,
                    version: self.grant_version,
                    key: r.key,
                    secret: successor_secret(&self.master, r.peer, &r.key),
                    peers: self.peers.keys().copied().filter(|p| *p != r.peer).collect(),
                };
                outs.push(HandoffOut {
                    peer: r.peer,
                    bytes: grant.encode(),
                });
                self.sent_grant.insert(r.peer, self.grant_version);
                self.grants_sent += 1;
            }
        }
        let peers: Vec<PeerId> = self.peers.keys().copied().collect();
        for peer in peers {
            if self.sent_roster.get(&peer) != Some(&self.roster_version) {
                outs.push(HandoffOut {
                    peer,
                    bytes: self.roster_for(peer).encode(),
                });
                self.sent_roster.insert(peer, self.roster_version);
                self.rosters_sent += 1;
            }
        }
        outs
    }

    /// Forgets what `peer` was sent, so the next [`Self::poll`] sends its roster (and grant)
    /// again. For a payload the transport refused.
    pub fn resend(&mut self, peer: PeerId) {
        self.sent_roster.remove(&peer);
        self.sent_grant.remove(&peer);
    }

    /// Does the host's side on a server: reads the players and their addresses from the session
    /// and the successors from `journal`, and sends what [`Self::poll`] hands out on
    /// `Channel::GAME_RPC`. `wrap` puts a payload into the game's message type. Returns the bytes
    /// handed to the transport. Route the successors' announcements to [`Self::on_payload`].
    pub fn pump_server<G: NetGame, S: DatagramSocket>(
        &mut self,
        server: &mut NetServer<G, S>,
        journal: &JournalHost,
        wrap: impl Fn(Vec<u8>) -> G::Message,
    ) -> usize {
        let peers: Vec<(PeerId, SocketAddr)> = server
            .session()
            .peers()
            .filter(|p| p.role == PeerRole::Player)
            .map(|p| (p.id, p.addr))
            .collect();
        self.set_peers(&peers);
        self.set_successors(&journal.successors());
        let mut sent = 0;
        for out in self.poll() {
            let len = out.bytes.len();
            match server.send(out.peer, Channel::GAME_RPC, &wrap(out.bytes)) {
                Ok(()) => sent += len,
                Err(SendError::NotConnected) => {}
                Err(e) => {
                    tracing::debug!(peer = out.peer.0, error = %e, "hand-off payload held for the next pump");
                    self.resend(out.peer);
                }
            }
        }
        sent
    }

    /// True when this host has probably been replaced: it told its clients about a successor and
    /// every one of them is silent (`all_silent`: no player is connected and heard within the
    /// interruption threshold; `NetServer::session().peers().all(|p| p.interrupted)`). The game
    /// then stops hosting and comes back as a client with [`Self::comeback`].
    pub fn deposed(&self, all_silent: bool) -> bool {
        all_silent && !self.ready.is_empty() && !self.peers.is_empty()
    }

    /// The lost host's way back into its own match (spec 120 § 10.5): a client state machine that
    /// is already past the host's loss and resumes to the first successor, then the second, with
    /// the host's own tickets. The promoted successor holds the host's player dormant under a
    /// new `PeerId` ([`PromotionPlan::host_seat`]); the accept names it. `None` when no successor
    /// was ever announced to the clients.
    pub fn comeback(&self, now_ms: u64) -> Option<HandoffClient> {
        if self.ready.is_empty() {
            return None;
        }
        let roster = Roster {
            epoch: self.epoch,
            version: self.roster_version,
            host_lost_ms: self.config.host_lost_ms,
            takeover_ms: self.config.takeover_ms,
            successors: self
                .ready
                .iter()
                .map(|r| RosterEntry {
                    peer: r.peer,
                    addr: r.addr,
                    key: r.key,
                    ticket: ticket_for(&successor_secret(&self.master, r.peer, &r.key), PeerId::SERVER),
                })
                .collect(),
        };
        let mut client = HandoffClient::new(self.config);
        client.returning_host = true;
        client.roster = Some(roster);
        client.host_losses = 1;
        client.phase = Phase::Lost {
            at_ms: now_ms,
            step: 0,
            since_ms: now_ms,
            asked: false,
        };
        Some(client)
    }

    /// What `net.migration` reports for the host's hand-off.
    pub fn status(&self) -> HandoffStatus {
        HandoffStatus {
            role: "host".into(),
            state: if self.ready.is_empty() {
                "no_successor".into()
            } else {
                "ready".into()
            },
            epoch: self.epoch,
            roster_version: self.roster_version,
            successors: self
                .ready
                .iter()
                .enumerate()
                .map(|(rank, r)| HandoffSuccessorRow {
                    peer: r.peer.0,
                    rank: rank as u8,
                    addr: r.addr.to_string(),
                    key: r.key.to_string(),
                })
                .collect(),
            host_lost_ms: self.config.host_lost_ms,
            takeover_ms: self.config.takeover_ms,
            rosters_sent: self.rosters_sent,
            grants_sent: self.grants_sent,
            refused: self.refused,
            last_refusal: self.last_refusal.map(str::to_owned),
            ..HandoffStatus::default()
        }
    }

    /// Publishes [`Self::status`] under `label` for the agent method `net.migration`.
    pub fn publish(&self, label: &str, state: &mut NetAgentState) {
        state.publish_handoff(label, self.status());
    }
}

/// One successor in a `net.migration` hand-off report.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HandoffSuccessorRow {
    /// The successor's peer id on the host.
    pub peer: u32,
    /// 0 is the first successor.
    pub rank: u8,
    /// The address it serves on after a promotion.
    pub addr: String,
    /// The public key of the server it becomes (64 hex digits).
    pub key: String,
}

/// The hand-off part of `net.migration` for one published host or client.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HandoffStatus {
    /// `host`, `client`, `successor` (a client the roster lists), `promoted` (a successor that
    /// became the server) or `returning_host` (a lost host on its way back as a client).
    pub role: String,
    /// Host: `no_successor` or `ready`. Others: `watching`, `resuming`, `armed`, `promoted`,
    /// `resumed` or `abandoned`.
    pub state: String,
    /// The host generation of the roster held (1: the host that started the match).
    pub epoch: u32,
    /// The roster's version.
    pub roster_version: u32,
    /// The successors of the roster, first successor first.
    pub successors: Vec<HandoffSuccessorRow>,
    /// Silence after which the host counts as lost, milliseconds.
    pub host_lost_ms: u64,
    /// How long a successor has to answer, milliseconds.
    pub takeover_ms: u64,
    /// Client: its rank in the roster, when it is a successor.
    pub rank: Option<u8>,
    /// Successor: it holds the host's grant for its announced key.
    pub granted: bool,
    /// Times this endpoint saw its host lost.
    pub host_losses: u64,
    /// The last hand-off, milliseconds from the loss being seen to this endpoint being connected
    /// to the successor (or promoted).
    pub last_handoff_ms: Option<u64>,
    /// The rank of the successor the last hand-off ended on.
    pub last_handoff_rank: Option<u8>,
    /// Armed or promoted: players of the lost host it holds a place for (the lost host's own
    /// included).
    pub expected_peers: u32,
    /// Armed or promoted: how many of them have resumed.
    pub resumed_peers: u32,
    /// Promoted: a majority of the lost host's clients resumed.
    pub quorum: bool,
    /// Host: rosters sent.
    pub rosters_sent: u64,
    /// Host: grants sent.
    pub grants_sent: u64,
    /// Hand-off payloads refused.
    pub refused: u64,
    /// The code of the last refusal.
    pub last_refusal: Option<String>,
}

/// What a client knows about its link to the host, for [`HandoffClient::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostLink {
    /// When the host was last heard on the client's clock (`NetClient::last_heard_ms`).
    pub last_heard_ms: u64,
    /// The connection ended (the host shut down, or the transport gave up).
    pub ended: bool,
}

impl HostLink {
    /// The link of a connection that is gone (ended, or given up by the game).
    pub fn gone() -> Self {
        Self {
            last_heard_ms: 0,
            ended: true,
        }
    }
}

/// Where a client resumes to.
#[derive(Clone, PartialEq, Eq)]
pub struct ResumeTarget {
    /// The successor's rank in the roster (0: the first).
    pub rank: u8,
    /// The successor's peer id on the lost host.
    pub peer: PeerId,
    /// Its server's address.
    pub addr: SocketAddr,
    /// Its server's public key; the client pins it.
    pub server_key: ServerKey,
    /// The client's resume ticket for that server.
    pub ticket: [u8; RESUME_TICKET_BYTES],
    /// How long this successor has to answer, milliseconds.
    pub takeover_ms: u64,
}

impl std::fmt::Debug for ResumeTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The ticket is a credential: not printed.
        f.debug_struct("ResumeTarget")
            .field("rank", &self.rank)
            .field("peer", &self.peer)
            .field("addr", &self.addr)
            .field("server_key", &self.server_key)
            .finish_non_exhaustive()
    }
}

impl ResumeTarget {
    /// `base` set up for the resume: the ticket as the resume ticket, the successor's key pinned,
    /// a connect timeout of [`Self::takeover_ms`] so the attempt ends when the next successor is
    /// due, and handshake packets resent at least every [`HANDOFF_HANDSHAKE_RESEND_MS`] (the
    /// attempt lasts 3 s at most and every lost handshake packet is time the match stands
    /// still).
    pub fn client_config(&self, mut base: ClientConfig) -> ClientConfig {
        base.resume = Some(self.ticket);
        base.server_key = Some(self.server_key);
        base.connect_timeout_ms = self.takeover_ms;
        base.handshake_resend_ms = base.handshake_resend_ms.min(HANDOFF_HANDSHAKE_RESEND_MS);
        base
    }
}

/// What a promoted successor needs to open its server.
#[derive(Clone)]
pub struct PromotionPlan {
    /// The successor's rank (0: the first).
    pub rank: u8,
    /// The `PeerId` this client had on the lost host; its player keeps it.
    pub own_peer: PeerId,
    /// The new host's generation, for [`HandoffHost::with_epoch`].
    pub epoch: u32,
    /// The key pair every client pinned.
    pub identity: ServerIdentity,
    /// The port announced; 0 means the client's own socket (the address the lost host saw).
    pub port: u16,
    /// The players to hold dormant, each with the ticket it will present.
    pub adopt: Vec<(PeerId, [u8; RESUME_TICKET_BYTES])>,
    /// The `PeerId` the lost host's own player gets on this server: the lowest id above every
    /// player's. The game moves what `PeerId::SERVER` owned in the shadow world to it.
    pub host_seat: PeerId,
    /// The lost host's ticket.
    pub host_ticket: [u8; RESUME_TICKET_BYTES],
}

impl std::fmt::Debug for PromotionPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Tickets and the key pair are not printed.
        f.debug_struct("PromotionPlan")
            .field("rank", &self.rank)
            .field("own_peer", &self.own_peer)
            .field("epoch", &self.epoch)
            .field("port", &self.port)
            .field("adopt", &self.adopt.len())
            .field("host_seat", &self.host_seat)
            .finish_non_exhaustive()
    }
}

impl PromotionPlan {
    /// `base` set up for the promoted server: the announced key pair, a resume grace when `base`
    /// has none ([`HANDOFF_RESUME_GRACE_MS`]), and room for every player held.
    pub fn server_config(&self, mut base: ServerConfig) -> ServerConfig {
        base.identity = Some(self.identity.clone());
        if base.resume_grace_ms == 0 {
            base.resume_grace_ms = HANDOFF_RESUME_GRACE_MS;
        }
        // The players, the lost host's seat and this successor's own id.
        base.max_peers = base.max_peers.max(self.adopt.len() + 2);
        base
    }

    /// Holds every player of the lost host on `server` (a server made with
    /// [`Self::server_config`] that has received nothing yet): this successor's own id is kept,
    /// each other player and the lost host's seat become dormant peers with their tickets.
    /// Returns how many dormant peers were adopted.
    pub fn apply<G: NetGame, S: DatagramSocket>(
        &self,
        server: &mut NetServer<G, S>,
    ) -> Result<usize, AdoptError> {
        if !server.keep_peer_id(self.own_peer) {
            return Err(AdoptError::IdInUse);
        }
        for (peer, ticket) in &self.adopt {
            server.adopt_dormant(*peer, *ticket)?;
        }
        server.adopt_dormant(self.host_seat, self.host_ticket)?;
        Ok(self.adopt.len() + 1)
    }
}

/// What the game does next.
#[derive(Debug, Clone)]
pub enum HandoffAction {
    /// Become the server at once: promote the shadow world and open a server from the plan on
    /// the client's own socket. Only for a successor that announced no port
    /// ([`HandoffClient::with_port`]): it has one socket, so it cannot wait for other players
    /// before it gives its connection to the host up.
    Promote(Box<PromotionPlan>),
    /// Open a server from the plan on the announced port, hold every player
    /// ([`PromotionPlan::apply`]) and start it at the shadow's tick
    /// (`NetServer::start_at_tick` with `JournalSuccessor::applied_tick`). Run it like a server
    /// that simulates nothing: `receive`, `advance_tick` on the game's tick schedule (the clients
    /// that arrive sync to a running clock) and `flush`, and report each
    /// `ServerEvent::PeerResumed` with [`HandoffClient::peer_resumed`]. Keep the shadow world
    /// and the connection to the host.
    Arm(Box<PromotionPlan>),
    /// The armed server takes the match over: promote the shadow world, simulate on from the
    /// server's current tick (the ticks it counted while armed are not simulated) and drop the
    /// connection to the host.
    Commit,
    /// The host is heard again: drop the armed server.
    Disarm,
    /// Connect to this successor on a second socket (end an earlier attempt first). Keep the
    /// connection to the lost host until this one is accepted: if the host is heard again first,
    /// [`HandoffAction::StayWithHost`] follows.
    Resume(ResumeTarget),
    /// The host is heard again: drop the resume attempt and carry on with the host.
    StayWithHost,
    /// No successor answered: drop the attempt. The connection to the old host, if the game
    /// still holds it, carries on under the transport's own timeout and resume; a game that
    /// holds none ends the match.
    ReturnToHost,
}

/// A readout for the game's interface ("host lost", "reconnecting", "resumed").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffEvent {
    /// The host is lost.
    HostLost {
        /// How long it had been silent, milliseconds (0 when the connection ended).
        silent_ms: u64,
    },
    /// This client is resuming to a successor.
    Resuming {
        /// The successor's rank.
        rank: u8,
        /// The successor's peer id on the lost host.
        peer: PeerId,
    },
    /// A successor did not answer in time (or refused); the next one is tried.
    SuccessorSilent {
        /// Its rank.
        rank: u8,
    },
    /// It was this successor's turn, but it holds no promotable shadow or no grant.
    NotPromotable {
        /// Its rank.
        rank: u8,
    },
    /// This successor opened its server and waits for the players (or for its host).
    Armed {
        /// Its rank.
        rank: u8,
    },
    /// This successor became the server.
    Promoted {
        /// Its rank.
        rank: u8,
        /// Milliseconds from the loss being seen.
        after_ms: u64,
    },
    /// This client is connected to the successor.
    Resumed {
        /// The successor's rank.
        rank: u8,
        /// Milliseconds from the loss being seen.
        after_ms: u64,
    },
    /// The successor accepted this client under another `PeerId` than it had (its ticket was not
    /// known there: the place was given away or the grace ran out).
    PeerIdChanged {
        /// The id on the lost host.
        was: PeerId,
        /// The id now.
        now: PeerId,
    },
    /// No successor answered.
    NoSuccessor,
    /// The host was heard again before a successor accepted this client; it stays with the host.
    HostRestored {
        /// Milliseconds from the loss being seen.
        after_ms: u64,
    },
    /// Promoted server: a player of the lost host resumed.
    PeerResumed {
        /// The player.
        peer: PeerId,
        /// Milliseconds from the promotion.
        after_ms: u64,
    },
    /// Promoted server: a majority of the lost host's clients resumed.
    Quorum {
        /// Milliseconds from the promotion.
        after_ms: u64,
    },
    /// Promoted server: every client of the lost host resumed (its own player aside).
    AllResumed {
        /// Milliseconds from the promotion.
        after_ms: u64,
    },
    /// Promoted server: no majority within [`HandoffConfig::quorum_wait_ms`]. The lost host may
    /// still be serving the others; the game decides whether to go on.
    NoQuorum {
        /// Players that resumed.
        resumed: u32,
        /// Clients of the lost host this server holds a place for.
        expected: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// Connected to a host (or not connected yet).
    Watching,
    /// The host is lost; working through the roster.
    Lost {
        at_ms: u64,
        /// Index into the roster's successors.
        step: usize,
        /// When this step began.
        since_ms: u64,
        /// The step's action was handed out.
        asked: bool,
    },
    /// This successor's server is open and holds the players; the match is not taken over yet.
    Armed {
        at_ms: u64,
        step: usize,
        since_ms: u64,
        /// The lost host's clients (not its own seat).
        expected: BTreeSet<PeerId>,
        host_seat: PeerId,
        resumed: BTreeSet<PeerId>,
    },
    /// This endpoint is the server now.
    Hosting {
        since_ms: u64,
        /// The lost host's clients (not its own seat).
        expected: BTreeSet<PeerId>,
        host_seat: PeerId,
        resumed: BTreeSet<PeerId>,
        quorum: bool,
        all: bool,
        judged: bool,
    },
    /// Nobody answered.
    Abandoned {
        /// When the loss was seen.
        at_ms: u64,
    },
}

/// Whether `clients` of the lost host's `expected` clients, with the successor itself, are a
/// majority of them all.
fn majority(clients: usize, expected: usize) -> bool {
    (clients + 1) * 2 > expected + 1
}

/// A client's side of the hand-off. Every client holds one for the whole match.
pub struct HandoffClient {
    config: HandoffConfig,
    identity: Option<ServerIdentity>,
    port: u16,
    me: Option<PeerId>,
    appointed: bool,
    /// The announcement to send.
    out: Option<Vec<u8>>,
    roster: Option<Roster>,
    grant: Option<Grant>,
    phase: Phase,
    returning_host: bool,
    events: VecDeque<HandoffEvent>,
    host_losses: u64,
    last_handoff_ms: Option<u64>,
    last_handoff_rank: Option<u8>,
    /// The highest host generation seen; older rosters are refused.
    epoch_floor: u32,
    refused: u64,
    last_refusal: Option<&'static str>,
}

impl std::fmt::Debug for HandoffClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandoffClient")
            .field("me", &self.me)
            .field("phase", &self.phase)
            .field("roster", &self.roster)
            .finish_non_exhaustive()
    }
}

impl HandoffClient {
    /// A client that watches its host with `config`'s thresholds until a roster brings the
    /// host's.
    pub fn new(config: HandoffConfig) -> Self {
        Self {
            config: config.sanitized(),
            identity: None,
            port: 0,
            me: None,
            appointed: false,
            out: None,
            roster: None,
            grant: None,
            phase: Phase::Watching,
            returning_host: false,
            events: VecDeque::new(),
            host_losses: 0,
            last_handoff_ms: None,
            last_handoff_rank: None,
            epoch_floor: 0,
            refused: 0,
            last_refusal: None,
        }
    }

    /// Uses this key pair for the server this client may become, instead of a fresh random one
    /// made at its first appointment (deterministic tests, or a host with a long-lived key).
    pub fn with_identity(mut self, identity: ServerIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// The port this client would serve on as a successor: a second socket the game can bind,
    /// at the address the host sees this client at. With a port the successor arms first and
    /// takes the match over only when a majority of the players arrived
    /// ([`HandoffAction::Arm`]). 0 (the default) is its client socket: the promoted server takes
    /// that socket over, so the successor promotes at once ([`HandoffAction::Promote`]) and a
    /// successor in its own outage gives up its place on a host that is still there.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Sets this client's `PeerId` (`ClientEvent::Connected`).
    pub fn set_peer(&mut self, peer: PeerId) {
        self.me = Some(peer);
    }

    /// This client's `PeerId`.
    pub fn peer(&self) -> Option<PeerId> {
        self.me
    }

    /// Tells the client whether the journal appointed it (`JournalSuccessor::is_successor`); call
    /// it every frame. On an appointment the client makes its key pair and queues its
    /// announcement ([`Self::poll_out`]). False when the system's random source failed and no
    /// key pair could be made (this client then never promotes).
    pub fn set_appointed(&mut self, appointed: bool) -> bool {
        if appointed && !self.appointed {
            if self.identity.is_none() {
                self.identity = ServerIdentity::generate().ok();
            }
            if let Some(identity) = &self.identity {
                self.out = Some(
                    Announce {
                        key: identity.public(),
                        port: self.port,
                    }
                    .encode(),
                );
            }
        }
        self.appointed = appointed;
        self.identity.is_some() || !appointed
    }

    /// A payload for the host (the announcement); send it on a reliable channel.
    pub fn poll_out(&mut self) -> Option<Vec<u8>> {
        self.out.take()
    }

    /// The roster held.
    pub fn roster(&self) -> Option<&Roster> {
        self.roster.as_ref()
    }

    /// This client's rank in the roster, when it is one of its successors.
    pub fn rank(&self) -> Option<u8> {
        let me = self.me?;
        self.roster
            .as_ref()?
            .successors
            .iter()
            .position(|s| s.peer == me)
            .map(|i| i as u8)
    }

    /// True while the host is lost and the roster is being worked through.
    pub fn migrating(&self) -> bool {
        matches!(self.phase, Phase::Lost { .. })
    }

    /// True once this endpoint is the server.
    pub fn is_host(&self) -> bool {
        matches!(self.phase, Phase::Hosting { .. })
    }

    /// True while this successor's server is armed and the match is not taken over yet.
    pub fn is_armed(&self) -> bool {
        matches!(self.phase, Phase::Armed { .. })
    }

    /// The next readout event.
    pub fn poll_event(&mut self) -> Option<HandoffEvent> {
        self.events.pop_front()
    }

    /// Takes a hand-off payload the host sent (a roster or a grant).
    pub fn on_payload(&mut self, bytes: &[u8]) -> Result<(), HandoffError> {
        let result = self.take(bytes);
        if let Err(e) = &result {
            self.refused += 1;
            self.last_refusal = Some(e.code());
        }
        result
    }

    fn take(&mut self, bytes: &[u8]) -> Result<(), HandoffError> {
        let mut r = WireReader::new(bytes);
        match r.u8()? {
            KIND_ROSTER => {
                let roster = Roster::decode(&mut r)?;
                let held = self
                    .roster
                    .as_ref()
                    .map_or((self.epoch_floor, 0), |h| (h.epoch, h.version));
                if roster.epoch < self.epoch_floor || (roster.epoch, roster.version) < held {
                    return Err(HandoffError::Stale);
                }
                self.epoch_floor = roster.epoch;
                self.roster = Some(roster);
                Ok(())
            }
            KIND_GRANT => {
                let grant = Grant::decode(&mut r)?;
                let held = self
                    .grant
                    .as_ref()
                    .map_or((self.epoch_floor, 0), |h| (h.epoch, h.version));
                if grant.epoch < self.epoch_floor || (grant.epoch, grant.version) < held {
                    return Err(HandoffError::Stale);
                }
                self.grant = Some(grant);
                Ok(())
            }
            other => Err(HandoffError::WrongKind(other)),
        }
    }

    fn thresholds(&self) -> (u64, u64) {
        self.roster
            .as_ref()
            .map_or((self.config.host_lost_ms, self.config.takeover_ms), |r| {
                (r.host_lost_ms, r.takeover_ms)
            })
    }

    /// The plan of this successor's promotion, when it holds the grant for the key the roster
    /// lists it with.
    fn plan(&self, rank: usize) -> Option<PromotionPlan> {
        let me = self.me?;
        let roster = self.roster.as_ref()?;
        let identity = self.identity.as_ref()?;
        let grant = self.grant.as_ref()?;
        let row = roster.successors.get(rank)?;
        if row.peer != me
            || row.key != identity.public()
            || grant.key != row.key
            || grant.epoch != roster.epoch
        {
            return None;
        }
        let adopt: Vec<(PeerId, [u8; RESUME_TICKET_BYTES])> = grant
            .peers
            .iter()
            .filter(|p| **p != me && **p != PeerId::SERVER)
            .map(|p| (*p, ticket_for(&grant.secret, *p)))
            .collect();
        let top = adopt.iter().map(|(p, _)| p.0).max().unwrap_or(0).max(me.0);
        Some(PromotionPlan {
            rank: rank as u8,
            own_peer: me,
            epoch: roster.epoch.saturating_add(1),
            identity: identity.clone(),
            port: self.port,
            adopt,
            host_seat: PeerId(top.saturating_add(1)),
            host_ticket: ticket_for(&grant.secret, PeerId::SERVER),
        })
    }

    /// Drives the hand-off; call it every frame once [`Self::set_peer`] was called, with the state
    /// of the link to the host and whether this client's shadow world can be promoted
    /// (`JournalSuccessor::promotable`). Returns what the game does next, once per decision.
    ///
    /// While the roster is being worked through, keep passing the link of the connection to the
    /// lost host for as long as the game holds it (a host heard again cancels the hand-off for
    /// this client), and [`HostLink::gone`] once it does not. Report the outcome of a resume with
    /// [`Self::resumed`] or [`Self::resume_failed`].
    pub fn poll(&mut self, now_ms: u64, link: HostLink, promotable: bool) -> Option<HandoffAction> {
        if matches!(self.phase, Phase::Watching) {
            // Not connected to a host yet: nothing to lose.
            self.me?;
            let (host_lost_ms, _) = self.thresholds();
            let silent_ms = now_ms.saturating_sub(link.last_heard_ms);
            if !link.ended && silent_ms < host_lost_ms {
                return None;
            }
            self.host_losses += 1;
            self.events.push_back(HandoffEvent::HostLost {
                silent_ms: if link.ended { 0 } else { silent_ms },
            });
            self.phase = Phase::Lost {
                at_ms: now_ms,
                step: 0,
                since_ms: now_ms,
                asked: false,
            };
            return self.step(now_ms, promotable);
        }
        if let Phase::Armed {
            at_ms,
            step,
            expected,
            host_seat,
            resumed,
            ..
        } = &self.phase
        {
            let (at_ms, rank) = (*at_ms, *step as u8);
            let clients = resumed.iter().filter(|p| expected.contains(p)).count();
            // Others lost the host too: a majority of the players is here.
            let evidence = !expected.is_empty() && majority(clients, expected.len());
            if evidence || link.ended {
                let after_ms = now_ms.saturating_sub(at_ms);
                let all = clients == expected.len();
                self.phase = Phase::Hosting {
                    since_ms: now_ms,
                    expected: expected.clone(),
                    host_seat: *host_seat,
                    resumed: resumed.clone(),
                    quorum: evidence || expected.is_empty(),
                    all,
                    judged: false,
                };
                self.events.push_back(HandoffEvent::Promoted { rank, after_ms });
                if evidence {
                    self.events.push_back(HandoffEvent::Quorum { after_ms: 0 });
                }
                if all && evidence {
                    self.events.push_back(HandoffEvent::AllResumed { after_ms: 0 });
                }
                self.last_handoff_ms = Some(after_ms);
                self.last_handoff_rank = Some(rank);
                return Some(HandoffAction::Commit);
            }
            if link.last_heard_ms > at_ms {
                // The silence was this successor's own outage.
                self.phase = Phase::Watching;
                self.events.push_back(HandoffEvent::HostRestored {
                    after_ms: now_ms.saturating_sub(at_ms),
                });
                return Some(HandoffAction::Disarm);
            }
            return None;
        }
        // The host is heard again after the loss was seen: the silence was this client's own.
        if let Phase::Lost { at_ms, .. } | Phase::Abandoned { at_ms } = self.phase
            && !self.returning_host
            && !link.ended
            && link.last_heard_ms > at_ms
        {
            let attempt = matches!(self.phase, Phase::Lost { asked: true, .. });
            self.phase = Phase::Watching;
            self.events.push_back(HandoffEvent::HostRestored {
                after_ms: now_ms.saturating_sub(at_ms),
            });
            return attempt.then_some(HandoffAction::StayWithHost);
        }
        if matches!(self.phase, Phase::Lost { .. }) {
            return self.step(now_ms, promotable);
        }
        if let Phase::Hosting {
            since_ms,
            expected,
            resumed,
            quorum,
            judged,
            ..
        } = &mut self.phase
            && !*quorum
            && !*judged
            && now_ms.saturating_sub(*since_ms) >= self.config.quorum_wait_ms
        {
            *judged = true;
            self.events.push_back(HandoffEvent::NoQuorum {
                resumed: resumed.len() as u32,
                expected: expected.len() as u32,
            });
        }
        None
    }

    fn step(&mut self, now_ms: u64, promotable: bool) -> Option<HandoffAction> {
        let (_, takeover_ms) = self.thresholds();
        loop {
            let Phase::Lost {
                at_ms,
                step,
                since_ms,
                asked,
            } = self.phase.clone()
            else {
                return None;
            };
            let target = self.roster.as_ref().and_then(|r| r.successors.get(step)).cloned();
            let Some(target) = target else {
                self.phase = Phase::Abandoned { at_ms };
                self.events.push_back(HandoffEvent::NoSuccessor);
                return Some(HandoffAction::ReturnToHost);
            };
            let rank = step as u8;
            if self.me == Some(target.peer) && !self.returning_host {
                // This client's turn to become the server.
                match self.plan(step).filter(|_| promotable) {
                    Some(plan) if plan.port != 0 => {
                        self.events.push_back(HandoffEvent::Armed { rank });
                        self.phase = Phase::Armed {
                            at_ms,
                            step,
                            since_ms: now_ms,
                            expected: plan.adopt.iter().map(|(p, _)| *p).collect(),
                            host_seat: plan.host_seat,
                            resumed: BTreeSet::new(),
                        };
                        return Some(HandoffAction::Arm(Box::new(plan)));
                    }
                    Some(plan) => {
                        let after_ms = now_ms.saturating_sub(at_ms);
                        self.events.push_back(HandoffEvent::Promoted { rank, after_ms });
                        self.last_handoff_ms = Some(after_ms);
                        self.last_handoff_rank = Some(rank);
                        self.phase = Phase::Hosting {
                            since_ms: now_ms,
                            expected: plan.adopt.iter().map(|(p, _)| *p).collect(),
                            host_seat: plan.host_seat,
                            resumed: BTreeSet::new(),
                            quorum: plan.adopt.is_empty(),
                            all: plan.adopt.is_empty(),
                            judged: false,
                        };
                        return Some(HandoffAction::Promote(Box::new(plan)));
                    }
                    None => {
                        self.events.push_back(HandoffEvent::NotPromotable { rank });
                        self.phase = Phase::Lost {
                            at_ms,
                            step: step + 1,
                            since_ms: now_ms,
                            asked: false,
                        };
                        continue;
                    }
                }
            }
            if !asked {
                self.phase = Phase::Lost {
                    at_ms,
                    step,
                    since_ms,
                    asked: true,
                };
                self.events.push_back(HandoffEvent::Resuming {
                    rank,
                    peer: target.peer,
                });
                return Some(HandoffAction::Resume(ResumeTarget {
                    rank,
                    peer: target.peer,
                    addr: target.addr,
                    server_key: target.key,
                    ticket: target.ticket,
                    takeover_ms,
                }));
            }
            if now_ms.saturating_sub(since_ms) < takeover_ms {
                return None;
            }
            self.events.push_back(HandoffEvent::SuccessorSilent { rank });
            self.phase = Phase::Lost {
                at_ms,
                step: step + 1,
                since_ms: now_ms,
                asked: false,
            };
        }
    }

    /// The resume attempt connected (`ClientEvent::Connected` on the new connection): the match
    /// goes on under the successor, and this client watches its new host. The roster and grant
    /// of the lost host are dropped; the new host sends its own.
    pub fn resumed(&mut self, now_ms: u64, peer: PeerId) {
        let Phase::Lost { at_ms, step, .. } = self.phase else {
            return;
        };
        let after_ms = now_ms.saturating_sub(at_ms);
        let rank = step as u8;
        self.events.push_back(HandoffEvent::Resumed { rank, after_ms });
        if let Some(was) = self.me
            && was != peer
        {
            self.events
                .push_back(HandoffEvent::PeerIdChanged { was, now: peer });
        }
        self.me = Some(peer);
        self.last_handoff_ms = Some(after_ms);
        self.last_handoff_rank = Some(rank);
        self.epoch_floor = self
            .roster
            .as_ref()
            .map_or(self.epoch_floor, |r| r.epoch.saturating_add(1));
        self.roster = None;
        self.grant = None;
        self.appointed = false;
        self.returning_host = false;
        self.phase = Phase::Watching;
    }

    /// The resume attempt ended without a connection (denied, or its connect timeout): the next
    /// successor is tried at the next [`Self::poll`] without waiting out the rest of the 3 s.
    pub fn resume_failed(&mut self, now_ms: u64) {
        if let Phase::Lost {
            at_ms,
            step,
            asked: true,
            ..
        } = self.phase
        {
            self.events
                .push_back(HandoffEvent::SuccessorSilent { rank: step as u8 });
            self.phase = Phase::Lost {
                at_ms,
                step: step + 1,
                since_ms: now_ms,
                asked: false,
            };
        }
    }

    /// Armed or promoted server: a player resumed (`ServerEvent::PeerResumed`). Raises
    /// [`HandoffEvent::PeerResumed`], and on a promoted server [`HandoffEvent::Quorum`] and
    /// [`HandoffEvent::AllResumed`] when they are reached. An armed server commits at the next
    /// [`Self::poll`] once a majority is there.
    pub fn peer_resumed(&mut self, now_ms: u64, peer: PeerId) {
        if let Phase::Armed {
            since_ms,
            expected,
            host_seat,
            resumed,
            ..
        } = &mut self.phase
        {
            // Counted; the next `poll` commits when this made a majority.
            if (expected.contains(&peer) || *host_seat == peer) && resumed.insert(peer) {
                let after_ms = now_ms.saturating_sub(*since_ms);
                self.events
                    .push_back(HandoffEvent::PeerResumed { peer, after_ms });
            }
            return;
        }
        let Phase::Hosting {
            since_ms,
            expected,
            host_seat,
            resumed,
            quorum,
            all,
            ..
        } = &mut self.phase
        else {
            return;
        };
        if !(expected.contains(&peer) || *host_seat == peer) || !resumed.insert(peer) {
            return;
        }
        let after_ms = now_ms.saturating_sub(*since_ms);
        self.events
            .push_back(HandoffEvent::PeerResumed { peer, after_ms });
        let clients = resumed.iter().filter(|p| expected.contains(p)).count();
        // This successor counts as one of the lost host's clients that is here.
        if !*quorum && majority(clients, expected.len()) {
            *quorum = true;
            self.events.push_back(HandoffEvent::Quorum { after_ms });
        }
        if !*all && clients == expected.len() {
            *all = true;
            self.events.push_back(HandoffEvent::AllResumed { after_ms });
        }
    }

    /// What `net.migration` reports for this endpoint's hand-off.
    pub fn status(&self) -> HandoffStatus {
        let (host_lost_ms, takeover_ms) = self.thresholds();
        let rank = self.rank();
        let (role, state) = match &self.phase {
            Phase::Hosting { .. } => ("promoted", "promoted"),
            phase => (
                if self.returning_host {
                    "returning_host"
                } else if rank.is_some() {
                    "successor"
                } else {
                    "client"
                },
                match phase {
                    Phase::Watching if self.last_handoff_ms.is_some() => "resumed",
                    Phase::Watching => "watching",
                    Phase::Lost { .. } => "resuming",
                    Phase::Armed { .. } => "armed",
                    _ => "abandoned",
                },
            ),
        };
        let (expected_peers, resumed_peers, quorum) = match &self.phase {
            Phase::Hosting {
                expected,
                resumed,
                quorum,
                ..
            } => (expected.len() as u32 + 1, resumed.len() as u32, *quorum),
            Phase::Armed {
                expected, resumed, ..
            } => (expected.len() as u32 + 1, resumed.len() as u32, false),
            _ => (0, 0, false),
        };
        HandoffStatus {
            role: role.into(),
            state: state.into(),
            epoch: self.roster.as_ref().map_or(self.epoch_floor, |r| r.epoch),
            roster_version: self.roster.as_ref().map_or(0, |r| r.version),
            successors: self.roster.as_ref().map_or_else(Vec::new, |r| {
                r.successors
                    .iter()
                    .enumerate()
                    .map(|(rank, s)| HandoffSuccessorRow {
                        peer: s.peer.0,
                        rank: rank as u8,
                        addr: s.addr.to_string(),
                        key: s.key.to_string(),
                    })
                    .collect()
            }),
            host_lost_ms,
            takeover_ms,
            rank,
            granted: rank.is_some_and(|r| self.plan(usize::from(r)).is_some()),
            host_losses: self.host_losses,
            last_handoff_ms: self.last_handoff_ms,
            last_handoff_rank: self.last_handoff_rank,
            expected_peers,
            resumed_peers,
            quorum,
            rosters_sent: 0,
            grants_sent: 0,
            refused: self.refused,
            last_refusal: self.last_refusal.map(str::to_owned),
        }
    }

    /// Publishes [`Self::status`] under `label` for the agent method `net.migration`.
    pub fn publish(&self, label: &str, state: &mut NetAgentState) {
        state.publish_handoff(label, self.status());
    }
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
