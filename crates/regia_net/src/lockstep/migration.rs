// SPDX-License-Identifier: MPL-2.0
//! Host migration for lockstep matches: when the host quits, the match continues (spec 120
//! § 10.5; ADR-8809).
//!
//! Every seat keeps a frame cache (the last [`LockstepConfig::frame_log_ticks`] frame bodies, 600
//! by default, with the delta base before them). The host distributes a [`MigrationRoster`] at
//! join over the sealed link (each seat's Noise static key, its address and a per-match migration
//! secret) and an [`RttMatrix`] every few seconds. When the host has been silent for
//! [`MigrationConfig::migrate_after_ms`], every seat ranks the survivors the same way
//! ([`rank_survivors`]: the lowest mean round trip to the others, ties by the lowest seat id),
//! and the winner listens on the agreed port or one of the next 15; the others connect to it with
//! its static key pinned, prove they are seats of the match, send the frames it lacks and their
//! next tick; the winner waits for every survivor or [`MigrationConfig::hello_wait_ms`] and
//! resumes the match at the **highest** next tick, so nobody steps back. If the winner does not
//! answer within [`MigrationConfig::next_candidate_after_ms`], seats move to the next ranked.
//!
//! The module is sans-IO like the rest of lockstep: a [`HostMigration`] per seat takes the seat,
//! the time and the messages that arrived, and hands out [`MigrationAction`]s (listen, connect,
//! send) and [`MigrationEvent`]s (`HostLost`, `Electing`, `Resuming { in_ticks }`).
//!
//! Every migration message carries the match id, the migration epoch and the sender's seat and
//! is authenticated with a keyed BLAKE3 MAC under the migration secret: a process that is not a
//! seat of this match can neither win the election nor join the re-hosted match.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::stamp::Stamper;
use super::wire::{FrameState, MAX_FRAMES_PER_PAYLOAD, MAX_SEATS};
use super::{LockstepConfig, LockstepError, LockstepHost, LockstepSeat, LockstepStatus, SeatId};
use crate::crypto::ServerKey;
use crate::payload::NetGame;
use crate::wire::{WireError, WireReader, WireWriter};

/// Ports the winner tries for the re-host listener: the agreed port and the next 15.
pub const MIGRATION_PORT_SPAN: u16 = 16;
/// How often the host shares its round-trip matrix, milliseconds (and at once when the seats
/// change).
pub const RTT_SHARE_MS: u64 = 3_000;
/// Lockstep stream payload: the roster (host to seat).
pub(crate) const KIND_ROSTER: u8 = 0x14;
/// Lockstep stream payload: the round-trip matrix (host to seat).
pub(crate) const KIND_RTT: u8 = 0x15;
const KIND_CLAIM: u8 = 0x30;
const KIND_ELECT: u8 = 0x31;
const KIND_HELLO_FRAMES: u8 = 0x32;
const KIND_HELLO: u8 = 0x33;
const KIND_RESUME: u8 = 0x34;
/// Bytes of the MAC at the end of a migration message.
const MAC_BYTES: usize = 32;
/// A round trip nobody measured, in the matrix.
const RTT_UNKNOWN: u16 = u16::MAX;
/// A silent seat, in the matrix encoding.
const RTT_SILENT: u16 = u16::MAX - 1;
/// The largest round trip the matrix carries, milliseconds.
pub(crate) const RTT_MAX_MS: u16 = u16::MAX - 2;
/// What an unknown round trip counts as in the ranking, milliseconds.
const UNKNOWN_RTT_PENALTY_MS: u64 = 60_000;
/// Frame bodies one hello part carries at most, bytes.
const HELLO_BATCH_BYTES: usize = 8 * 1024;
/// A failed connection to the winner is tried again after this long, milliseconds.
const CONNECT_RETRY_MS: u64 = 250;
const MAC_CONTEXT: &str = "regia.lockstep.migration 2026-11-19 message mac";
const TOKEN_CONTEXT: &str = "regia.lockstep.migration 2026-11-19 resume token";

/// Host-migration settings (spec 120 § 10.5). Every seat of a match uses the same values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MigrationConfig {
    /// How long the host has to be silent before the seats elect a new one, milliseconds.
    pub migrate_after_ms: u64,
    /// How long the winner waits for every survivor's hello before it resumes without the
    /// missing ones, milliseconds.
    pub hello_wait_ms: u64,
    /// How long a seat waits for the winner's answer before it moves to the next ranked seat,
    /// milliseconds.
    pub next_candidate_after_ms: u64,
    /// How far ahead of the resume decision the first new frame is due, milliseconds: the
    /// countdown of [`MigrationEvent::Resuming`], and the time the frames a seat lacks have to
    /// reach it.
    pub resume_lead_ms: u64,
}

impl Default for MigrationConfig {
    fn default() -> Self {
        Self {
            migrate_after_ms: 3_000,
            hello_wait_ms: 15_000,
            next_candidate_after_ms: 3_000,
            resume_lead_ms: 1_000,
        }
    }
}

/// One seat of a [`MigrationRoster`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RosterSeat {
    /// The seat.
    pub seat: SeatId,
    /// The seat's Noise static public key: the key a re-host listener on this seat proves, and
    /// the key the others pin when they connect to it.
    pub key: ServerKey,
    /// The seat's address as the host sees it (the re-host listener's address), when known.
    pub ip: Option<IpAddr>,
    /// The seat id's generation: resume tokens of earlier players of the seat stop working. The
    /// host fills it in when it shares the roster.
    pub generation: u32,
}

impl std::fmt::Debug for RosterSeat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RosterSeat")
            .field("seat", &self.seat)
            .field("key", &self.key)
            .field("ip", &self.ip)
            .field("generation", &self.generation)
            .finish()
    }
}

/// What every seat needs to re-host the match (spec 120 § 10.5, authenticated election). The host
/// shares it with [`LockstepHost::share_roster`] over the sealed link; it is never sent in the
/// clear and never logged (its `Debug` leaves the secret out).
#[derive(Clone, PartialEq, Eq)]
pub struct MigrationRoster {
    /// The match: messages of another match are refused.
    pub match_id: u64,
    /// The per-match migration secret: it keys the MACs of every migration message and the
    /// resume tokens.
    pub secret: [u8; 32],
    /// The agreed re-host port; the winner tries it and the next 15.
    pub port: u16,
    /// The seat whose process hosts the match now (`None`: a dedicated host process).
    pub host_seat: Option<SeatId>,
    /// Every seat of the match.
    pub seats: Vec<RosterSeat>,
}

impl std::fmt::Debug for MigrationRoster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MigrationRoster")
            .field("match_id", &self.match_id)
            .field("port", &self.port)
            .field("host_seat", &self.host_seat)
            .field("seats", &self.seats)
            .finish_non_exhaustive()
    }
}

impl MigrationRoster {
    /// A roster with a fresh secret from the operating system's random source; `None` when it has
    /// none. Add the seats, then [`LockstepHost::share_roster`].
    pub fn generate(match_id: u64, port: u16, host_seat: Option<SeatId>) -> Option<Self> {
        // det-scan: allow(os_random): the migration secret keys MACs and resume tokens; it never enters a frame or a hash
        let secret = crate::crypto::os_random::<32>()?;
        Some(Self {
            match_id,
            secret,
            port,
            host_seat,
            seats: Vec::new(),
        })
    }

    /// The roster entry of `seat`.
    pub fn seat(&self, seat: SeatId) -> Option<&RosterSeat> {
        self.seats.iter().find(|s| s.seat == seat)
    }

    /// The addresses a re-host listener on `seat` may be at: its address with the agreed port
    /// and the next 15.
    pub fn listen_addrs(&self, seat: SeatId) -> Vec<SocketAddr> {
        let Some(ip) = self.seat(seat).and_then(|s| s.ip) else {
            return Vec::new();
        };
        listen_ports(self.port)
            .into_iter()
            .map(|port| SocketAddr::new(ip, port))
            .collect()
    }

    fn mac_key(&self) -> [u8; 32] {
        blake3::derive_key(MAC_CONTEXT, &self.secret)
    }
}

/// The agreed port and the next 15 (wrapping below 65536, port 0 skipped).
pub fn listen_ports(port: u16) -> Vec<u16> {
    (0..MIGRATION_PORT_SPAN)
        .map(|i| port.wrapping_add(i))
        .filter(|p| *p != 0)
        .collect()
}

/// The resume token of `seat` under a match secret: any host of the match computes the same one,
/// so a seat (the old host too) can come back to a re-hosted match with it.
pub(crate) fn resume_token(secret: &[u8; 32], seat: SeatId, generation: u32) -> u64 {
    let key = blake3::derive_key(TOKEN_CONTEXT, secret);
    let mut msg = [0u8; 6];
    msg[..2].copy_from_slice(&seat.0.to_le_bytes());
    msg[2..].copy_from_slice(&generation.to_le_bytes());
    let hash = blake3::keyed_hash(&key, &msg);
    let mut out = [0u8; 8];
    out.copy_from_slice(&hash.as_bytes()[..8]);
    u64::from_le_bytes(out)
}

/// The round trips the host measured, shared with every seat (spec 120 § 10.5): each seat's
/// round trip to the host and, when the game measured them, round trips between seats. Its keys
/// are the seats of the match when it was shared: the election's candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RttMatrix {
    /// Bumped each time the host shares a new matrix: when two seats hold different ones, the
    /// newer one decides.
    pub version: u32,
    /// Each seat's round trip to the host, milliseconds (`None`: not measured yet).
    pub host_rtt_ms: BTreeMap<SeatId, Option<u16>>,
    /// Round trips between two seats, milliseconds, keyed by `(lower, higher)` seat.
    pub pair_rtt_ms: BTreeMap<(SeatId, SeatId), u16>,
    /// Seats the host had not heard for its interruption threshold (or never heard) when it
    /// shared the matrix: they rank after every other seat and the winner does not wait for
    /// their hello (a silent seat may still say hello in time and resume with the others).
    #[serde(default)]
    pub silent: BTreeSet<SeatId>,
}

impl RttMatrix {
    /// The round trip between `a` and `b`: measured, or through the host (the sum of both round
    /// trips to it) when only those are known.
    pub fn rtt(&self, a: SeatId, b: SeatId) -> Option<u64> {
        let key = (a.min(b), a.max(b));
        if let Some(ms) = self.pair_rtt_ms.get(&key) {
            return Some(u64::from(*ms));
        }
        let ra = (*self.host_rtt_ms.get(&a)?)?;
        let rb = (*self.host_rtt_ms.get(&b)?)?;
        Some(u64::from(ra) + u64::from(rb))
    }

    /// The seats the matrix lists.
    pub fn seats(&self) -> impl Iterator<Item = SeatId> + '_ {
        self.host_rtt_ms.keys().copied()
    }

    fn encode_into(&self, w: &mut WireWriter) {
        w.u32(self.version);
        w.u16(self.host_rtt_ms.len().min(MAX_SEATS) as u16);
        for (seat, ms) in self.host_rtt_ms.iter().take(MAX_SEATS) {
            w.u16(seat.0);
            let code = if self.silent.contains(seat) {
                RTT_SILENT
            } else {
                ms.map_or(RTT_UNKNOWN, |ms| ms.min(RTT_MAX_MS))
            };
            w.u16(code);
        }
        w.u16(self.pair_rtt_ms.len().min(MAX_SEATS) as u16);
        for ((a, b), ms) in self.pair_rtt_ms.iter().take(MAX_SEATS) {
            w.u16(a.0);
            w.u16(b.0);
            w.u16(*ms);
        }
    }

    fn decode_from(r: &mut WireReader<'_>) -> Result<Self, MigrationError> {
        let version = r.u32()?;
        let n = usize::from(r.u16()?);
        if n > MAX_SEATS || n * 4 > r.remaining() {
            return Err(MigrationError::Malformed("matrix seat count"));
        }
        let mut host_rtt_ms = BTreeMap::new();
        let mut silent = BTreeSet::new();
        for _ in 0..n {
            let seat = SeatId(r.u16()?);
            let ms = r.u16()?;
            if ms == RTT_SILENT {
                silent.insert(seat);
            }
            host_rtt_ms.insert(seat, (ms <= RTT_MAX_MS).then_some(ms));
        }
        let m = usize::from(r.u16()?);
        if m > MAX_SEATS || m * 6 > r.remaining() {
            return Err(MigrationError::Malformed("matrix pair count"));
        }
        let mut pair_rtt_ms = BTreeMap::new();
        for _ in 0..m {
            let a = SeatId(r.u16()?);
            let b = SeatId(r.u16()?);
            let ms = r.u16()?;
            pair_rtt_ms.insert((a.min(b), a.max(b)), ms);
        }
        Ok(Self {
            version,
            host_rtt_ms,
            pair_rtt_ms,
            silent,
        })
    }
}

/// The survivors in election order (spec 120 § 10.5): the best connected first (the lowest sum,
/// hence mean, of round trips to the other survivors; an unknown round trip counts as 60 s), ties
/// by the lowest seat id; seats the matrix lists silent come after all others. Every seat that
/// holds the same matrix computes the same order.
pub fn rank_survivors(matrix: &RttMatrix, survivors: &BTreeSet<SeatId>) -> Vec<SeatId> {
    let mut scored: Vec<(bool, u64, SeatId)> = survivors
        .iter()
        .map(|&seat| {
            let sum = survivors
                .iter()
                .filter(|other| **other != seat)
                .map(|other| matrix.rtt(seat, *other).unwrap_or(UNKNOWN_RTT_PENALTY_MS))
                .sum::<u64>();
            (matrix.silent.contains(&seat), sum, seat)
        })
        .collect();
    scored.sort_unstable();
    scored.into_iter().map(|(_, _, seat)| seat).collect()
}

pub(crate) fn encode_roster(roster: &MigrationRoster) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(64 + roster.seats.len() * 48);
    w.u8(KIND_ROSTER);
    w.u64(roster.match_id);
    w.raw(&roster.secret);
    w.u16(roster.port);
    w.u32(roster.host_seat.map_or(0, |s| u32::from(s.0) + 1));
    w.u16(roster.seats.len().min(MAX_SEATS) as u16);
    for s in roster.seats.iter().take(MAX_SEATS) {
        w.u16(s.seat.0);
        w.raw(&s.key.0);
        match s.ip {
            None => w.u8(0),
            Some(IpAddr::V4(v4)) => {
                w.u8(4);
                w.raw(&v4.octets());
            }
            Some(IpAddr::V6(v6)) => {
                w.u8(6);
                w.raw(&v6.octets());
            }
        }
        w.u32(s.generation);
    }
    w.finish()
}

pub(crate) fn decode_roster(bytes: &[u8]) -> Result<MigrationRoster, MigrationError> {
    let mut r = WireReader::new(bytes);
    if r.u8()? != KIND_ROSTER {
        return Err(MigrationError::Malformed("not a roster"));
    }
    let match_id = r.u64()?;
    let secret = r.array::<32>()?;
    let port = r.u16()?;
    let host_seat = match r.u32()? {
        0 => None,
        n => Some(SeatId(
            u16::try_from(n - 1).map_err(|_| MigrationError::Malformed("roster host seat"))?,
        )),
    };
    let n = usize::from(r.u16()?);
    if n > MAX_SEATS || n * 39 > r.remaining() {
        return Err(MigrationError::Malformed("roster seat count"));
    }
    let mut seats = Vec::with_capacity(n);
    for _ in 0..n {
        let seat = SeatId(r.u16()?);
        let key = ServerKey(r.array::<32>()?);
        let ip = match r.u8()? {
            0 => None,
            4 => Some(IpAddr::V4(Ipv4Addr::from(r.array::<4>()?))),
            6 => Some(IpAddr::V6(Ipv6Addr::from(r.array::<16>()?))),
            _ => return Err(MigrationError::Malformed("roster address")),
        };
        let generation = r.u32()?;
        seats.push(RosterSeat {
            seat,
            key,
            ip,
            generation,
        });
    }
    r.finish()?;
    Ok(MigrationRoster {
        match_id,
        secret,
        port,
        host_seat,
        seats,
    })
}

pub(crate) fn encode_rtt(matrix: &RttMatrix) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(16 + matrix.host_rtt_ms.len() * 4);
    w.u8(KIND_RTT);
    matrix.encode_into(&mut w);
    w.finish()
}

pub(crate) fn decode_rtt(bytes: &[u8]) -> Result<RttMatrix, MigrationError> {
    let mut r = WireReader::new(bytes);
    if r.u8()? != KIND_RTT {
        return Err(MigrationError::Malformed("not a matrix"));
    }
    let matrix = RttMatrix::decode_from(&mut r)?;
    r.finish()?;
    Ok(matrix)
}

/// Why a migration message was refused. [`MigrationError::code`] is the agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MigrationError {
    /// The bytes do not decode as a migration message.
    #[error("malformed migration message: {0}")]
    Malformed(&'static str),
    /// No roster has arrived from the host: this seat cannot check or send migration messages.
    #[error("no migration roster: the host never shared one")]
    NoRoster,
    /// The message names another match, or its MAC does not verify under the migration secret.
    #[error("the migration message is not authentic for this match")]
    BadMac,
    /// The sender is not a seat of this match.
    #[error("{0} is not a seat of this match")]
    NotMember(SeatId),
    /// The message belongs to another migration (an earlier one, or one this seat missed).
    #[error("migration epoch {got} where {want} was expected")]
    StaleEpoch {
        /// The message's epoch.
        got: u32,
        /// This seat's epoch.
        want: u32,
    },
    /// The message does not fit what this seat is doing (an election message while it watches a
    /// live host, a hello to a seat that does not lead, an answer from a seat it does not follow).
    #[error("unexpected migration message: {0}")]
    Unexpected(&'static str),
}

impl MigrationError {
    /// The agent-protocol error code (lowercase ascii, digits and `_`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "migration_malformed",
            Self::NoRoster => "migration_no_roster",
            Self::BadMac => "migration_bad_mac",
            Self::NotMember(_) => "migration_not_member",
            Self::StaleEpoch { .. } => "migration_stale_epoch",
            Self::Unexpected(_) => "migration_unexpected",
        }
    }
}

impl From<WireError> for MigrationError {
    fn from(_: WireError) -> Self {
        Self::Malformed("truncated or invalid field")
    }
}

/// A migration message's body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Body {
    /// A seat that follows the receiver: it holds this matrix.
    Claim { matrix: RttMatrix },
    /// The winner answers a claim: frames from `next` on are what it lacks.
    Elect { next: u64, matrix: RttMatrix },
    /// Frames the winner lacks, from `base_tick` on.
    HelloFrames { base_tick: u64, bodies: Vec<Vec<u8>> },
    /// The seat's next tick: every frame before it arrived.
    Hello { next: u64 },
    /// The match resumes at `tick`, due `in_ticks` after the winner decided.
    Resume { tick: u64, in_ticks: u32 },
}

/// A decoded, authenticated migration message.
#[derive(Debug)]
struct Msg {
    epoch: u32,
    from: SeatId,
    body: Body,
}

fn kind_of(body: &Body) -> u8 {
    match body {
        Body::Claim { .. } => KIND_CLAIM,
        Body::Elect { .. } => KIND_ELECT,
        Body::HelloFrames { .. } => KIND_HELLO_FRAMES,
        Body::Hello { .. } => KIND_HELLO,
        Body::Resume { .. } => KIND_RESUME,
    }
}

fn seal(roster: &MigrationRoster, epoch: u32, from: SeatId, body: &Body) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(64);
    w.u8(kind_of(body));
    w.u64(roster.match_id);
    w.u32(epoch);
    w.u16(from.0);
    match body {
        Body::Claim { matrix } => matrix.encode_into(&mut w),
        Body::Elect { next, matrix } => {
            w.u64(*next);
            matrix.encode_into(&mut w);
        }
        Body::HelloFrames { base_tick, bodies } => {
            w.u64(*base_tick);
            w.u16(bodies.len() as u16);
            for body in bodies {
                w.u32(body.len() as u32);
                w.raw(body);
            }
        }
        Body::Hello { next } => w.u64(*next),
        Body::Resume { tick, in_ticks } => {
            w.u64(*tick);
            w.u32(*in_ticks);
        }
    }
    let mut out = w.finish();
    let mac = blake3::keyed_hash(&roster.mac_key(), &out);
    out.extend_from_slice(mac.as_bytes());
    out
}

fn open(roster: &MigrationRoster, bytes: &[u8], max_body: usize) -> Result<Msg, MigrationError> {
    let Some(split) = bytes.len().checked_sub(MAC_BYTES) else {
        return Err(MigrationError::Malformed("shorter than its MAC"));
    };
    let (signed, mac) = bytes.split_at(split);
    let mut tag = [0u8; MAC_BYTES];
    tag.copy_from_slice(mac);
    // `blake3::Hash` compares in constant time.
    if blake3::keyed_hash(&roster.mac_key(), signed) != blake3::Hash::from(tag) {
        return Err(MigrationError::BadMac);
    }
    let mut r = WireReader::new(signed);
    let kind = r.u8()?;
    if r.u64()? != roster.match_id {
        return Err(MigrationError::BadMac);
    }
    let epoch = r.u32()?;
    let from = SeatId(r.u16()?);
    let body = match kind {
        KIND_CLAIM => Body::Claim {
            matrix: RttMatrix::decode_from(&mut r)?,
        },
        KIND_ELECT => Body::Elect {
            next: r.u64()?,
            matrix: RttMatrix::decode_from(&mut r)?,
        },
        KIND_HELLO_FRAMES => {
            let base_tick = r.u64()?;
            let n = usize::from(r.u16()?);
            if n > MAX_FRAMES_PER_PAYLOAD || n * 4 > r.remaining() {
                return Err(MigrationError::Malformed("hello frame count"));
            }
            let mut bodies = Vec::with_capacity(n);
            for _ in 0..n {
                let len = r.u32()? as usize;
                if len > max_body {
                    return Err(MigrationError::Malformed("hello frame length"));
                }
                bodies.push(r.take(len)?.to_vec());
            }
            Body::HelloFrames { base_tick, bodies }
        }
        KIND_HELLO => Body::Hello { next: r.u64()? },
        KIND_RESUME => Body::Resume {
            tick: r.u64()?,
            in_ticks: r.u32()?,
        },
        _ => return Err(MigrationError::Malformed("migration message kind")),
    };
    r.finish()?;
    if roster.seat(from).is_none() {
        return Err(MigrationError::NotMember(from));
    }
    Ok(Msg { epoch, from, body })
}

/// What the application has to do for a [`HostMigration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationAction {
    /// This seat won the election: open the re-host listener with this seat's static key on the
    /// first of `ports` that is free, then call [`HostMigration::listening`] (or
    /// [`HostMigration::listen_failed`] when none is).
    Listen {
        /// The agreed port and the next 15, in order.
        ports: Vec<u16>,
    },
    /// This seat no longer leads: close the re-host listener.
    StopListening,
    /// Connect to the winner `seat` with `key` pinned (a listener that proves another key is not
    /// the winner), trying `addrs` in order; then call [`HostMigration::connected`] or
    /// [`HostMigration::connect_failed`]. `addrs` is empty when the roster has no address for
    /// the seat: use what the game knows of it.
    Connect {
        /// The winner.
        seat: SeatId,
        /// Its address with the agreed port and the next 15.
        addrs: Vec<SocketAddr>,
        /// Its Noise static public key.
        key: ServerKey,
    },
    /// Send `bytes` to seat `to` on the re-host link's reliable stream.
    Send {
        /// The receiver.
        to: SeatId,
        /// The message.
        bytes: Vec<u8>,
    },
}

/// What the game shows during a migration (DO27's "HOST LOST -> CONNECTION ESTABLISHED -> GAME
/// RESUMING IN 5..1").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationEvent {
    /// The host has been silent for `migrate_after_ms`: an election starts. `tick` is the next
    /// frame this seat needs.
    HostLost {
        /// The next frame this seat needs.
        tick: u64,
    },
    /// The seats elect `winner` (`rank` 0 is the best connected; a higher rank follows a winner
    /// that did not answer).
    Electing {
        /// The seat that becomes the host.
        winner: SeatId,
        /// Its place in the ranking.
        rank: usize,
    },
    /// The match resumes at `tick`, `in_ticks` ticks after this event (the countdown).
    Resuming {
        /// The first frame of the new host.
        tick: u64,
        /// Ticks until it is due.
        in_ticks: u32,
    },
    /// This seat is the new host: take it with [`HostMigration::take_host`] and drive it (this
    /// seat stays a seat of it, on an in-process link).
    Promoted {
        /// The first frame it builds.
        tick: u64,
    },
    /// No seat is left to elect: every survivor was tried. The match cannot continue.
    NoCandidate,
}

/// What [`HostMigration::take_host`] builds a resumed host from.
#[derive(Debug, Clone)]
pub(crate) struct Resume {
    pub(crate) winner: SeatId,
    pub(crate) log_base: FrameState,
    pub(crate) front: u64,
    pub(crate) bodies: Vec<Vec<u8>>,
    pub(crate) seats_next: BTreeMap<SeatId, u64>,
    pub(crate) roster: MigrationRoster,
    pub(crate) matrix: RttMatrix,
    pub(crate) stamper: Stamper,
    pub(crate) resume_at_ms: u64,
    pub(crate) now_ms: u64,
}

#[derive(Debug, Clone)]
enum Phase {
    Watching,
    Following {
        winner: SeatId,
        since_ms: u64,
        connected: bool,
        retry_at_ms: u64,
        /// The winner's answer: it lacks the frames from this tick on.
        elected: Option<u64>,
    },
    Leading {
        since_ms: u64,
        listening: Option<u16>,
        /// The survivors that have to say hello.
        expected: BTreeSet<SeatId>,
        hellos: BTreeMap<SeatId, u64>,
        claimed: BTreeSet<SeatId>,
        frames: BTreeMap<u64, Vec<u8>>,
    },
}

/// A seat's part in host migration (spec 120 § 10.5). One per seat, next to its
/// [`LockstepSeat`]: call [`Self::poll`] once per application frame and [`Self::on_message`] for
/// every migration message; do what [`Self::poll_action`] asks and show what
/// [`Self::poll_event`] says.
#[derive(Debug)]
pub struct HostMigration {
    seat: SeatId,
    config: MigrationConfig,
    lockstep: LockstepConfig,
    epoch: u32,
    roster_rev: u64,
    matrix_version: Option<u32>,
    roster: Option<MigrationRoster>,
    matrix: RttMatrix,
    current_host: Option<SeatId>,
    ranking: Vec<SeatId>,
    /// Candidates of this epoch given up on.
    tried: BTreeSet<SeatId>,
    phase: Phase,
    actions: VecDeque<MigrationAction>,
    events: VecDeque<MigrationEvent>,
    resume: Option<Resume>,
    migrations: u64,
    refused: u64,
    last_resume_tick: Option<u64>,
    last_refusal: Option<&'static str>,
}

/// What `net.lockstep` reports about host migration (the `migration` object).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MigrationStatus {
    /// `watching`, `following` or `leading`.
    pub phase: String,
    /// Migrations this seat has been through (the epoch of the next one).
    pub epoch: u32,
    /// The seat whose process hosts the match (`None`: a dedicated host).
    pub current_host: Option<u16>,
    /// The seat this one follows or is (while electing).
    pub candidate: Option<u16>,
    /// The ranking of the running election, best connected first.
    pub ranking: Vec<u16>,
    /// Migrations completed.
    pub migrations: u64,
    /// The tick the last migration resumed at.
    pub last_resume_tick: Option<u64>,
    /// Migration messages refused (bad MAC, not a seat, stale epoch, malformed, unexpected).
    pub refused: u64,
    /// The code of the last refusal.
    pub last_refusal: Option<String>,
    /// The version of the round-trip matrix this seat holds.
    pub matrix_version: Option<u32>,
    /// A roster from the host arrived (migration is possible).
    pub roster: bool,
}

impl HostMigration {
    /// The migration state of `seat` in a match with these settings (the same on every seat).
    pub fn new(seat: SeatId, config: MigrationConfig, lockstep: LockstepConfig) -> Self {
        Self {
            seat,
            config,
            lockstep: lockstep.sanitized(),
            epoch: 0,
            roster_rev: 0,
            matrix_version: None,
            roster: None,
            matrix: RttMatrix::default(),
            current_host: None,
            ranking: Vec::new(),
            tried: BTreeSet::new(),
            phase: Phase::Watching,
            actions: VecDeque::new(),
            events: VecDeque::new(),
            resume: None,
            migrations: 0,
            refused: 0,
            last_resume_tick: None,
            last_refusal: None,
        }
    }

    /// This seat.
    pub fn seat(&self) -> SeatId {
        self.seat
    }

    /// The settings.
    pub fn config(&self) -> &MigrationConfig {
        &self.config
    }

    /// The seat whose process hosts the match, as this seat knows it.
    pub fn current_host(&self) -> Option<SeatId> {
        self.current_host
    }

    /// Migrations completed.
    pub fn migrations(&self) -> u64 {
        self.migrations
    }

    /// The seat this one follows while electing (itself when it leads).
    pub fn candidate(&self) -> Option<SeatId> {
        match &self.phase {
            Phase::Watching => None,
            Phase::Following { winner, .. } => Some(*winner),
            Phase::Leading { .. } => Some(self.seat),
        }
    }

    /// Whether an election is running.
    pub fn electing(&self) -> bool {
        !matches!(self.phase, Phase::Watching)
    }

    /// The next thing to do.
    pub fn poll_action(&mut self) -> Option<MigrationAction> {
        self.actions.pop_front()
    }

    /// The next thing to show.
    pub fn poll_event(&mut self) -> Option<MigrationEvent> {
        self.events.pop_front()
    }

    /// The resumed host after [`MigrationEvent::Promoted`], once: a relay host of the match that
    /// continues at the highest next tick, with this seat as one of its seats (connect them
    /// in-process). Set the game's input rules on it again.
    pub fn take_host<G: NetGame>(&mut self) -> Option<Result<LockstepHost<G>, LockstepError>> {
        let resume = self.resume.take()?;
        Some(LockstepHost::resumed(self.lockstep.clone(), resume))
    }

    /// The `migration` object of `net.lockstep`.
    pub fn status(&self) -> MigrationStatus {
        MigrationStatus {
            phase: match self.phase {
                Phase::Watching => "watching",
                Phase::Following { .. } => "following",
                Phase::Leading { .. } => "leading",
            }
            .to_owned(),
            epoch: self.epoch,
            current_host: self.current_host.map(|s| s.0),
            candidate: self.candidate().map(|s| s.0),
            ranking: self.ranking.iter().map(|s| s.0).collect(),
            migrations: self.migrations,
            last_resume_tick: self.last_resume_tick,
            refused: self.refused,
            last_refusal: self.last_refusal.map(str::to_owned),
            matrix_version: self.matrix_version,
            roster: self.roster.is_some(),
        }
    }

    /// Puts [`Self::status`] into a seat's or host's `net.lockstep` status.
    pub fn annotate(&self, status: &mut LockstepStatus) {
        status.migration = Some(self.status());
    }

    /// Takes the roster and the matrix the host shared, through the seat.
    fn pull<G: NetGame>(&mut self, seat: &LockstepSeat<G>) {
        if let Some((rev, roster)) = seat.migration_info()
            && rev != self.roster_rev
        {
            self.roster_rev = rev;
            if self.current_host.is_none() || self.migrations == 0 {
                self.current_host = roster.host_seat;
            }
            self.roster = Some(roster.clone());
        }
        if let Some(matrix) = seat.rtt_matrix()
            && self.matrix_version.is_none_or(|v| matrix.version > v)
        {
            self.adopt(matrix.clone());
        }
    }

    fn adopt(&mut self, matrix: RttMatrix) {
        self.matrix_version = Some(matrix.version);
        self.matrix = matrix;
    }

    /// Drives the migration: watches the host's silence, gives up on a winner that does not
    /// answer, and resumes the match when this seat leads and every survivor said hello (or the
    /// wait is over).
    pub fn poll<G: NetGame>(&mut self, seat: &mut LockstepSeat<G>, now_ms: u64) {
        self.pull(seat);
        match &self.phase {
            Phase::Watching => {
                let lost = self.roster.is_some()
                    && seat.ended_at().is_none()
                    && seat
                        .host_silent_ms(now_ms)
                        .is_some_and(|ms| ms >= self.config.migrate_after_ms);
                if lost {
                    self.events.push_back(MigrationEvent::HostLost {
                        tick: seat.frames_received_through(),
                    });
                    self.tried.clear();
                    self.elect(seat, now_ms);
                }
            }
            &Phase::Following {
                winner,
                since_ms,
                connected,
                retry_at_ms,
                elected,
            } => {
                let wait = match elected {
                    None => self.config.next_candidate_after_ms,
                    Some(_) => self.config.hello_wait_ms + self.config.next_candidate_after_ms,
                };
                if now_ms.saturating_sub(since_ms) >= wait {
                    // The winner died too, or never listened: the next ranked.
                    self.tried.insert(winner);
                    self.elect(seat, now_ms);
                } else if !connected && now_ms >= retry_at_ms {
                    self.connect_to(winner);
                    if let Phase::Following { retry_at_ms, .. } = &mut self.phase {
                        *retry_at_ms = now_ms + CONNECT_RETRY_MS;
                    }
                }
            }
            Phase::Leading {
                since_ms,
                listening,
                expected,
                hellos,
                ..
            } => {
                let all = expected.iter().all(|s| hellos.contains_key(s));
                let over = now_ms.saturating_sub(*since_ms) >= self.config.hello_wait_ms;
                let nobody = hellos.is_empty() && !expected.is_empty();
                let listening = listening.is_some();
                if over && nobody {
                    // Nobody came: this seat alone lost the host (or every other survivor died).
                    // It never runs the match on its own while others may still be in it.
                    self.actions.push_back(MigrationAction::StopListening);
                    self.phase = Phase::Watching;
                    self.events.push_back(MigrationEvent::NoCandidate);
                } else if listening && (all || over) {
                    self.finish(seat, now_ms);
                }
            }
        }
    }

    /// The survivors of this epoch: the matrix's seats without the lost host and the candidates
    /// given up on.
    fn survivors(&self) -> BTreeSet<SeatId> {
        let roster = self.roster.as_ref();
        self.matrix
            .seats()
            .filter(|s| Some(*s) != self.current_host && !self.tried.contains(s))
            .filter(|s| roster.is_some_and(|r| r.seat(*s).is_some()))
            .collect()
    }

    /// Ranks the survivors and follows (or becomes) the best one.
    fn elect<G: NetGame>(&mut self, seat: &LockstepSeat<G>, now_ms: u64) {
        if matches!(self.phase, Phase::Leading { .. }) {
            self.actions.push_back(MigrationAction::StopListening);
        }
        let survivors = self.survivors();
        self.ranking = rank_survivors(&self.matrix, &survivors);
        let rank = self.tried.len();
        let Some(&winner) = self.ranking.first() else {
            self.phase = Phase::Watching;
            self.events.push_back(MigrationEvent::NoCandidate);
            return;
        };
        self.events.push_back(MigrationEvent::Electing { winner, rank });
        if winner == self.seat {
            let expected = survivors
                .into_iter()
                .filter(|s| *s != self.seat && !self.matrix.silent.contains(s))
                .collect();
            self.phase = Phase::Leading {
                since_ms: now_ms,
                listening: None,
                expected,
                hellos: BTreeMap::new(),
                claimed: BTreeSet::new(),
                frames: BTreeMap::new(),
            };
            let port = self.roster.as_ref().map_or(0, |r| r.port);
            self.actions.push_back(MigrationAction::Listen {
                ports: listen_ports(port),
            });
        } else {
            self.phase = Phase::Following {
                winner,
                since_ms: now_ms,
                connected: false,
                retry_at_ms: now_ms + CONNECT_RETRY_MS,
                elected: None,
            };
            self.connect_to(winner);
        }
        let _ = seat;
    }

    fn connect_to(&mut self, winner: SeatId) {
        let Some(roster) = &self.roster else {
            return;
        };
        let Some(entry) = roster.seat(winner) else {
            return;
        };
        self.actions.push_back(MigrationAction::Connect {
            seat: winner,
            addrs: roster.listen_addrs(winner),
            key: entry.key,
        });
    }

    /// The re-host listener of this (leading) seat is open on `port`.
    pub fn listening(&mut self, port: u16) {
        if let Phase::Leading { listening, .. } = &mut self.phase {
            *listening = Some(port);
        }
    }

    /// None of the 16 ports was free: this seat gives up leading; the others move on to the next
    /// ranked when it does not answer, and so does this seat.
    pub fn listen_failed<G: NetGame>(&mut self, seat: &LockstepSeat<G>, now_ms: u64) {
        if matches!(self.phase, Phase::Leading { .. }) {
            self.tried.insert(self.seat);
            self.elect(seat, now_ms);
        }
    }

    /// The connection to the winner `to` is up (its static key verified): this seat claims its
    /// place.
    pub fn connected(&mut self, to: SeatId) {
        let Phase::Following {
            winner, connected, ..
        } = &mut self.phase
        else {
            return;
        };
        if *winner != to {
            return;
        }
        *connected = true;
        let body = Body::Claim {
            matrix: self.matrix.clone(),
        };
        self.send(to, &body);
    }

    /// No address of the winner `to` answered with its key: tried again shortly, until the
    /// winner's time is up.
    pub fn connect_failed(&mut self, to: SeatId) {
        if let Phase::Following {
            winner, connected, ..
        } = &mut self.phase
            && *winner == to
        {
            *connected = false;
        }
    }

    fn send(&mut self, to: SeatId, body: &Body) {
        let Some(roster) = &self.roster else {
            return;
        };
        let bytes = seal(roster, self.epoch, self.seat, body);
        self.actions.push_back(MigrationAction::Send { to, bytes });
    }

    fn refuse(&mut self, error: MigrationError) -> MigrationError {
        self.refused += 1;
        self.last_refusal = Some(error.code());
        tracing::debug!(seat = %self.seat, code = error.code(), "migration message refused");
        error
    }

    /// Takes one migration message (from the re-host link). Returns the seat it is from, proven
    /// by the MAC: bind the link to it. A refused message changes nothing.
    pub fn on_message<G: NetGame>(
        &mut self,
        seat: &mut LockstepSeat<G>,
        bytes: &[u8],
        now_ms: u64,
    ) -> Result<SeatId, MigrationError> {
        self.pull(seat);
        let Some(roster) = &self.roster else {
            return Err(self.refuse(MigrationError::NoRoster));
        };
        let max_body = self
            .lockstep
            .max_input_bytes
            .saturating_mul(MAX_SEATS)
            .max(1 << 16);
        let msg = match open(roster, bytes, max_body) {
            Ok(msg) => msg,
            Err(e) => return Err(self.refuse(e)),
        };
        if msg.epoch != self.epoch {
            return Err(self.refuse(MigrationError::StaleEpoch {
                got: msg.epoch,
                want: self.epoch,
            }));
        }
        let from = msg.from;
        match self.handle(seat, msg, now_ms) {
            Ok(()) => Ok(from),
            Err(e) => Err(self.refuse(e)),
        }
    }

    fn handle<G: NetGame>(
        &mut self,
        seat: &mut LockstepSeat<G>,
        msg: Msg,
        now_ms: u64,
    ) -> Result<(), MigrationError> {
        let from = msg.from;
        match msg.body {
            Body::Claim { matrix } => {
                if !matches!(self.phase, Phase::Leading { .. }) {
                    return Err(MigrationError::Unexpected("a claim to a seat that does not lead"));
                }
                if !self.matrix.host_rtt_ms.contains_key(&from) && !matrix.host_rtt_ms.contains_key(&from) {
                    return Err(MigrationError::NotMember(from));
                }
                if self.matrix_version.is_none_or(|v| matrix.version > v) {
                    // The claimer holds a newer matrix: the election follows it.
                    self.adopt(matrix);
                    let ranking = rank_survivors(&self.matrix, &self.survivors());
                    if ranking.first() != Some(&self.seat) {
                        self.elect(seat, now_ms);
                        return Ok(());
                    }
                    self.ranking = ranking;
                }
                if !self.survivors().contains(&from) {
                    return Err(MigrationError::NotMember(from));
                }
                let next = seat.frames_received_through();
                if let Phase::Leading { claimed, .. } = &mut self.phase {
                    claimed.insert(from);
                }
                let body = Body::Elect {
                    next,
                    matrix: self.matrix.clone(),
                };
                self.send(from, &body);
                Ok(())
            }
            Body::Elect { next, matrix } => {
                let Phase::Following { winner, .. } = self.phase else {
                    return Err(MigrationError::Unexpected(
                        "an election answer to a seat that does not follow",
                    ));
                };
                if self.matrix_version.is_none_or(|v| matrix.version > v) {
                    self.adopt(matrix);
                    self.ranking = rank_survivors(&self.matrix, &self.survivors());
                }
                if self.ranking.first() != Some(&from) {
                    if from != winner {
                        return Err(MigrationError::Unexpected(
                            "an election answer from a seat that did not win",
                        ));
                    }
                    // The winner's matrix is older than this seat's: it will yield to a claim.
                    self.elect(seat, now_ms);
                    return Ok(());
                }
                if from != winner {
                    // A newer matrix names `from`: follow it.
                    self.phase = Phase::Following {
                        winner: from,
                        since_ms: now_ms,
                        connected: true,
                        retry_at_ms: now_ms,
                        elected: None,
                    };
                    self.events.push_back(MigrationEvent::Electing {
                        winner: from,
                        rank: self.tried.len(),
                    });
                }
                if let Phase::Following { elected, .. } = &mut self.phase {
                    *elected = Some(next);
                }
                self.say_hello(seat, from, next);
                Ok(())
            }
            Body::HelloFrames { base_tick, bodies } => {
                let Phase::Leading { claimed, frames, .. } = &mut self.phase else {
                    return Err(MigrationError::Unexpected("frames to a seat that does not lead"));
                };
                if !claimed.contains(&from) {
                    return Err(MigrationError::Unexpected("frames before a claim"));
                }
                let own = seat.frames_received_through();
                let cap = own.saturating_add(self.lockstep.frame_log_ticks as u64);
                for (i, body) in bodies.into_iter().enumerate() {
                    let tick = base_tick.saturating_add(i as u64);
                    if tick >= own && tick < cap {
                        frames.entry(tick).or_insert(body);
                    }
                }
                Ok(())
            }
            Body::Hello { next } => {
                let Phase::Leading { claimed, hellos, .. } = &mut self.phase else {
                    return Err(MigrationError::Unexpected("a hello to a seat that does not lead"));
                };
                if !claimed.contains(&from) {
                    return Err(MigrationError::Unexpected("a hello before a claim"));
                }
                hellos.insert(from, next);
                Ok(())
            }
            Body::Resume { tick, in_ticks } => {
                let Phase::Following {
                    winner,
                    elected: Some(_),
                    ..
                } = self.phase
                else {
                    return Err(MigrationError::Unexpected(
                        "a resume to a seat that did not say hello",
                    ));
                };
                if from != winner {
                    return Err(MigrationError::Unexpected(
                        "a resume from a seat this one does not follow",
                    ));
                }
                seat.rehost(now_ms);
                self.events.push_back(MigrationEvent::Resuming { tick, in_ticks });
                self.complete(winner, tick);
                Ok(())
            }
        }
    }

    /// Sends the winner the frames it lacks (from `from_tick`) and this seat's next tick.
    fn say_hello<G: NetGame>(&mut self, seat: &LockstepSeat<G>, winner: SeatId, from_tick: u64) {
        let next = seat.frames_received_through();
        if next > from_tick
            && let Some(bodies) = seat.cached_bodies(from_tick)
        {
            let mut base = from_tick;
            let mut part: Vec<Vec<u8>> = Vec::new();
            let mut bytes = 0usize;
            for body in bodies {
                if !part.is_empty()
                    && (part.len() >= MAX_FRAMES_PER_PAYLOAD || bytes + body.len() > HELLO_BATCH_BYTES)
                {
                    let n = part.len() as u64;
                    let body = Body::HelloFrames {
                        base_tick: base,
                        bodies: std::mem::take(&mut part),
                    };
                    self.send(winner, &body);
                    base += n;
                    bytes = 0;
                }
                bytes += body.len();
                part.push(body);
            }
            if !part.is_empty() {
                let body = Body::HelloFrames {
                    base_tick: base,
                    bodies: part,
                };
                self.send(winner, &body);
            }
        }
        self.send(winner, &Body::Hello { next });
    }

    /// The leading seat resumes the match at the highest next tick it can build on.
    fn finish<G: NetGame>(&mut self, seat: &mut LockstepSeat<G>, now_ms: u64) {
        let Phase::Leading {
            hellos, mut frames, ..
        } = std::mem::replace(&mut self.phase, Phase::Watching)
        else {
            return;
        };
        let own = seat.frames_received_through();
        let (log_base, front, mut bodies) = seat.cache_parts();
        let mut tick = own;
        while let Some(body) = frames.remove(&tick) {
            bodies.push(body);
            tick += 1;
        }
        let resume_tick = tick;
        let mut seats_next: BTreeMap<SeatId, u64> =
            hellos.iter().map(|(s, n)| (*s, (*n).min(resume_tick))).collect();
        seats_next.insert(self.seat, own);
        let in_ticks =
            u32::try_from(self.config.resume_lead_ms.saturating_mul(1000) / self.lockstep.tick_us().max(1))
                .unwrap_or(u32::MAX);
        let mut roster = self.roster.clone().unwrap_or_else(|| MigrationRoster {
            match_id: 0,
            secret: [0; 32],
            port: 0,
            host_seat: None,
            seats: Vec::new(),
        });
        roster.host_seat = Some(self.seat);
        self.resume = Some(Resume {
            winner: self.seat,
            log_base,
            front,
            bodies,
            seats_next,
            roster,
            matrix: self.matrix.clone(),
            stamper: seat.stamper_clone(),
            resume_at_ms: now_ms + self.config.resume_lead_ms,
            now_ms,
        });
        for to in hellos.keys().copied().collect::<Vec<_>>() {
            self.send(
                to,
                &Body::Resume {
                    tick: resume_tick,
                    in_ticks,
                },
            );
        }
        seat.rehost(now_ms);
        self.events.push_back(MigrationEvent::Resuming {
            tick: resume_tick,
            in_ticks,
        });
        self.events
            .push_back(MigrationEvent::Promoted { tick: resume_tick });
        self.complete(self.seat, resume_tick);
    }

    fn complete(&mut self, host: SeatId, tick: u64) {
        self.phase = Phase::Watching;
        self.current_host = Some(host);
        self.epoch = self.epoch.wrapping_add(1);
        self.migrations += 1;
        self.last_resume_tick = Some(tick);
        self.tried.clear();
        if let Some(roster) = &mut self.roster {
            roster.host_seat = Some(host);
        }
    }
}

#[cfg(test)]
pub(crate) fn seal_for_tests(
    roster: &MigrationRoster,
    epoch: u32,
    from: SeatId,
    kind: u8,
    next: u64,
) -> Vec<u8> {
    let body = match kind {
        KIND_CLAIM => Body::Claim {
            matrix: RttMatrix::default(),
        },
        KIND_ELECT => Body::Elect {
            next,
            matrix: RttMatrix {
                version: u32::MAX,
                ..RttMatrix::default()
            },
        },
        KIND_HELLO => Body::Hello { next },
        _ => Body::Resume {
            tick: next,
            in_ticks: 1,
        },
    };
    seal(roster, epoch, from, &body)
}

#[cfg(test)]
pub(crate) const TEST_KIND_CLAIM: u8 = KIND_CLAIM;
#[cfg(test)]
pub(crate) const TEST_KIND_ELECT: u8 = KIND_ELECT;
#[cfg(test)]
pub(crate) const TEST_KIND_HELLO: u8 = KIND_HELLO;
#[cfg(test)]
pub(crate) const TEST_KIND_RESUME: u8 = KIND_RESUME;
