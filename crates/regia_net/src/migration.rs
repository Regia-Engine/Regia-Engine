// SPDX-License-Identifier: MPL-2.0
//! Warm successors for replication games: the successor journal (spec 120 § 10.5; ADR-8853).
//!
//! A listen server that quits takes the match with it unless another process already holds the
//! world. The host therefore picks one or two **successors**, its best connected clients
//! ([`rank_candidates`]), and streams each of them a journal on the bulk stream:
//!
//! - a **full state** (the game's own bytes, with the world's digest) every
//!   [`JournalConfig::full_every_ms`] (5 s), cut into parts;
//! - the **log** of every tick: that tick's inputs and events, as two opaque blobs the game
//!   writes, plus the world's digest on the ticks a full state was taken.
//!
//! A successor applies the journal to a **shadow world** ([`ShadowWorld`]) as it arrives, so the
//! shadow trails the host by a tick or two and can be promoted ([`JournalSuccessor::promote`])
//! when the host is lost. The hand-off itself (clients resuming to the successor) is the next
//! part of the design and not in this module.
//!
//! The module is sans-IO: [`JournalHost::poll`] hands out payloads for
//! `Channel::Stream(StreamId::BULK)`, [`JournalSuccessor::on_payload`] takes them, and
//! [`JournalSuccessor::poll_ack`] hands back a small cumulative acknowledgment (any channel; it
//! is safe to lose). [`JournalHost::pump_server`] does the host's side on a [`NetServer`].
//!
//! Rules that keep it bounded and honest:
//!
//! - **Budget.** Each successor's journal spends at most [`JournalConfig::budget_share`] of what
//!   the rate controller allows for that peer (`NetServer::send_allowance`). The log goes before
//!   full-state parts; nothing borrows.
//! - **One fragment at a time.** What one poll hands a successor is one payload of at most
//!   [`JournalConfig::part_bytes`] (900: one stream fragment), and [`JournalHost::poll_links`]
//!   hands out the next only when the stream has acknowledged the one before (one may be on its
//!   way on a link with a healthy allowance). The transport fails a connection whose stream has a
//!   gap with later fragments stored behind it for about 2 s (`reassembly_stalled`), and on a
//!   link at the rate floor the sender's own pacing holds bulk repairs back that long: measured
//!   on `bad-wifi`, a journal that kept feeding the stream cost seven of eight clients their
//!   connection. With nothing behind the gap that cannot happen.
//! - **Depth.** A successor holds its newest full state and at most
//!   [`JournalConfig::depth_ticks`] ticks of log after it. A record at exactly that depth is
//!   taken; one past it is refused (`journal_past_window`) and the successor waits for a full
//!   state. The host does not send past the window either: its log waits there for the next
//!   full state.
//! - **Digest at every full state.** The host's digest rides the log record of a full-state tick.
//!   A shadow whose digest matched there adopts the full state without loading it; any other
//!   shadow loads it, checks the digest again and replays the log it kept.
//! - **Hostile input.** Every length is capped before anything is stored, decoding never panics,
//!   and each refusal has a code ([`JournalError::code`]).

use std::collections::VecDeque;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::connection::{Channel, SendError, StreamId};
use crate::payload::NetGame;
use crate::server::NetServer;
use crate::session::PeerId;
use crate::socket::DatagramSocket;
use crate::status::NetAgentState;
use crate::wire::{WireError, WireReader, WireWriter};

/// Default [`JournalConfig::full_every_ms`]: a full state every 5 s.
pub const JOURNAL_FULL_EVERY_MS: u64 = 5_000;
/// Default [`JournalConfig::depth_ticks`]: 600 ticks of log after a full state (10 s at 60 Hz,
/// two full-state periods).
pub const JOURNAL_DEPTH_TICKS: u32 = 600;
/// The largest depth a successor accepts from a host, ticks.
pub const MAX_JOURNAL_DEPTH_TICKS: u32 = 7_200;
/// Default [`JournalConfig::part_bytes`]: what one poll hands a successor is one payload of at
/// most 900 bytes, which travels as a single fragment of the bulk stream at the default packet
/// size. With one fragment per payload and the in-flight limit of [`JournalHost::poll_links`],
/// no later fragment waits behind a lost one at the receiver, so the journal cannot trip the
/// transport's reassembly stall deadline (which fails the connection).
pub const JOURNAL_PART_BYTES: usize = 900;
/// Under this send allowance, bytes per second, the journal is stop-and-wait: a new payload
/// leaves only when the peer has acknowledged every fragment before it. At or above it one
/// payload may be on its way when the next leaves.
pub const JOURNAL_STOP_AND_WAIT_BYTES_PER_S: u32 = 2 * JOURNAL_STARVED_BYTES_PER_S;
/// The largest full-state part a successor accepts, bytes.
pub const MAX_JOURNAL_PART_BYTES: usize = 32 * 1024;
/// The largest full state, bytes (8 MiB).
pub const MAX_JOURNAL_FULL_BYTES: usize = 8 * 1024 * 1024;
/// The largest inputs blob and the largest events blob of one tick, bytes each.
pub const MAX_JOURNAL_RECORD_BYTES: usize = 16 * 1024;
/// The largest journal payload, bytes: what a game's message type must be able to carry.
pub const MAX_JOURNAL_PAYLOAD_BYTES: usize = 2 * MAX_JOURNAL_RECORD_BYTES + 64;
/// Successors a host keeps warm at most.
pub const MAX_SUCCESSORS: usize = 2;
/// Default [`JournalConfig::budget_share`]: a quarter of the peer's send allowance.
pub const JOURNAL_BUDGET_SHARE: f32 = 0.25;
/// Default [`JournalConfig::fallback_bytes_per_s`]: the allowance assumed for a peer whose link
/// has no rate controller, bytes per second.
pub const JOURNAL_FALLBACK_BYTES_PER_S: u32 = 128 * 1024;
/// A candidate whose send allowance is under this many bytes per second is starved: its link is
/// near the rate controller's floor and a quarter of it carries little journal. Starved clients
/// are picked only when no other client is left.
pub const JOURNAL_STARVED_BYTES_PER_S: u32 = 16 * 1024;
/// Bytes of a world digest.
pub const DIGEST_BYTES: usize = 32;

/// A world digest: the game's hash of its authoritative state (BLAKE3 of a canonical encoding,
/// for example).
pub type WorldDigest = [u8; DIGEST_BYTES];

const KIND_APPOINT: u8 = 0x40;
const KIND_DISMISS: u8 = 0x41;
const KIND_FULL: u8 = 0x42;
const KIND_LOG: u8 = 0x43;
const KIND_ACK: u8 = 0x44;
const KIND_FULL_DATA: u8 = 0x45;
/// Log record flag: the record carries the host's world digest after its tick.
const RECORD_HAS_DIGEST: u8 = 0x01;
/// Log record flag: the record carries inputs (an empty blob is not written).
const RECORD_HAS_INPUTS: u8 = 0x02;
/// Log record flag: the record carries events.
const RECORD_HAS_EVENTS: u8 = 0x04;
const RECORD_FLAGS: u8 = RECORD_HAS_DIGEST | RECORD_HAS_INPUTS | RECORD_HAS_EVENTS;
/// Records one log payload carries at most.
const MAX_LOG_BATCH: usize = 64;
/// Messages one payload carries at most.
const MAX_BUNDLE_MESSAGES: usize = 16;
/// An idle token bucket holds at most this many ticks of the budget rate, and never less than
/// one payload.
const BURST_TICKS: f64 = 2.0;
/// The smallest full-state part worth a payload, bytes (the last part may be smaller).
const MIN_PART_BYTES: usize = 64;
/// Bytes of a full-state data part before its bytes.
const FULL_DATA_HEADER_BYTES: usize = 1 + 4 + 4 + 4 + 2;
/// The host takes an extra full state (outside the cadence) at most this often, milliseconds: for
/// a successor the newest one cannot seed, or whose shadow failed on it.
const EXTRA_FULL_GAP_MS: u64 = 1_000;
/// Full-state ticks at which the shadow matched the host that a successor remembers.
const VERIFIED_KEPT: usize = 8;
/// Lag values the histogram tells apart; larger lags count as this.
const LAG_BUCKETS: usize = 64;

/// Whether `bytes` is a journal payload (by its first byte), so a game can route its bulk-stream
/// messages.
pub fn is_journal_payload(bytes: &[u8]) -> bool {
    matches!(bytes.first(), Some(&(KIND_APPOINT..=KIND_FULL_DATA)))
}

/// Why a journal call or payload was refused. [`Self::code`] is the agent-protocol code.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum JournalError {
    /// The payload does not decode.
    #[error("journal payload does not decode: {0}")]
    Malformed(#[from] WireError),
    /// Journal data for a client that is not a successor, or an acknowledgment from a peer that
    /// is not one.
    #[error("not a successor")]
    NotSuccessor,
    /// A payload this side never takes (a log on the host, an acknowledgment on a client).
    #[error("unexpected journal payload")]
    Unexpected,
    /// The payload names an appointment that is over.
    #[error("payload of an appointment that is over")]
    StaleAppointment,
    /// A log record arrived before any full state started.
    #[error("log record before any full state")]
    NoBase,
    /// A log record skipped ticks.
    #[error("log record for tick {got}, expected {expected}")]
    Gap {
        /// The tick the successor expected.
        expected: u64,
        /// The tick it got.
        got: u64,
    },
    /// A log record past the journal's depth after the newest full state.
    #[error("log record for tick {tick} is past the journal window (last tick {limit})")]
    PastWindow {
        /// The record's tick.
        tick: u64,
        /// The last tick the window holds.
        limit: u64,
    },
    /// A full-state part out of order, or past the length its header said.
    #[error("full-state part out of order")]
    FullOutOfOrder,
    /// A full state over the size cap.
    #[error("full state of {len} bytes exceeds {max}")]
    FullTooLarge {
        /// The declared or given length.
        len: usize,
        /// The cap.
        max: usize,
    },
    /// A tick's inputs or events blob over [`MAX_JOURNAL_RECORD_BYTES`].
    #[error("journal record of {len} bytes exceeds {max}")]
    RecordTooLarge {
        /// The blob's length.
        len: usize,
        /// The cap.
        max: usize,
    },
    /// The shadow world refused a full state.
    #[error("the shadow world refused the full state: {0}")]
    LoadFailed(String),
    /// The shadow world refused a log record.
    #[error("the shadow world refused a log record: {0}")]
    ApplyFailed(String),
    /// A loaded full state does not have the digest the host sent with it.
    #[error("the full state of tick {tick} does not match the host's digest")]
    DigestMismatch {
        /// The full state's tick.
        tick: u64,
    },
    /// A tick at or before the newest journaled tick.
    #[error("tick {tick} is not after the newest journaled tick {newest}")]
    TickNotAfter {
        /// The tick given.
        tick: u64,
        /// The newest journaled tick.
        newest: u64,
    },
    /// A full state for a tick that is not the newest journaled tick.
    #[error("a full state must be of the newest journaled tick")]
    FullTickUnknown,
    /// The shadow is not warm, so it cannot be promoted.
    #[error("the shadow world is not warm")]
    NotWarm,
}

impl JournalError {
    /// The code agents and logs see: lowercase ASCII, digits and `_`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "journal_malformed",
            Self::NotSuccessor => "journal_not_successor",
            Self::Unexpected => "journal_unexpected",
            Self::StaleAppointment => "journal_stale_appointment",
            Self::NoBase => "journal_no_base",
            Self::Gap { .. } => "journal_gap",
            Self::PastWindow { .. } => "journal_past_window",
            Self::FullOutOfOrder => "journal_full_out_of_order",
            Self::FullTooLarge { .. } => "journal_full_too_large",
            Self::RecordTooLarge { .. } => "journal_record_too_large",
            Self::LoadFailed(_) => "journal_load_failed",
            Self::ApplyFailed(_) => "journal_apply_failed",
            Self::DigestMismatch { .. } => "journal_digest_mismatch",
            Self::TickNotAfter { .. } => "journal_tick_not_after",
            Self::FullTickUnknown => "journal_full_tick_unknown",
            Self::NotWarm => "journal_not_warm",
        }
    }
}

/// How a host journals and how a successor bounds what it holds. The successor takes the depth
/// from the host's appointment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalConfig {
    /// The simulation's tick rate, per second.
    pub tick_hz: u32,
    /// A full state is wanted this often, milliseconds.
    pub full_every_ms: u64,
    /// Successors kept warm: 1 or 2 (0 turns the journal off).
    pub successors: usize,
    /// Ticks of log a successor holds after its newest full state; one past is refused.
    pub depth_ticks: u32,
    /// Share of a successor's send allowance its journal may spend, `0.0..=1.0`.
    pub budget_share: f32,
    /// The allowance assumed for a peer whose link has no rate controller, bytes per second.
    pub fallback_bytes_per_s: u32,
    /// Most bytes in the payload one poll hands a successor (the log of the tick and a part of
    /// the full state). Keep it under one stream fragment unless the bulk stream of every link
    /// is known never to be starved.
    pub part_bytes: usize,
    /// A sitting successor is compared with the other candidates this often, milliseconds.
    pub rerank_ms: u64,
    /// A candidate replaces a sitting successor only when its score is below the successor's
    /// times this factor ...
    pub switch_factor: f32,
    /// ... and better by at least this many milliseconds.
    pub switch_min_ms: f32,
    /// The host compares smoothed scores: each client's score follows its link numbers with
    /// this time constant, milliseconds, so one burst of loss does not unseat a successor.
    pub score_smooth_ms: u64,
}

impl Default for JournalConfig {
    fn default() -> Self {
        Self {
            tick_hz: 60,
            full_every_ms: JOURNAL_FULL_EVERY_MS,
            successors: MAX_SUCCESSORS,
            depth_ticks: JOURNAL_DEPTH_TICKS,
            budget_share: JOURNAL_BUDGET_SHARE,
            fallback_bytes_per_s: JOURNAL_FALLBACK_BYTES_PER_S,
            part_bytes: JOURNAL_PART_BYTES,
            rerank_ms: 10_000,
            switch_factor: 0.5,
            switch_min_ms: 20.0,
            score_smooth_ms: 10_000,
        }
    }
}

impl JournalConfig {
    /// The same settings with every field inside its limits.
    pub fn sanitized(mut self) -> Self {
        self.tick_hz = self.tick_hz.clamp(1, 1_000);
        self.full_every_ms = self.full_every_ms.max(100);
        self.successors = self.successors.min(MAX_SUCCESSORS);
        self.depth_ticks = self.depth_ticks.clamp(1, MAX_JOURNAL_DEPTH_TICKS);
        self.budget_share = if self.budget_share.is_finite() {
            self.budget_share.clamp(0.0, 1.0)
        } else {
            JOURNAL_BUDGET_SHARE
        };
        self.part_bytes = self.part_bytes.clamp(256, MAX_JOURNAL_PART_BYTES);
        self.switch_factor = if self.switch_factor.is_finite() {
            self.switch_factor.clamp(0.0, 1.0)
        } else {
            0.5
        };
        self.switch_min_ms = if self.switch_min_ms.is_finite() {
            self.switch_min_ms.max(0.0)
        } else {
            20.0
        };
        self
    }
}

/// The world a successor keeps warm. The game implements it on a server-shaped copy of its
/// state; the bytes are the game's own.
pub trait ShadowWorld {
    /// Replaces the whole world with the full state taken after `tick`.
    fn load_full(&mut self, tick: u64, bytes: &[u8]) -> Result<(), String>;
    /// Applies one tick of the log: the inputs the host stepped `tick` with and the events it
    /// journaled for it.
    fn apply(&mut self, tick: u64, inputs: &[u8], events: &[u8]) -> Result<(), String>;
    /// The digest of the world now, computed the way the host computes the one it journals.
    fn digest(&self) -> WorldDigest;
}

/// One tick of the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    /// The tick.
    pub tick: u64,
    /// The inputs the host stepped the tick with.
    pub inputs: Arc<[u8]>,
    /// The events the host journaled for the tick.
    pub events: Arc<[u8]>,
    /// The host's world digest after the tick, on the ticks it sent one.
    pub digest: Option<WorldDigest>,
}

/// A client the host may make a successor, with the link numbers the choice reads.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Candidate {
    /// The client.
    pub peer: PeerId,
    /// Smoothed round trip to the host, milliseconds.
    pub rtt_ms: f32,
    /// Round-trip variation, milliseconds.
    pub jitter_ms: f32,
    /// Recent loss, `0.0..=1.0`.
    pub loss_fraction: f32,
    /// The client is silent (`Interrupted`): never appointed, and replaced when it is one.
    pub interrupted: bool,
    /// The rate controller's send allowance for the client, bytes per second
    /// (`NetServer::send_allowance`); `None` when the link has no rate controller.
    pub allowance_bytes_per_s: Option<u32>,
}

impl Candidate {
    /// The connection score, milliseconds; lower is better connected: the round trip plus its
    /// variation plus 10 ms for every percent of loss.
    pub fn score_ms(&self) -> f32 {
        let part = |v: f32, bad: f32| if v.is_finite() && v >= 0.0 { v } else { bad };
        part(self.rtt_ms, 60_000.0)
            + part(self.jitter_ms, 0.0)
            + part(self.loss_fraction, 1.0).min(1.0) * 1_000.0
    }
}

impl Candidate {
    /// The link's allowance is under [`JOURNAL_STARVED_BYTES_PER_S`].
    pub fn starved(&self) -> bool {
        self.allowance_bytes_per_s
            .is_some_and(|a| a < JOURNAL_STARVED_BYTES_PER_S)
    }

    /// 0: a link that can carry a journal; 1: starved; 2: interrupted.
    fn tier(&self) -> u8 {
        if self.interrupted {
            2
        } else {
            u8::from(self.starved())
        }
    }
}

/// Ranks candidates best connected first: clients whose link can carry a journal, then starved
/// ones ([`Candidate::starved`]), then interrupted ones; inside each group the lowest
/// [`Candidate::score_ms`], ties by the lowest peer id.
pub fn rank_candidates(candidates: &[Candidate]) -> Vec<PeerId> {
    rank_scored(candidates, Candidate::score_ms)
}

/// [`rank_candidates`] with the score of each candidate given by `score`.
fn rank_scored(candidates: &[Candidate], score: impl Fn(&Candidate) -> f32) -> Vec<PeerId> {
    let mut sorted: Vec<(&Candidate, f32)> = candidates.iter().map(|c| (c, score(c))).collect();
    sorted.sort_by(|a, b| {
        a.0.tier()
            .cmp(&b.0.tier())
            .then(a.1.total_cmp(&b.1))
            .then(a.0.peer.cmp(&b.0.peer))
    });
    let mut out: Vec<PeerId> = Vec::with_capacity(sorted.len());
    for (c, _) in sorted {
        if !out.contains(&c.peer) {
            out.push(c.peer);
        }
    }
    out
}

/// A successor's link, as [`JournalHost::poll_links`] reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JournalLink {
    /// The rate controller's send allowance for the peer, bytes per second; `None` when the
    /// link has no rate controller.
    pub allowance_bytes_per_s: Option<u32>,
    /// Fragments of the peer's bulk stream that are queued or sent and not acknowledged yet
    /// (`StreamStatus::queued_fragments`).
    pub in_flight_fragments: usize,
}

/// A payload for one peer's bulk stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalOut {
    /// The successor (or the client being dismissed).
    pub peer: PeerId,
    /// The payload, at most [`MAX_JOURNAL_PAYLOAD_BYTES`].
    pub bytes: Vec<u8>,
}

/// What the host tells the game about its successors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalEvent {
    /// A client became a successor, or its rank changed.
    Appointed {
        /// The client.
        peer: PeerId,
        /// 0 is the first successor.
        rank: u8,
    },
    /// A client is no longer a successor.
    Dismissed {
        /// The client.
        peer: PeerId,
        /// `left`, `interrupted`, `fell_behind` or `outranked`.
        reason: &'static str,
    },
    /// A successor's shadow is warm: it holds a full state and follows the log.
    Warm {
        /// The successor.
        peer: PeerId,
    },
}

/// A successor's state, as it acknowledges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ShadowState {
    /// Not a successor.
    #[default]
    Idle,
    /// Appointed; no usable full state yet.
    Seeding,
    /// A full state is loaded and the log is applied as it arrives.
    Warm,
    /// The shadow lost the journal (a refusal) and waits for a full state.
    Stale,
}

impl ShadowState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Seeding => "seeding",
            Self::Warm => "warm",
            Self::Stale => "stale",
        }
    }

    fn wire(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Seeding => 1,
            Self::Warm => 2,
            Self::Stale => 3,
        }
    }

    fn from_wire(v: u8) -> Result<Self, WireError> {
        Ok(match v {
            0 => Self::Idle,
            1 => Self::Seeding,
            2 => Self::Warm,
            3 => Self::Stale,
            _ => return Err(WireError::Invalid("journal state")),
        })
    }
}

/// A decoded log record, borrowing the payload.
struct RecordRef<'a> {
    inputs: &'a [u8],
    events: &'a [u8],
    digest: Option<WorldDigest>,
}

/// A decoded journal payload.
enum Msg<'a> {
    Appoint {
        appointment: u32,
        rank: u8,
        depth_ticks: u32,
    },
    Dismiss {
        appointment: u32,
    },
    /// The start of a full state: its tick, length and digest.
    FullBegin {
        appointment: u32,
        seq: u32,
        /// The log starts again after this full state: the successor drops what it holds.
        reseed: bool,
        tick: u64,
        total: u32,
        digest: WorldDigest,
    },
    /// The next bytes of a full state, at `offset`.
    FullData {
        appointment: u32,
        seq: u32,
        offset: u32,
        bytes: &'a [u8],
    },
    /// The log of `records.len()` ticks in a row. The header is five bytes: the stream is
    /// ordered, so the low bits of the appointment and of the first tick are enough to tell a
    /// stale or skipped payload.
    Log {
        appointment_low: u8,
        tick_low: u16,
        records: Vec<RecordRef<'a>>,
    },
    Ack(Ack),
}

/// A successor's cumulative acknowledgment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ack {
    appointment: u32,
    state: ShadowState,
    /// `u64::MAX`: nothing applied.
    applied_tick: u64,
    /// The newest full state the successor has seen whole (loaded, adopted or failed).
    seen_seq: u32,
    digest_mismatches: u32,
    replay_divergences: u32,
    refused: u32,
}

/// A payload's messages, in order: a payload is one message or several in a row (the host
/// bundles what one poll hands a successor, so it travels as one stream fragment).
fn decode_all(bytes: &[u8]) -> Result<Vec<Msg<'_>>, WireError> {
    let mut r = WireReader::new(bytes);
    let mut out = vec![decode_one(&mut r)?];
    while !r.is_empty() {
        if out.len() >= MAX_BUNDLE_MESSAGES {
            return Err(WireError::Invalid("journal bundle length"));
        }
        out.push(decode_one(&mut r)?);
    }
    Ok(out)
}

/// A payload that is exactly one message.
#[cfg(test)]
fn decode(bytes: &[u8]) -> Result<Msg<'_>, WireError> {
    let mut r = WireReader::new(bytes);
    let msg = decode_one(&mut r)?;
    r.finish()?;
    Ok(msg)
}

fn decode_one<'a>(r: &mut WireReader<'a>) -> Result<Msg<'a>, WireError> {
    let msg = match r.u8()? {
        KIND_APPOINT => {
            let appointment = r.u32()?;
            let rank = r.u8()?;
            let depth_ticks = r.u32()?;
            if usize::from(rank) >= MAX_SUCCESSORS {
                return Err(WireError::Invalid("journal rank"));
            }
            if depth_ticks == 0 || depth_ticks > MAX_JOURNAL_DEPTH_TICKS {
                return Err(WireError::Invalid("journal depth"));
            }
            Msg::Appoint {
                appointment,
                rank,
                depth_ticks,
            }
        }
        KIND_DISMISS => Msg::Dismiss {
            appointment: r.u32()?,
        },
        KIND_FULL => {
            let appointment = r.u32()?;
            let seq = r.u32()?;
            let reseed = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("journal full flags")),
            };
            let tick = r.u64()?;
            let total = r.u32()?;
            if total as usize > MAX_JOURNAL_FULL_BYTES {
                return Err(WireError::TooLong {
                    len: total as usize,
                    max: MAX_JOURNAL_FULL_BYTES,
                });
            }
            Msg::FullBegin {
                appointment,
                seq,
                reseed,
                tick,
                total,
                digest: r.array::<DIGEST_BYTES>()?,
            }
        }
        KIND_FULL_DATA => {
            let appointment = r.u32()?;
            let seq = r.u32()?;
            let offset = r.u32()?;
            let bytes = r.bytes_u16(MAX_JOURNAL_PART_BYTES)?;
            if bytes.is_empty() || offset as usize + bytes.len() > MAX_JOURNAL_FULL_BYTES {
                return Err(WireError::Invalid("journal full part"));
            }
            Msg::FullData {
                appointment,
                seq,
                offset,
                bytes,
            }
        }
        KIND_LOG => {
            let appointment_low = r.u8()?;
            let tick_low = r.u16()?;
            let count = usize::from(r.u8()?);
            if count == 0 || count > MAX_LOG_BATCH {
                return Err(WireError::Invalid("journal log count"));
            }
            let mut records = Vec::with_capacity(count);
            for _ in 0..count {
                let flags = r.u8()?;
                if flags & !RECORD_FLAGS != 0 {
                    return Err(WireError::Invalid("journal record flags"));
                }
                let mut blob = |flag: u8| -> Result<&[u8], WireError> {
                    if flags & flag == 0 {
                        return Ok(&[]);
                    }
                    let bytes = r.bytes_u16(MAX_JOURNAL_RECORD_BYTES)?;
                    if bytes.is_empty() {
                        return Err(WireError::Invalid("journal record blob"));
                    }
                    Ok(bytes)
                };
                let inputs = blob(RECORD_HAS_INPUTS)?;
                let events = blob(RECORD_HAS_EVENTS)?;
                let digest = if flags & RECORD_HAS_DIGEST != 0 {
                    Some(r.array::<DIGEST_BYTES>()?)
                } else {
                    None
                };
                records.push(RecordRef {
                    inputs,
                    events,
                    digest,
                });
            }
            Msg::Log {
                appointment_low,
                tick_low,
                records,
            }
        }
        KIND_ACK => Msg::Ack(Ack {
            appointment: r.u32()?,
            state: ShadowState::from_wire(r.u8()?)?,
            applied_tick: r.u64()?,
            seen_seq: r.u32()?,
            digest_mismatches: r.u32()?,
            replay_divergences: r.u32()?,
            refused: r.u32()?,
        }),
        _ => return Err(WireError::Invalid("journal payload kind")),
    };
    Ok(msg)
}

fn encode_appoint(appointment: u32, rank: u8, depth_ticks: u32) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(10);
    w.u8(KIND_APPOINT);
    w.u32(appointment);
    w.u8(rank);
    w.u32(depth_ticks);
    w.finish()
}

fn encode_dismiss(appointment: u32) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(5);
    w.u8(KIND_DISMISS);
    w.u32(appointment);
    w.finish()
}

/// The start of a full state: the tick, length and digest alone, so it leaves at once and the log
/// can follow it. `reseed`: the log starts again after this full state.
fn encode_full_begin(appointment: u32, full: &FullState, reseed: bool) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(FULL_BEGIN_BYTES);
    w.u8(KIND_FULL);
    w.u32(appointment);
    w.u32(full.seq);
    w.u8(u8::from(reseed));
    w.u64(full.tick);
    w.u32(full.bytes.len() as u32);
    w.raw(&full.digest);
    w.finish()
}

const FULL_BEGIN_BYTES: usize = 1 + 4 + 4 + 1 + 8 + 4 + DIGEST_BYTES;

/// The bytes of a full state from `offset`, at most `len` of them.
fn encode_full_data(appointment: u32, full: &FullState, offset: usize, len: usize) -> Vec<u8> {
    let start = offset.min(full.bytes.len());
    let end = start
        .saturating_add(len.min(MAX_JOURNAL_PART_BYTES))
        .min(full.bytes.len());
    let mut w = WireWriter::with_capacity(FULL_DATA_HEADER_BYTES + (end - start));
    w.u8(KIND_FULL_DATA);
    w.u32(appointment);
    w.u32(full.seq);
    w.u32(start as u32);
    let _ = w.bytes_u16(&full.bytes[start..end], MAX_JOURNAL_PART_BYTES);
    w.finish()
}

fn record_wire_len(r: &JournalRecord) -> usize {
    let blob = |b: &[u8]| if b.is_empty() { 0 } else { 2 + b.len() };
    1 + blob(&r.inputs) + blob(&r.events) + if r.digest.is_some() { DIGEST_BYTES } else { 0 }
}

/// Bytes of a log payload before its records: an empty tick costs these and one flags byte.
const LOG_HEADER_BYTES: usize = 1 + 1 + 2 + 1;

fn encode_log<'a>(
    appointment: u32,
    first_tick: u64,
    records: impl ExactSizeIterator<Item = &'a JournalRecord>,
) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(256);
    w.u8(KIND_LOG);
    w.u8(appointment as u8);
    w.u16(first_tick as u16);
    w.u8(records.len().min(MAX_LOG_BATCH) as u8);
    for r in records.take(MAX_LOG_BATCH) {
        let mut flags = 0;
        if r.digest.is_some() {
            flags |= RECORD_HAS_DIGEST;
        }
        if !r.inputs.is_empty() {
            flags |= RECORD_HAS_INPUTS;
        }
        if !r.events.is_empty() {
            flags |= RECORD_HAS_EVENTS;
        }
        w.u8(flags);
        // Record blobs are capped at `MAX_JOURNAL_RECORD_BYTES` when they are journaled.
        if !r.inputs.is_empty() {
            let _ = w.bytes_u16(&r.inputs, MAX_JOURNAL_RECORD_BYTES);
        }
        if !r.events.is_empty() {
            let _ = w.bytes_u16(&r.events, MAX_JOURNAL_RECORD_BYTES);
        }
        if let Some(d) = &r.digest {
            w.raw(d);
        }
    }
    w.finish()
}

fn encode_ack(a: &Ack) -> Vec<u8> {
    let mut w = WireWriter::with_capacity(26);
    w.u8(KIND_ACK);
    w.u32(a.appointment);
    w.u8(a.state.wire());
    w.u64(a.applied_tick);
    w.u32(a.seen_seq);
    w.u32(a.digest_mismatches);
    w.u32(a.replay_divergences);
    w.u32(a.refused);
    w.finish()
}

/// One captured full state.
#[derive(Debug)]
struct FullState {
    seq: u32,
    tick: u64,
    bytes: Vec<u8>,
    digest: WorldDigest,
}

/// A histogram of lag samples, ticks.
#[derive(Debug, Clone)]
struct LagHist {
    counts: [u64; LAG_BUCKETS + 1],
    samples: u64,
    max: u64,
}

impl Default for LagHist {
    fn default() -> Self {
        Self {
            counts: [0; LAG_BUCKETS + 1],
            samples: 0,
            max: 0,
        }
    }
}

impl LagHist {
    fn add(&mut self, lag: u64) {
        self.counts[(lag as usize).min(LAG_BUCKETS)] += 1;
        self.samples += 1;
        self.max = self.max.max(lag);
    }

    /// The smallest lag that at least 99 % of the samples are at or under.
    fn p99(&self) -> u64 {
        if self.samples == 0 {
            return 0;
        }
        let need = (self.samples * 99).div_ceil(100);
        let mut seen = 0;
        for (lag, n) in self.counts.iter().enumerate() {
            seen += n;
            if seen >= need {
                return if lag == LAG_BUCKETS { self.max } else { lag as u64 };
            }
        }
        self.max
    }
}

/// A full state on its way to one successor.
#[derive(Debug)]
struct Sending {
    full: Arc<FullState>,
    /// It seeds the successor anew: the log starts again after it.
    reseed: bool,
    /// Its start (tick, length, digest) has been sent.
    begun: bool,
    /// Bytes sent.
    offset: usize,
}

/// The host's view of one successor.
#[derive(Debug)]
struct Successor {
    peer: PeerId,
    appointment: u32,
    rank: u8,
    /// The rank the successor was last told.
    told_rank: Option<u8>,
    /// The full state being sent.
    sending: Option<Sending>,
    /// Sequence and tick of the newest full state sent whole.
    sent: Option<(u32, u64)>,
    /// The newest full state this successor was started on.
    started_seq: u32,
    /// The full state that last seeded it (its first, or the one after it fell behind or went
    /// stale).
    seed_seq: u32,
    /// The next log tick to send (`None`: waits for a full state to start).
    next_log_tick: Option<u64>,
    /// It needs a full state before any more log: new, behind, or its shadow failed.
    needs_full: bool,
    ack: Option<Ack>,
    warm_told: bool,
    tokens: f64,
    refilled_ms: Option<u64>,
    rate: f64,
    bytes_sent: u64,
    budget_bytes: f64,
    second_start_ms: u64,
    second_bytes: u64,
    second_budget: f64,
    last_second_bytes: u64,
    seconds_over_budget: u64,
    fell_behind: u64,
    /// `fell_behind` at the last comparison of successors.
    fell_behind_seen: u64,
    lag: LagHist,
    lag_now: u64,
}

impl Successor {
    fn new(peer: PeerId, appointment: u32, rank: u8) -> Self {
        Self {
            peer,
            appointment,
            rank,
            told_rank: None,
            sending: None,
            sent: None,
            started_seq: 0,
            seed_seq: 0,
            next_log_tick: None,
            needs_full: true,
            ack: None,
            warm_told: false,
            tokens: 0.0,
            refilled_ms: None,
            rate: 0.0,
            bytes_sent: 0,
            budget_bytes: 0.0,
            second_start_ms: 0,
            second_bytes: 0,
            second_budget: 0.0,
            last_second_bytes: 0,
            seconds_over_budget: 0,
            fell_behind: 0,
            fell_behind_seen: 0,
            lag: LagHist::default(),
            lag_now: 0,
        }
    }

    /// The last acknowledgment, when it is of the journal this successor was last seeded with:
    /// one that has not seen that full state whole describes the journal before it.
    fn seeded_ack(&self) -> Option<Ack> {
        self.ack.filter(|a| a.seen_seq >= self.seed_seq)
    }

    fn acked_tick(&self) -> Option<u64> {
        self.seeded_ack()
            .map(|a| a.applied_tick)
            .filter(|t| *t != u64::MAX)
    }

    fn warm(&self) -> bool {
        !self.needs_full && self.seeded_ack().is_some_and(|a| a.state == ShadowState::Warm)
    }

    fn state(&self) -> &'static str {
        if self.warm() {
            "warm"
        } else if self.ack.is_some_and(|a| a.state == ShadowState::Stale) {
            "stale"
        } else {
            "seeding"
        }
    }

    /// The burst an idle bucket may hold, bytes: two ticks of the rate, and at least one
    /// payload, so a successor that waited for its stream can fill the payload it sends next.
    fn burst(&self, tick_hz: u32, part_bytes: usize) -> f64 {
        (self.rate * BURST_TICKS / f64::from(tick_hz.max(1))).max(part_bytes as f64)
    }

    fn refill(&mut self, now_ms: u64, rate: f64, tick_hz: u32, part_bytes: usize) {
        let last = self.refilled_ms.replace(now_ms);
        let Some(last) = last else {
            self.second_start_ms = now_ms;
            self.rate = rate;
            return;
        };
        let dt_ms = now_ms.saturating_sub(last);
        // The rate held since the last refill pays for the time since then.
        let add = self.rate * dt_ms as f64 / 1_000.0;
        self.tokens += add;
        self.budget_bytes += add;
        self.second_budget += add;
        self.rate = rate;
        while now_ms.saturating_sub(self.second_start_ms) >= 1_000 {
            if self.second_bytes as f64 > self.second_budget + self.burst(tick_hz, part_bytes) {
                self.seconds_over_budget += 1;
            }
            self.last_second_bytes = self.second_bytes;
            self.second_bytes = 0;
            self.second_budget = 0.0;
            self.second_start_ms += 1_000;
        }
    }

    /// Takes `len` bytes of budget, or says no.
    fn spend(&mut self, len: usize) -> bool {
        if self.tokens < len as f64 {
            return false;
        }
        self.tokens -= len as f64;
        self.bytes_sent += len as u64;
        self.second_bytes += len as u64;
        true
    }
}

/// The host's side of the successor journal: it picks the successors, keeps the log ring and the
/// newest full state, and hands out each successor's payloads inside its budget.
///
/// Per tick, after the game stepped `tick`:
///
/// ```text
/// if host.full_wanted(now_ms) {
///     let digest = world.digest();
///     host.record_tick(tick, &inputs, &events, Some(digest))?;
///     host.record_full(now_ms, tick, world.encode(), digest)?;
/// } else {
///     host.record_tick(tick, &inputs, &events, None)?;
/// }
/// host.pump_server(now_ms, &mut server, wrap);   // or set_candidates + poll by hand
/// ```
#[derive(Debug)]
pub struct JournalHost {
    config: JournalConfig,
    appointments: u32,
    successors: Vec<Successor>,
    /// The last `depth_ticks` records, contiguous.
    ring: VecDeque<JournalRecord>,
    newest_tick: Option<u64>,
    full: Option<Arc<FullState>>,
    full_seq: u32,
    last_full_ms: Option<u64>,
    last_rerank_ms: Option<u64>,
    last_candidates_ms: Option<u64>,
    /// Each candidate's smoothed score, and when the scores were last moved.
    scores: Vec<(PeerId, f32)>,
    scores_ms: Option<u64>,
    control: VecDeque<JournalOut>,
    held: VecDeque<JournalOut>,
    events: VecDeque<JournalEvent>,
    full_states: u64,
    successor_changes: u64,
    refused: u64,
    last_refusal: Option<&'static str>,
}

impl JournalHost {
    /// A host journal with these settings (sanitized).
    pub fn new(config: JournalConfig) -> Self {
        Self {
            config: config.sanitized(),
            appointments: 0,
            successors: Vec::new(),
            ring: VecDeque::new(),
            newest_tick: None,
            full: None,
            full_seq: 0,
            last_full_ms: None,
            last_rerank_ms: None,
            last_candidates_ms: None,
            scores: Vec::new(),
            scores_ms: None,
            control: VecDeque::new(),
            held: VecDeque::new(),
            events: VecDeque::new(),
            full_states: 0,
            successor_changes: 0,
            refused: 0,
            last_refusal: None,
        }
    }

    /// The settings.
    pub fn config(&self) -> &JournalConfig {
        &self.config
    }

    /// The successors, first successor first.
    pub fn successors(&self) -> Vec<PeerId> {
        self.successors.iter().map(|s| s.peer).collect()
    }

    /// Whether `peer`'s shadow is warm, by its last acknowledgment.
    pub fn is_warm(&self, peer: PeerId) -> bool {
        self.successors.iter().any(|s| s.peer == peer && s.warm())
    }

    /// The newest journaled tick.
    pub fn newest_tick(&self) -> Option<u64> {
        self.newest_tick
    }

    /// The next thing to tell the game.
    pub fn poll_event(&mut self) -> Option<JournalEvent> {
        self.events.pop_front()
    }

    /// Gives the host the clients that may be successors now (players that are connected), with
    /// their link numbers. The host ranks them by a score smoothed over
    /// [`JournalConfig::score_smooth_ms`]. A successor missing from the list has left; an interrupted one is
    /// replaced; a vacancy goes to the best connected candidate; and every
    /// [`JournalConfig::rerank_ms`] a candidate takes the place of a sitting successor that fell
    /// out of the journal and is not warm again (when the candidate's link is not starved), or
    /// of the worst sitting successor when it is better connected by the switch margin. When the first successor goes, the second
    /// becomes the first and keeps its shadow.
    pub fn set_candidates(&mut self, now_ms: u64, candidates: &[Candidate]) {
        let find = |peer: PeerId| candidates.iter().find(|c| c.peer == peer);
        let before: Vec<(PeerId, u8)> = self.successors.iter().map(|s| (s.peer, s.rank)).collect();
        // Successors that left or went silent.
        let mut i = 0;
        while i < self.successors.len() {
            let peer = self.successors[i].peer;
            let reason = match find(peer) {
                None => Some("left"),
                Some(c) if c.interrupted => Some("interrupted"),
                Some(_) => None,
            };
            if let Some(reason) = reason {
                self.dismiss(i, reason, reason != "left");
            } else {
                i += 1;
            }
        }
        // Smoothed scores: a new client starts at its score now.
        let dt_ms = self
            .scores_ms
            .replace(now_ms)
            .map_or(0, |t| now_ms.saturating_sub(t));
        let follow = if self.config.score_smooth_ms == 0 {
            1.0
        } else {
            1.0 - (-(dt_ms as f32) / self.config.score_smooth_ms as f32).exp()
        };
        self.scores.retain(|(peer, _)| find(*peer).is_some());
        for c in candidates {
            let now_score = c.score_ms();
            match self.scores.iter_mut().find(|(peer, _)| *peer == c.peer) {
                Some((_, score)) => *score += follow * (now_score - *score),
                None => self.scores.push((c.peer, now_score)),
            }
        }
        let scores = &self.scores;
        let smooth = |c: &Candidate| {
            scores
                .iter()
                .find(|(peer, _)| *peer == c.peer)
                .map_or_else(|| c.score_ms(), |(_, score)| *score)
        };
        let ranked = rank_scored(candidates, smooth);
        // The comparison of sitting successors with the other candidates.
        let mut replaced: Option<PeerId> = None;
        let rerank_due = self
            .last_rerank_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= self.config.rerank_ms);
        if rerank_due {
            self.last_rerank_ms = Some(now_ms);
            // A successor that fell out of the journal since the last comparison and is not warm
            // again: its link does not carry the journal.
            let failing = self
                .successors
                .iter()
                .position(|s| s.fell_behind > s.fell_behind_seen && !s.warm());
            for s in &mut self.successors {
                s.fell_behind_seen = s.fell_behind;
            }
            // The worst sitting successor by score.
            let worst = self
                .successors
                .iter()
                .enumerate()
                .filter_map(|(i, s)| find(s.peer).map(|c| (i, c.starved(), smooth(c))))
                .max_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2)));
            let best = ranked
                .iter()
                .filter(|p| !self.successors.iter().any(|s| s.peer == **p))
                .filter_map(|p| find(*p))
                .find(|c| !c.interrupted)
                .map(|c| (c.starved(), smooth(c)));
            if self.successors.len() >= self.config.successors
                && let Some((best_starved, best_score)) = best
            {
                if let Some(at) = failing
                    && !best_starved
                {
                    replaced = Some(self.successors[at].peer);
                    self.dismiss(at, "fell_behind", true);
                } else if let Some((worst_at, worst_starved, worst_score)) = worst
                    && worst_starved == best_starved
                    && best_score < worst_score * self.config.switch_factor
                    && worst_score - best_score >= self.config.switch_min_ms
                {
                    replaced = Some(self.successors[worst_at].peer);
                    self.dismiss(worst_at, "outranked", true);
                }
            }
        }
        // Vacancies go to the best connected candidates.
        for peer in ranked {
            if self.successors.len() >= self.config.successors {
                break;
            }
            if self.successors.iter().any(|s| s.peer == peer)
                || replaced == Some(peer)
                || find(peer).is_none_or(|c| c.interrupted)
            {
                continue;
            }
            self.appointments = self.appointments.wrapping_add(1).max(1);
            let rank = self.successors.len() as u8;
            self.successors
                .push(Successor::new(peer, self.appointments, rank));
        }
        for (i, s) in self.successors.iter_mut().enumerate() {
            s.rank = i as u8;
        }
        let after: Vec<(PeerId, u8)> = self.successors.iter().map(|s| (s.peer, s.rank)).collect();
        if before != after {
            self.successor_changes += 1;
            for (peer, rank) in after {
                if !before.contains(&(peer, rank)) {
                    self.events.push_back(JournalEvent::Appointed { peer, rank });
                }
            }
        }
    }

    fn dismiss(&mut self, at: usize, reason: &'static str, tell: bool) {
        let s = self.successors.remove(at);
        self.held.retain(|o| o.peer != s.peer);
        if tell && s.told_rank.is_some() {
            self.control.push_back(JournalOut {
                peer: s.peer,
                bytes: encode_dismiss(s.appointment),
            });
        }
        self.events
            .push_back(JournalEvent::Dismissed { peer: s.peer, reason });
    }

    /// Journals one tick: the inputs the host stepped `tick` with and the events of that tick,
    /// and the world's digest after it on a tick a full state is taken (any tick may carry one).
    /// Ticks go up by one; after a jump the log starts again and every successor is seeded anew.
    pub fn record_tick(
        &mut self,
        tick: u64,
        inputs: &[u8],
        events: &[u8],
        digest: Option<WorldDigest>,
    ) -> Result<(), JournalError> {
        for blob in [inputs, events] {
            if blob.len() > MAX_JOURNAL_RECORD_BYTES {
                return Err(JournalError::RecordTooLarge {
                    len: blob.len(),
                    max: MAX_JOURNAL_RECORD_BYTES,
                });
            }
        }
        if let Some(newest) = self.newest_tick {
            if tick <= newest {
                return Err(JournalError::TickNotAfter { tick, newest });
            }
            // The lag each successor has just before this tick is journaled.
            for s in &mut self.successors {
                if let Some(acked) = s.acked_tick() {
                    s.lag_now = newest.saturating_sub(acked);
                    s.lag.add(s.lag_now);
                }
            }
            if tick != newest + 1 {
                self.restart_log();
            }
        }
        self.newest_tick = Some(tick);
        self.ring.push_back(JournalRecord {
            tick,
            inputs: inputs.into(),
            events: events.into(),
            digest,
        });
        while self.ring.len() > self.config.depth_ticks as usize {
            self.ring.pop_front();
        }
        Ok(())
    }

    /// The log lost its continuity: every successor needs a full state taken after the jump.
    fn restart_log(&mut self) {
        self.ring.clear();
        self.full = None;
        self.last_full_ms = None;
        for s in &mut self.successors {
            s.needs_full = true;
            s.sending = None;
            s.next_log_tick = None;
        }
        self.held.clear();
    }

    /// Whether the game should take a full state now ([`Self::record_full`]): there is a
    /// successor, and the newest full state is older than [`JournalConfig::full_every_ms`] and
    /// a successor is free to start the next (one still receiving an older full state finishes
    /// it first), or it cannot seed a successor that needs one (it is too old, or that shadow
    /// failed on it).
    pub fn full_wanted(&self, now_ms: u64) -> bool {
        if self.successors.is_empty() {
            return false;
        }
        let (Some(full), Some(at)) = (&self.full, self.last_full_ms) else {
            return true;
        };
        let age = now_ms.saturating_sub(at);
        // On the cadence, when a successor is free to start it.
        (age >= self.config.full_every_ms && self.successors.iter().any(|s| s.sending.is_none()))
            || (age >= EXTRA_FULL_GAP_MS
                && self.successors.iter().any(|s| {
                    s.needs_full && s.sending.is_none() && (s.started_seq == full.seq || !self.seeds(full))
                }))
    }

    /// Whether `full` can still seed a successor: the ring holds every tick after it.
    fn seeds(&self, full: &FullState) -> bool {
        match (self.ring.front(), self.newest_tick) {
            (Some(front), Some(newest)) => {
                (front.tick <= full.tick + 1 || full.tick == newest)
                    && newest.saturating_sub(full.tick) < u64::from(self.config.depth_ticks)
            }
            _ => true,
        }
    }

    /// Stores the full state the game took after `tick` (the newest journaled tick), with the
    /// world's digest. One encoding serves every successor.
    pub fn record_full(
        &mut self,
        now_ms: u64,
        tick: u64,
        bytes: Vec<u8>,
        digest: WorldDigest,
    ) -> Result<(), JournalError> {
        if bytes.len() > MAX_JOURNAL_FULL_BYTES {
            return Err(JournalError::FullTooLarge {
                len: bytes.len(),
                max: MAX_JOURNAL_FULL_BYTES,
            });
        }
        match self.newest_tick {
            Some(newest) if newest != tick => return Err(JournalError::FullTickUnknown),
            Some(_) => {}
            // A full state before the first tick starts the journal.
            None => self.newest_tick = Some(tick),
        }
        if let Some(record) = self.ring.back_mut()
            && record.tick == tick
            && record.digest.is_none()
        {
            record.digest = Some(digest);
        }
        self.full_seq = self.full_seq.wrapping_add(1).max(1);
        self.full = Some(Arc::new(FullState {
            seq: self.full_seq,
            tick,
            bytes,
            digest,
        }));
        self.last_full_ms = Some(now_ms);
        self.full_states += 1;
        Ok(())
    }

    /// Puts back a payload the transport refused (backpressure): the next [`Self::poll`] returns
    /// it first, and nothing new is made for that peer until it has left.
    pub fn hold(&mut self, out: JournalOut) {
        self.held.push_back(out);
    }

    /// The payloads to send now, each on its peer's bulk stream, in order: at most one per
    /// successor. `allowance` is the rate controller's allowance for a peer, bytes per second
    /// (`NetServer::send_allowance`); the journal spends [`JournalConfig::budget_share`] of it.
    /// This form does not look at the stream: use [`Self::poll_links`] on a transport that
    /// fails a connection whose stream stalls.
    pub fn poll(&mut self, now_ms: u64, mut allowance: impl FnMut(PeerId) -> Option<u32>) -> Vec<JournalOut> {
        self.poll_links(now_ms, |peer| JournalLink {
            allowance_bytes_per_s: allowance(peer),
            in_flight_fragments: 0,
        })
    }

    /// [`Self::poll`] with each successor's link: its send allowance and how many fragments of
    /// its bulk stream the peer has not acknowledged yet (`NetServer::stream_status`). A
    /// successor gets a new payload only while at most one fragment is on its way, and none
    /// when its allowance is under [`JOURNAL_STOP_AND_WAIT_BYTES_PER_S`]: with one fragment per
    /// payload, no later fragment then waits at the receiver behind one whose repair the sender's
    /// pacing holds back, which is what makes the transport fail a connection
    /// (`reassembly_stalled`).
    pub fn poll_links(
        &mut self,
        now_ms: u64,
        mut link: impl FnMut(PeerId) -> JournalLink,
    ) -> Vec<JournalOut> {
        let mut out: Vec<JournalOut> = self.held.drain(..).collect();
        let blocked: Vec<PeerId> = out.iter().map(|o| o.peer).collect();
        out.extend(self.control.drain(..));
        let part_bytes = self.config.part_bytes;
        let tick_hz = self.config.tick_hz;
        let depth = u64::from(self.config.depth_ticks);
        let newest = self.newest_tick;
        let ring_front = self.ring.front().map(|r| r.tick);
        let seeds = self.full.as_ref().is_some_and(|f| self.seeds(f));
        for s in &mut self.successors {
            let link = link(s.peer);
            let allowed = link
                .allowance_bytes_per_s
                .unwrap_or(self.config.fallback_bytes_per_s);
            s.refill(
                now_ms,
                f64::from(allowed) * f64::from(self.config.budget_share),
                tick_hz,
                part_bytes,
            );
            let in_flight_limit = usize::from(
                link.allowance_bytes_per_s
                    .is_none_or(|a| a >= JOURNAL_STOP_AND_WAIT_BYTES_PER_S),
            );
            if blocked.contains(&s.peer) || link.in_flight_fragments > in_flight_limit {
                // Nothing new while the stream still carries what it was handed.
                s.tokens = s.tokens.min(s.burst(tick_hz, part_bytes));
                continue;
            }
            // What this poll hands the successor: one payload of at most `part_bytes` (a message
            // that is larger alone goes alone).
            let mut bundle: Vec<u8> = Vec::new();
            let mut waiting = false;
            'peer: {
                if s.told_rank != Some(s.rank) {
                    let bytes = encode_appoint(s.appointment, s.rank, self.config.depth_ticks);
                    if !s.spend(bytes.len()) {
                        waiting = true;
                        break 'peer;
                    }
                    s.told_rank = Some(s.rank);
                    bundle.extend_from_slice(&bytes);
                }
                // Start a full state: the first one, a fresh seed, or the newest on the cadence.
                if s.sending.is_none()
                    && let Some(full) = &self.full
                    && full.seq != s.started_seq
                    && (!s.needs_full || seeds)
                {
                    let reseed = s.needs_full;
                    if reseed {
                        s.next_log_tick = Some(full.tick + 1);
                        s.needs_full = false;
                        s.sent = None;
                        s.seed_seq = full.seq;
                        s.warm_told = false;
                    }
                    s.started_seq = full.seq;
                    s.sending = Some(Sending {
                        full: Arc::clone(full),
                        reseed,
                        begun: false,
                        offset: 0,
                    });
                }
                if s.needs_full {
                    break 'peer;
                }
                // A first full state: its start names the tick the log follows, so it goes
                // before the log. A later one goes after the log, so the shadow has replayed
                // through its tick and can compare digests before the full state arrives.
                if s.sent.is_none()
                    && let Some(sending) = &mut s.sending
                    && !sending.begun
                {
                    let bytes = encode_full_begin(s.appointment, &sending.full, sending.reseed);
                    if s.tokens < bytes.len() as f64 {
                        waiting = true;
                        break 'peer;
                    }
                    sending.begun = true;
                    s.spend(bytes.len());
                    bundle.extend_from_slice(&bytes);
                }
                // The log, oldest first, several ticks to a message when it is behind.
                while let (Some(next), Some(newest), Some(front)) = (s.next_log_tick, newest, ring_front) {
                    if next > newest {
                        break;
                    }
                    let Some(base_tick) = s
                        .sent
                        .map(|(_, tick)| tick)
                        .or(s.sending.as_ref().map(|sending| sending.full.tick))
                    else {
                        break;
                    };
                    if next < front {
                        // The ring no longer holds the next record: only a full state taken
                        // since helps (`full_wanted` asks for one).
                        s.fell_behind += 1;
                        s.needs_full = true;
                        s.next_log_tick = None;
                        s.sending = None;
                        break 'peer;
                    }
                    if next > base_tick + depth {
                        // The window after its full state is used up: the log waits for the
                        // next full state (the one on its way, or the next on the cadence).
                        break;
                    }
                    let limit = newest.min(base_tick + depth);
                    let start = (next - front) as usize;
                    let room = part_bytes.saturating_sub(bundle.len());
                    let mut count = 0;
                    let mut len = LOG_HEADER_BYTES;
                    for r in self.ring.iter().skip(start) {
                        if r.tick > limit || count >= MAX_LOG_BATCH {
                            break;
                        }
                        let add = record_wire_len(r);
                        // A record that does not fit the payload waits for the next one, unless
                        // it is larger than a payload alone.
                        if len + add > room && (count > 0 || !bundle.is_empty()) {
                            break;
                        }
                        len += add;
                        count += 1;
                    }
                    if count == 0 {
                        // The payload is full: the rest of the log goes with the next.
                        break 'peer;
                    }
                    if !s.spend(len) {
                        waiting = true;
                        break 'peer;
                    }
                    let bytes = encode_log(s.appointment, next, self.ring.iter().skip(start).take(count));
                    debug_assert_eq!(bytes.len(), len);
                    s.next_log_tick = Some(next + count as u64);
                    bundle.extend_from_slice(&bytes);
                }
                // The full state with the budget and the room that are left.
                while let Some(sending) = &mut s.sending {
                    let room = part_bytes.saturating_sub(bundle.len());
                    if !sending.begun {
                        let bytes = encode_full_begin(s.appointment, &sending.full, sending.reseed);
                        if bytes.len() > room && !bundle.is_empty() {
                            break 'peer;
                        }
                        if s.tokens < bytes.len() as f64 {
                            waiting = true;
                            break 'peer;
                        }
                        sending.begun = true;
                        s.spend(bytes.len());
                        bundle.extend_from_slice(&bytes);
                        continue;
                    }
                    let left = sending.full.bytes.len() - sending.offset;
                    if left > 0 {
                        let affordable = (s.tokens as usize).saturating_sub(FULL_DATA_HEADER_BYTES);
                        let len = left
                            .min(room.saturating_sub(FULL_DATA_HEADER_BYTES))
                            .min(affordable);
                        if len < left.min(MIN_PART_BYTES) {
                            break 'peer;
                        }
                        let bytes = encode_full_data(s.appointment, &sending.full, sending.offset, len);
                        sending.offset += len;
                        s.spend(bytes.len());
                        bundle.extend_from_slice(&bytes);
                        continue;
                    }
                    let (seq, tick) = (sending.full.seq, sending.full.tick);
                    s.sent = Some((seq, tick));
                    s.sending = None;
                    // A log that waited at the window never reached this full state's tick: the
                    // shadow loads the full state, and the log goes on after it.
                    if s.next_log_tick.is_some_and(|next| next <= tick) {
                        s.next_log_tick = Some(tick + 1);
                    }
                }
            }
            if !waiting {
                s.tokens = s.tokens.min(s.burst(tick_hz, part_bytes));
            }
            if !bundle.is_empty() {
                out.push(JournalOut {
                    peer: s.peer,
                    bytes: bundle,
                });
            }
        }
        out
    }

    /// Takes a journal payload a client sent: a successor's acknowledgment.
    pub fn on_payload(&mut self, peer: PeerId, bytes: &[u8]) -> Result<(), JournalError> {
        let result = self.take(peer, bytes);
        if let Err(e) = &result {
            self.refused += 1;
            self.last_refusal = Some(e.code());
        }
        result
    }

    fn take(&mut self, peer: PeerId, bytes: &[u8]) -> Result<(), JournalError> {
        for msg in decode_all(bytes)? {
            let Msg::Ack(ack) = msg else {
                return Err(JournalError::Unexpected);
            };
            self.take_ack(peer, ack)?;
        }
        Ok(())
    }

    fn take_ack(&mut self, peer: PeerId, ack: Ack) -> Result<(), JournalError> {
        let Some(s) = self.successors.iter_mut().find(|s| s.peer == peer) else {
            return Err(JournalError::NotSuccessor);
        };
        if ack.appointment != s.appointment {
            return Err(JournalError::StaleAppointment);
        }
        // Acknowledgments may arrive out of order: keep the newest.
        if let Some(old) = s.ack
            && (old.seen_seq, old.applied_tick.wrapping_add(1))
                > (ack.seen_seq, ack.applied_tick.wrapping_add(1))
            && old.refused >= ack.refused
        {
            return Ok(());
        }
        s.ack = Some(ack);
        if s.warm() && !s.warm_told {
            s.warm_told = true;
            self.events.push_back(JournalEvent::Warm { peer });
        }
        // A shadow that is stale after everything sent so far needs a full state again
        // (`full_wanted` asks the game for a new one when the newest already failed there).
        let caught_up = s.sending.is_none() && s.sent.is_some_and(|(seq, _)| ack.seen_seq >= seq);
        if ack.state == ShadowState::Stale && caught_up && !s.needs_full {
            s.needs_full = true;
            s.next_log_tick = None;
            s.warm_told = false;
        }
        Ok(())
    }

    /// Does the host's side on a server: every second it reads the players' link numbers
    /// (`NetServer::peer_rows`) for [`Self::set_candidates`], and on every call it sends what
    /// [`Self::poll`] hands out on each successor's bulk stream, inside the share of
    /// `NetServer::send_allowance`. `wrap` puts a payload into the game's message type. Returns
    /// the bytes handed to the transport. Route the acknowledgments the successors send back to
    /// [`Self::on_payload`].
    pub fn pump_server<G: NetGame, S: DatagramSocket>(
        &mut self,
        now_ms: u64,
        server: &mut NetServer<G, S>,
        wrap: impl Fn(Vec<u8>) -> G::Message,
    ) -> usize {
        // Every second, and at once when a successor has left.
        let due = self
            .last_candidates_ms
            .is_none_or(|t| now_ms.saturating_sub(t) >= 1_000);
        let gone = self
            .successors
            .iter()
            .any(|s| server.session().peer(s.peer).is_none());
        if due || gone {
            self.last_candidates_ms = Some(now_ms);
            let candidates: Vec<Candidate> = server
                .session()
                .peers()
                .filter(|p| p.role == crate::handshake::PeerRole::Player)
                .map(|p| Candidate {
                    peer: p.id,
                    rtt_ms: p.link.rtt_ms as f32,
                    jitter_ms: p.link.jitter_ms as f32,
                    loss_fraction: p.link.loss_fraction,
                    interrupted: p.interrupted,
                    allowance_bytes_per_s: server.send_allowance(p.id),
                })
                .collect();
            self.set_candidates(now_ms, &candidates);
        }
        let outs = self.poll_links(now_ms, |peer| JournalLink {
            allowance_bytes_per_s: server.send_allowance(peer),
            in_flight_fragments: server
                .stream_status(peer, StreamId::BULK)
                .map_or(0, |s| s.queued_fragments),
        });
        let mut sent = 0;
        let mut stuck: Vec<PeerId> = Vec::new();
        for out in outs {
            if stuck.contains(&out.peer) {
                self.hold(out);
                continue;
            }
            let len = out.bytes.len();
            let message = wrap(out.bytes.clone());
            match server.send(out.peer, Channel::Stream(StreamId::BULK), &message) {
                Ok(()) => sent += len,
                Err(SendError::NotConnected) => {}
                Err(e @ (SendError::TooLarge { .. } | SendError::UnknownStream(_))) => {
                    // The game's message type or stream table cannot carry the journal: nothing
                    // a retry would fix. The successor reports the gap and stays cold.
                    tracing::warn!(peer = out.peer.0, error = %e, "journal payload not sendable");
                }
                Err(_) => {
                    stuck.push(out.peer);
                    self.hold(out);
                }
            }
        }
        sent
    }

    /// What `net.migration` reports for the host.
    pub fn status(&self) -> JournalStatus {
        let rows: Vec<SuccessorRow> = self
            .successors
            .iter()
            .map(|s| SuccessorRow {
                peer: s.peer.0,
                rank: s.rank,
                state: s.state().to_owned(),
                acked_tick: s.acked_tick(),
                lag_ticks: s.lag_now,
                lag_ticks_p99: s.lag.p99(),
                lag_ticks_max: s.lag.max,
                queued_ticks: match (s.next_log_tick, self.newest_tick) {
                    (Some(next), Some(newest)) => (newest + 1).saturating_sub(next),
                    _ => 0,
                },
                full_seq: s.sent.map_or(0, |(seq, _)| seq),
                bytes_sent: s.bytes_sent,
                bytes_per_s: s.last_second_bytes,
                budget_bytes_per_s: s.rate as u64,
                budget_bytes: s.budget_bytes as u64,
                seconds_over_budget: s.seconds_over_budget,
                digest_mismatches: s.ack.map_or(0, |a| u64::from(a.digest_mismatches)),
                replay_divergences: s.ack.map_or(0, |a| u64::from(a.replay_divergences)),
                fell_behind: s.fell_behind,
            })
            .collect();
        let warm = rows.iter().filter(|r| r.state == "warm").count();
        JournalStatus {
            role: "host".to_owned(),
            state: if rows.is_empty() {
                "no_successor"
            } else if warm == 0 {
                "seeding"
            } else {
                "warm"
            }
            .to_owned(),
            tick: self.newest_tick,
            journal_lag_ticks: rows.iter().map(|r| r.lag_ticks).max().unwrap_or(0),
            journal_lag_ticks_p99: rows.iter().map(|r| r.lag_ticks_p99).max().unwrap_or(0),
            rank: None,
            base_tick: self.full.as_ref().map(|f| f.tick),
            full_states: self.full_states,
            full_state_bytes: self.full.as_ref().map_or(0, |f| f.bytes.len() as u64),
            log_ticks: self.ring.len() as u64,
            depth_ticks: self.config.depth_ticks,
            digest_checks: 0,
            digest_mismatches: rows.iter().map(|r| r.digest_mismatches).sum(),
            replay_divergences: rows.iter().map(|r| r.replay_divergences).sum(),
            reloads: 0,
            successor_changes: self.successor_changes,
            refused: self.refused,
            last_refusal: self.last_refusal.map(str::to_owned),
            promotable: false,
            successors: rows,
        }
    }

    /// Publishes [`Self::status`] under `label` for the agent method `net.migration`.
    pub fn publish(&self, label: &str, state: &mut NetAgentState) {
        state.publish_migration(label, self.status());
    }
}

/// One successor in the host's `net.migration` report.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SuccessorRow {
    /// The successor's peer id.
    pub peer: u32,
    /// 0 is the first successor.
    pub rank: u8,
    /// `seeding`, `warm` or `stale`.
    pub state: String,
    /// The newest tick the successor acknowledged applying.
    pub acked_tick: Option<u64>,
    /// Journal lag, ticks: the newest journaled tick minus the acknowledged one, read just
    /// before the host journals a tick (it includes the acknowledgment's way back).
    pub lag_ticks: u64,
    /// The 99th percentile of the lag since the appointment.
    pub lag_ticks_p99: u64,
    /// The largest lag since the appointment.
    pub lag_ticks_max: u64,
    /// Journaled ticks not yet handed to the transport for this successor.
    pub queued_ticks: u64,
    /// The newest full state sent whole.
    pub full_seq: u32,
    /// Journal bytes handed to the transport for this successor.
    pub bytes_sent: u64,
    /// Journal bytes in the last whole second.
    pub bytes_per_s: u64,
    /// The journal's budget now: its share of the peer's send allowance, bytes per second.
    pub budget_bytes_per_s: u64,
    /// The budget accrued since the appointment, bytes; `bytes_sent` never exceeds it.
    pub budget_bytes: u64,
    /// Whole seconds in which the journal sent more than that second's budget plus what an idle
    /// bucket holds (two ticks of the rate, at least one payload).
    pub seconds_over_budget: u64,
    /// Full states the shadow loaded whose digest did not match.
    pub digest_mismatches: u64,
    /// Full-state ticks at which the shadow's replayed digest differed from the host's.
    pub replay_divergences: u64,
    /// Times the successor fell out of the host's log ring (its budget did not carry the journal
    /// for about two journal depths) and was seeded again.
    pub fell_behind: u64,
}

/// What `net.migration` reports for one published host journal or successor.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct JournalStatus {
    /// `host`, `successor` or `client` (a client that is not a successor).
    pub role: String,
    /// Host: `no_successor`, `seeding` or `warm` (at least one warm successor). Client: `idle`,
    /// `seeding`, `warm` or `stale`.
    pub state: String,
    /// Host: the newest journaled tick. Successor: the newest tick applied to the shadow.
    pub tick: Option<u64>,
    /// Journal lag, ticks. Host: the worst successor's (`successors[].lag_ticks`). Successor:
    /// log records it holds and has not applied.
    pub journal_lag_ticks: u64,
    /// Host: the worst successor's 99th-percentile lag.
    pub journal_lag_ticks_p99: u64,
    /// The host's successors, first successor first.
    pub successors: Vec<SuccessorRow>,
    /// Successor: its rank (0 is the first successor).
    pub rank: Option<u8>,
    /// The tick of the newest full state (taken on the host, held on a successor).
    pub base_tick: Option<u64>,
    /// Full states taken (host) or adopted and loaded (successor).
    pub full_states: u64,
    /// Bytes of the newest full state.
    pub full_state_bytes: u64,
    /// Ticks of log held (the host's ring, a successor's log after its full state).
    pub log_ticks: u64,
    /// The journal depth, ticks.
    pub depth_ticks: u32,
    /// Successor: full-state ticks at which the shadow's digest was compared with the host's.
    pub digest_checks: u64,
    /// Loaded full states whose digest did not match the host's.
    pub digest_mismatches: u64,
    /// Full-state ticks at which the replayed shadow differed from the host (the next full state
    /// repairs it).
    pub replay_divergences: u64,
    /// Successor: full states loaded into the shadow (the first one, and repairs).
    pub reloads: u64,
    /// Host: times the set of successors or their order changed.
    pub successor_changes: u64,
    /// Journal payloads refused.
    pub refused: u64,
    /// The code of the last refusal.
    pub last_refusal: Option<String>,
    /// Successor: the shadow can be promoted now.
    pub promotable: bool,
}

/// A full state arriving in parts.
#[derive(Debug)]
struct Incoming {
    seq: u32,
    tick: u64,
    total: usize,
    bytes: Vec<u8>,
    digest: WorldDigest,
}

/// The newest whole full state a successor holds.
#[derive(Debug)]
struct Base {
    seq: u32,
    tick: u64,
    bytes: Vec<u8>,
}

/// What a promoted successor starts the server from.
#[derive(Debug)]
pub struct Promotion<W> {
    /// The shadow world, at [`Self::tick`].
    pub world: W,
    /// The newest tick applied.
    pub tick: u64,
    /// The tick of the newest full state.
    pub base_tick: u64,
    /// The newest full state's bytes.
    pub base: Vec<u8>,
    /// The log after the full state, through [`Self::tick`].
    pub log: Vec<JournalRecord>,
}

/// A client's side of the successor journal. Every client may hold one; it stays idle until the
/// host appoints it. Feed it the journal payloads of the bulk stream and send what
/// [`Self::poll_ack`] returns back to the host (unreliable is fine).
#[derive(Debug)]
pub struct JournalSuccessor<W: ShadowWorld> {
    world: W,
    state: ShadowState,
    appointment: u32,
    rank: u8,
    depth_ticks: u32,
    base: Option<Base>,
    incoming: Option<Incoming>,
    /// The log after the base (or after the incoming full state, before the first base).
    log: VecDeque<JournalRecord>,
    /// The newest tick applied to the shadow.
    applied_tick: Option<u64>,
    /// The last ticks at which the shadow's digest matched the host's, with that digest.
    verified: VecDeque<(u64, WorldDigest)>,
    /// The shadow has differed from the host since the last full state.
    diverged: bool,
    seen_seq: u32,
    ack_due: bool,
    full_states: u64,
    reloads: u64,
    digest_checks: u64,
    digest_mismatches: u64,
    replay_divergences: u64,
    refused: u64,
    last_refusal: Option<&'static str>,
}

impl<W: ShadowWorld> JournalSuccessor<W> {
    /// An idle successor around the shadow world `world`.
    pub fn new(world: W) -> Self {
        Self {
            world,
            state: ShadowState::Idle,
            appointment: 0,
            rank: 0,
            depth_ticks: JOURNAL_DEPTH_TICKS,
            base: None,
            incoming: None,
            log: VecDeque::new(),
            applied_tick: None,
            verified: VecDeque::new(),
            diverged: false,
            seen_seq: 0,
            ack_due: false,
            full_states: 0,
            reloads: 0,
            digest_checks: 0,
            digest_mismatches: 0,
            replay_divergences: 0,
            refused: 0,
            last_refusal: None,
        }
    }

    /// The shadow world.
    pub fn world(&self) -> &W {
        &self.world
    }

    /// Whether the host appointed this client.
    pub fn is_successor(&self) -> bool {
        self.state != ShadowState::Idle
    }

    /// This successor's rank (0 is the first), while appointed.
    pub fn rank(&self) -> Option<u8> {
        self.is_successor().then_some(self.rank)
    }

    /// The newest tick applied to the shadow.
    pub fn applied_tick(&self) -> Option<u64> {
        self.applied_tick
    }

    /// The tick of the newest full state held.
    pub fn base_tick(&self) -> Option<u64> {
        self.base.as_ref().map(|b| b.tick)
    }

    /// Whether the shadow can be promoted: it holds a full state and has applied the log.
    pub fn promotable(&self) -> bool {
        self.state == ShadowState::Warm && self.base.is_some() && self.applied_tick.is_some()
    }

    /// Takes a journal payload from the host.
    pub fn on_payload(&mut self, bytes: &[u8]) -> Result<(), JournalError> {
        let result = self.take(bytes);
        if let Err(e) = &result {
            self.refused += 1;
            self.last_refusal = Some(e.code());
            self.ack_due = self.is_successor();
        }
        result
    }

    /// The journal is lost: drop it and wait for a full state.
    fn go_stale(&mut self) {
        self.state = ShadowState::Stale;
        self.base = None;
        self.incoming = None;
        self.log.clear();
        self.applied_tick = None;
        self.verified.clear();
        self.diverged = false;
    }

    fn take(&mut self, bytes: &[u8]) -> Result<(), JournalError> {
        for msg in decode_all(bytes)? {
            self.take_msg(msg)?;
        }
        Ok(())
    }

    fn take_msg(&mut self, msg: Msg<'_>) -> Result<(), JournalError> {
        match msg {
            Msg::Appoint {
                appointment,
                rank,
                depth_ticks,
            } => {
                if !self.is_successor() || appointment != self.appointment {
                    self.go_stale();
                    self.state = ShadowState::Seeding;
                    self.appointment = appointment;
                    self.seen_seq = 0;
                }
                self.rank = rank;
                self.depth_ticks = depth_ticks;
                self.ack_due = true;
                Ok(())
            }
            Msg::Dismiss { appointment } => {
                if self.is_successor() && appointment == self.appointment {
                    self.go_stale();
                    self.state = ShadowState::Idle;
                }
                Ok(())
            }
            Msg::Ack(_) => Err(JournalError::Unexpected),
            Msg::FullBegin {
                appointment,
                seq,
                reseed,
                tick,
                total,
                digest,
            } => {
                self.check(appointment)?;
                if reseed {
                    // The host starts this successor's journal again: what it held is behind
                    // the host's ring.
                    self.go_stale();
                    self.state = ShadowState::Seeding;
                }
                self.begin_full(seq, tick, total as usize, digest)
            }
            Msg::FullData {
                appointment,
                seq,
                offset,
                bytes,
            } => {
                self.check(appointment)?;
                self.take_full_data(seq, offset as usize, bytes)
            }
            Msg::Log {
                appointment_low,
                tick_low,
                records,
            } => {
                if !self.is_successor() {
                    return Err(JournalError::NotSuccessor);
                }
                if appointment_low != self.appointment as u8 {
                    return Err(JournalError::StaleAppointment);
                }
                // The first tick from its low bits, around the tick expected next.
                let expected = self.expected_tick()?;
                let delta = tick_low.wrapping_sub(expected as u16) as i16;
                let Some(first_tick) = expected.checked_add_signed(i64::from(delta)) else {
                    self.go_stale();
                    return Err(JournalError::Gap {
                        expected,
                        got: u64::from(tick_low),
                    });
                };
                for (i, r) in records.into_iter().enumerate() {
                    self.take_record(first_tick + i as u64, &r)?;
                }
                Ok(())
            }
        }
    }

    /// The tick the next log record should have.
    fn expected_tick(&self) -> Result<u64, JournalError> {
        // The full state the log follows: the one held, or the first one, still arriving.
        let base_tick = self
            .base
            .as_ref()
            .map(|b| b.tick)
            .or(self.incoming.as_ref().map(|i| i.tick))
            .ok_or(JournalError::NoBase)?;
        Ok(self.log.back().map_or(base_tick + 1, |r| r.tick + 1))
    }

    fn check(&self, appointment: u32) -> Result<(), JournalError> {
        if !self.is_successor() {
            Err(JournalError::NotSuccessor)
        } else if appointment != self.appointment {
            Err(JournalError::StaleAppointment)
        } else {
            Ok(())
        }
    }

    fn begin_full(
        &mut self,
        seq: u32,
        tick: u64,
        total: usize,
        digest: WorldDigest,
    ) -> Result<(), JournalError> {
        if self.base.as_ref().is_some_and(|b| seq <= b.seq) {
            // An older full state than the one held: nothing to do.
            return Ok(());
        }
        if self.state == ShadowState::Stale {
            // A new seed: the old journal is gone already.
            self.state = ShadowState::Seeding;
        }
        if self.base.is_none() {
            // The log restarts after this full state.
            self.log.clear();
        }
        self.incoming = Some(Incoming {
            seq,
            tick,
            total,
            bytes: Vec::new(),
            digest,
        });
        self.finish_if_whole()
    }

    fn take_full_data(&mut self, seq: u32, offset: usize, bytes: &[u8]) -> Result<(), JournalError> {
        if self.base.as_ref().is_some_and(|b| seq <= b.seq) {
            return Ok(());
        }
        let Some(inc) = &mut self.incoming else {
            // Bytes of a full state whose start was refused or never seen.
            return Err(JournalError::FullOutOfOrder);
        };
        if inc.seq != seq || inc.bytes.len() != offset || offset + bytes.len() > inc.total {
            self.incoming = None;
            self.seen_seq = self.seen_seq.max(seq);
            if self.base.is_none() {
                self.go_stale();
            }
            return Err(JournalError::FullOutOfOrder);
        }
        inc.bytes.extend_from_slice(bytes);
        self.finish_if_whole()
    }

    fn finish_if_whole(&mut self) -> Result<(), JournalError> {
        if self
            .incoming
            .as_ref()
            .is_none_or(|inc| inc.bytes.len() < inc.total)
        {
            return Ok(());
        }
        let Some(inc) = self.incoming.take() else {
            return Ok(());
        };
        self.seen_seq = inc.seq;
        self.ack_due = true;
        self.finish_full(inc)
    }

    /// A whole full state: adopt it when the shadow already matched the host at its tick, load
    /// it and replay the kept log otherwise.
    fn finish_full(&mut self, inc: Incoming) -> Result<(), JournalError> {
        let adopt = self.state == ShadowState::Warm
            && !self.diverged
            && self.verified.contains(&(inc.tick, inc.digest))
            && self.applied_tick.is_some_and(|a| a >= inc.tick);
        while self.log.front().is_some_and(|r| r.tick <= inc.tick) {
            self.log.pop_front();
        }
        if self.log.front().is_some_and(|r| r.tick != inc.tick + 1) {
            // The kept log does not continue this full state.
            self.log.clear();
        }
        if !adopt {
            if let Err(e) = self.world.load_full(inc.tick, &inc.bytes) {
                self.go_stale();
                return Err(JournalError::LoadFailed(e));
            }
            self.reloads += 1;
            self.digest_checks += 1;
            if self.world.digest() != inc.digest {
                self.digest_mismatches += 1;
                self.go_stale();
                return Err(JournalError::DigestMismatch { tick: inc.tick });
            }
            self.applied_tick = Some(inc.tick);
            self.verified.clear();
            self.verified.push_back((inc.tick, inc.digest));
            self.diverged = false;
            self.state = ShadowState::Warm;
            // Replay what arrived after the full state was taken.
            let kept: Vec<JournalRecord> = self.log.iter().cloned().collect();
            for record in &kept {
                self.apply(record)?;
            }
        }
        self.full_states += 1;
        self.base = Some(Base {
            seq: inc.seq,
            tick: inc.tick,
            bytes: inc.bytes,
        });
        Ok(())
    }

    fn take_record(&mut self, tick: u64, r: &RecordRef<'_>) -> Result<(), JournalError> {
        let Some(base_tick) = self
            .base
            .as_ref()
            .map(|b| b.tick)
            .or(self.incoming.as_ref().map(|i| i.tick))
        else {
            return Err(JournalError::NoBase);
        };
        let expected = self.log.back().map_or(base_tick + 1, |r| r.tick + 1);
        if tick < expected {
            // Already held (a full state overtook the log).
            return Ok(());
        }
        if tick > expected {
            self.go_stale();
            return Err(JournalError::Gap { expected, got: tick });
        }
        let limit = base_tick.saturating_add(u64::from(self.depth_ticks));
        if tick > limit {
            self.go_stale();
            return Err(JournalError::PastWindow { tick, limit });
        }
        let record = JournalRecord {
            tick,
            inputs: r.inputs.into(),
            events: r.events.into(),
            digest: r.digest,
        };
        self.log.push_back(record.clone());
        if self.state == ShadowState::Warm && self.applied_tick.is_some_and(|a| a + 1 == tick) {
            self.apply(&record)?;
        }
        Ok(())
    }

    fn apply(&mut self, record: &JournalRecord) -> Result<(), JournalError> {
        if let Err(e) = self.world.apply(record.tick, &record.inputs, &record.events) {
            self.go_stale();
            return Err(JournalError::ApplyFailed(e));
        }
        self.applied_tick = Some(record.tick);
        self.ack_due = true;
        if let Some(digest) = record.digest {
            self.digest_checks += 1;
            if self.world.digest() == digest {
                // A slow full state may still be on its way when the next full-state tick is
                // replayed: remember the last few.
                if self.verified.len() >= VERIFIED_KEPT {
                    self.verified.pop_front();
                }
                self.verified.push_back((record.tick, digest));
                self.diverged = false;
            } else {
                // The log did not reproduce the host's world: the full state of this tick, which
                // follows, repairs the shadow.
                self.replay_divergences += 1;
                self.diverged = true;
            }
        }
        Ok(())
    }

    /// The acknowledgment to send to the host, when something changed since the last one. It is
    /// cumulative: any channel will do, and a lost one is replaced by the next.
    pub fn poll_ack(&mut self) -> Option<Vec<u8>> {
        if !self.ack_due || !self.is_successor() {
            self.ack_due = false;
            return None;
        }
        self.ack_due = false;
        Some(encode_ack(&Ack {
            appointment: self.appointment,
            state: self.state,
            applied_tick: self.applied_tick.unwrap_or(u64::MAX),
            seen_seq: self.seen_seq,
            digest_mismatches: self.digest_mismatches.min(u64::from(u32::MAX)) as u32,
            replay_divergences: self.replay_divergences.min(u64::from(u32::MAX)) as u32,
            refused: self.refused.min(u64::from(u32::MAX)) as u32,
        }))
    }

    /// Hands over the shadow to become the server: the world at the newest applied tick, with
    /// the full state and the log it came from. `journal_not_warm` when the shadow is not
    /// promotable ([`Self::promotable`]).
    pub fn promote(self) -> Result<Promotion<W>, JournalError> {
        if !self.promotable() {
            return Err(JournalError::NotWarm);
        }
        match (self.base, self.applied_tick) {
            (Some(base), Some(tick)) => Ok(Promotion {
                world: self.world,
                tick,
                base_tick: base.tick,
                base: base.bytes,
                log: self.log.into_iter().filter(|r| r.tick <= tick).collect(),
            }),
            _ => Err(JournalError::NotWarm),
        }
    }

    /// What `net.migration` reports for this client.
    pub fn status(&self) -> JournalStatus {
        let newest_held = self.log.back().map(|r| r.tick).or(self.applied_tick);
        JournalStatus {
            role: if self.is_successor() {
                "successor"
            } else {
                "client"
            }
            .to_owned(),
            state: self.state.as_str().to_owned(),
            tick: self.applied_tick,
            journal_lag_ticks: match (newest_held, self.applied_tick) {
                (Some(held), Some(applied)) => held.saturating_sub(applied),
                _ => self.log.len() as u64,
            },
            journal_lag_ticks_p99: 0,
            successors: Vec::new(),
            rank: self.rank(),
            base_tick: self.base_tick(),
            full_states: self.full_states,
            full_state_bytes: self.base.as_ref().map_or(0, |b| b.bytes.len() as u64),
            log_ticks: self.log.len() as u64,
            depth_ticks: self.depth_ticks,
            digest_checks: self.digest_checks,
            digest_mismatches: self.digest_mismatches,
            replay_divergences: self.replay_divergences,
            reloads: self.reloads,
            successor_changes: 0,
            refused: self.refused,
            last_refusal: self.last_refusal.map(str::to_owned),
            promotable: self.promotable(),
        }
    }

    /// Publishes [`Self::status`] under `label` for the agent method `net.migration`.
    pub fn publish(&self, label: &str, state: &mut NetAgentState) {
        state.publish_migration(label, self.status());
    }
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
