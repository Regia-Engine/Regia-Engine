// SPDX-License-Identifier: MPL-2.0
//! The unreliable state channel (spec 72 § 4, protocol 2, ADR-1950): deltas against acknowledged
//! baselines.
//!
//! A connection on this channel gets its spawns and deltas in `STATE` frames on the unreliable
//! channel, one or more per send tick, each at most `replication.mtu_bytes`. Every frame has a
//! sequence number and stands alone: a lost one needs no resend, because the next delta for its
//! entities is computed against the baseline the client still holds. What must arrive stays on
//! the reliable tick frame of `crate::runtime`: ownership bits, despawns, out-of-scope notices and
//! reliable messages.
//!
//! ```text
//! STATE  := tag u8 | tick varint | seq varint | count varint | record*
//! record := id varint | bits varint | body (`bits` long, so one bad record is skipped alone)
//! body   := kind u2 | version_ago ago | spawn fields | update fields | far fields
//! kind   := 0 spawn | 1 repeat spawn | 2 update | 3 far update
//! update := base_ago ago | the update fields of a tick frame, whose transform is
//!           teleport u1 | delta u1 | (delta against the baseline's transform | mode u2 | record)
//! far    := base_ago ago | far position (`transform_record`) | entries
//! ago    := 0 (this tick) | 1 0 u5 (1..=32) | 1 1 varint (33 and more)
//! entries:= 0 (none) | 1 varint (count - 1) | entry*
//! ACK    := tag u8 | tick varint | protocol varint | newest+1 varint | words u8 | u64*
//! ```
//!
//! `version_ago` is the packet tick minus the state version's tick, `base_ago` the version's tick
//! minus the baseline's. Protocol 4 (spec 120 § 8.2, ADR-8467) brought the `ago` and entry-count
//! codes and the quantized transform record with its delta form (`transform_record`). The acknowledgment names the newest state sequence received and a bit for
//! each of the 256 before it, so a lost `ACK` costs nothing: the next one carries the same bits.
//!
//! **Far updates (kind 3, spec 120 § 8.3, ADR-10124).** An update of a far-tier copy whose
//! transform fits the far record carries the coarse position instead of the transform fields; its
//! component entries are an update's. The client keeps such a version as a *far version*: a
//! baseline for component deltas and for further far records, never for a transform delta. An
//! exact record of the same version (a spawn or an update) replaces it, whichever arrives first,
//! so the server, which only learns what arrived from acknowledgments, never has to know which
//! of the two the client applied first: after a far baseline it sends the transform absolute.
//!
//! **Negotiation.** A protocol 1 peer ignores what follows the tick in an `ACK`, and a protocol 1
//! client always answers its first frame with one, so the first `ACK` decides: the server holds a
//! new connection's state back behind empty tick frames until it arrives. With the extension the
//! connection takes this channel; without it the connection keeps the reliable tick stream
//! (protocol 1). A client takes whichever frames the server sends.
//!
//! **Entropy (protocol 3, spec 72 § 18, ADR-3315).** A connection whose first `ACK` names
//! protocol ≥ 3, on a server configured for it, receives `TAG_STATE_ENTROPY` frames instead of
//! `STATE` frames; a protocol 2 peer never sees one, so its frames stay byte-identical:
//!
//! ```text
//! STATE_ENTROPY := tag u8(8) | tick varint | seq varint | flags u8 | body
//!   flags 0 (raw fallback): count varint | record*
//!   flags 1 (range-coded):  coded_len varint | coded bytes (the count+records, range-coded;
//!                           decoded length in the payload's varint header)
//! ```

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

use regia_ecs::prelude::*;

use super::{
    ApplyOutcome, ClientReplication, ENTRY_DELTA, ENTRY_FULL, ENTRY_REMOVED, ErasedComp, LevelIndex,
    NetEvents, NetOutOfScope, NetSpawned, NetTransform, NetTransformMode, NetUnbound, QTransform,
    ReplicationRegistry, SpawnKind, StateVersion, TransformReplication, create_copy, note_transform,
    proxy_state, read_net_id, read_u128, record_refs, set_owned, set_parent, skip_bits, transform_mode_from,
    write_transform,
};
use crate::bits::{BitReader, BitWriter, DecodeError, varint_bits};
use crate::codec::CodecContext;
use crate::message::Channel;
use crate::model::{NetId, NetTick};
use crate::runtime::NetEntities;

/// Sent packets the server remembers per connection, and the reach of an acknowledgment.
pub const STATE_PACKET_LOG: usize = 256;
/// The most versions an entity keeps on a client; a baseline this many versions old gets a full
/// state. A wire constant: the server reads it as what the client still holds, so it changes only
/// with [`crate::payload::REGIA_NET_WIRE_VERSION`] (512 since wire 6, ADR-10051; 64 before).
///
/// Sized for spec 120's `rtt-extreme` profile (1,500 ms one way): the baseline of an entity that
/// changes every tick is one round trip of versions behind, 180 at 60 Hz and 384 at 128 Hz, plus
/// the acknowledgments a 20 % loss delays. 512 holds a round trip of 4 s at 128 Hz, 8.5 s at
/// 60 Hz and 17 s at 30 Hz. A client does not store that many on a shorter path: it lets go of
/// the versions older than the baseline the server names ([`STATE_BASE_REORDER_TICKS`]).
pub const STATE_VERSION_RING: u32 = 512;
/// Ticks of reordering a client's version ring allows for (ADR-10051). The server's baseline for
/// an entity only moves forward, so once a frame names baseline `b`, only frames sent before it
/// can name an older one. The client lets go of the versions older than `b` when it has seen a
/// frame of that entity at least this many ticks newer. A late frame (older than the newest that
/// carried the entity) naming a version the client does not hold is taken as lost: not
/// acknowledged, and no resync. A ring therefore holds one round trip of versions plus one to
/// two times this margin.
pub const STATE_BASE_REORDER_TICKS: u32 = 8;
/// Bytes of `replication.mtu_bytes` left for the transport below a state frame: endpoint header,
/// packet header, record and part framing.
pub const STATE_PACKET_RESERVE: usize = 64;
/// Bytes of a state frame's own header at most.
const STATE_HEADER_BYTES: usize = 16;
/// Smallest supported state frame body.
const MIN_STATE_PAYLOAD: usize = 128;
/// Smallest configured state MTU, including reserved transport and state headers.
pub const MIN_STATE_MTU_BYTES: u32 = (STATE_PACKET_RESERVE + STATE_HEADER_BYTES + MIN_STATE_PAYLOAD) as u32;
// `settings` validates `config/net.ron` without the `ecs` feature, so it keeps its own copy.
const _: () = assert!(MIN_STATE_MTU_BYTES == crate::settings::MIN_STATE_MTU_BYTES);
/// A record unacknowledged this long is sent again even without a loss verdict (the acks stopped),
/// until the connection has a round-trip sample; then [`StateLink::in_flight_timeout_ticks`]
/// follows the measured round trip (spec 120 § 8.1, ADR-8419).
pub const IN_FLIGHT_TIMEOUT_TICKS: u32 = 30;
/// The longest round-trip-based in-flight timeout, in ticks.
pub const MAX_IN_FLIGHT_TIMEOUT_TICKS: u32 = 1024;
/// The most sent frames the server remembers per connection, however many are in flight
/// (ADR-8419): the log keeps every frame younger than its retention, above [`STATE_PACKET_LOG`].
pub const MAX_STATE_PACKET_LOG: usize = 4096;
/// A record larger than one packet travels reliably, so a later frame's acknowledgment does not
/// make it lost: the next one waits for its acknowledgment, or this many in-flight timeouts.
pub const OVERSIZE_TIMEOUT_FACTOR: u32 = 4;
/// Ticks a despawned id is remembered: an older state frame still on the way must not bring it back.
pub const TOMBSTONE_TICKS: u32 = 1200;
/// The most entities a client keeps state for, and the bound on each of its state-channel maps
/// unless [`ClientStateChannel::entity_cap`] sets a lower one (ADR-8760): the entity limit of the
/// snapshot codec, [`crate::snapshot_delta::MAX_ENTITIES`]. A server that spawns a copy past it
/// is refused with a protocol error; tombstones and held ownership bits past it drop their oldest
/// half.
pub const MAX_CLIENT_ENTITIES: usize = crate::snapshot_delta::MAX_ENTITIES;

pub(super) const KIND_SPAWN: u64 = 0;
pub(super) const KIND_REPEAT: u64 = 1;
pub(super) const KIND_UPDATE: u64 = 2;
/// A far-tier update: the far record instead of the transform fields (ADR-10124).
pub(super) const KIND_FAR: u64 = 3;

const ACK_WORDS: usize = STATE_PACKET_LOG / 64;

/// Field writers a connection keeps for its next records.
const MAX_SPARE_WRITERS: usize = 512;
/// Larger writers are not kept: an oversize record's buffer should not stay around.
const MAX_SPARE_WRITER_BITS: usize = 2048;

/// How one connection receives state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnWire {
    /// Waiting for the client's first acknowledgment; only empty tick frames go out.
    #[default]
    Undecided,
    /// The reliable tick stream (protocol 1, or `replication.state_channel: Reliable`).
    Stream,
    /// The unreliable state channel with acknowledged baselines (protocol 2).
    Acked,
}

impl ConnWire {
    /// Lowercase name (agent protocol).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Undecided => "undecided",
            Self::Stream => "reliable_stream",
            Self::Acked => "acked_deltas",
        }
    }
}

/// Counters of one connection's state channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StateCounters {
    /// State frames sent.
    pub packets_sent: u64,
    /// State frames acknowledged.
    pub packets_acked: u64,
    /// State frames that left the log unacknowledged.
    pub packets_lost: u64,
    /// Records sent.
    pub records_sent: u64,
    /// Baselines that fell out of the version ring (a full state went instead).
    pub baselines_expired: u64,
    /// Records larger than one packet (sent reliably, one at a time).
    pub oversize_records: u64,
    /// Round-trip samples taken from acknowledged state frames (ADR-8419).
    pub rtt_samples: u64,
    /// Update candidates the extrapolation gate held back: the client's extrapolation of every
    /// record it may hold was still within the profile's gate (spec 120 § 8.2, ADR-8522).
    pub gate_held: u64,
    /// Far-tier updates sent as far records (kind 3: the coarse position instead of the transform
    /// fields, spec 120 § 8.3, ADR-10124).
    pub far_records: u64,
}

/// One record in a sent packet: what an acknowledgment makes the baseline.
#[derive(Debug, Clone)]
pub(super) struct SentRecord {
    pub(super) slot: u32,
    pub(super) id: NetId,
    pub(super) version: Arc<StateVersion>,
    pub(super) unmatched: Vec<u16>,
    pub(super) teleport: bool,
    pub(super) resend: bool,
    /// A far record (kind 3): acknowledged, the client holds the version's far form unless an
    /// exact record of the same version is acknowledged too (ADR-10124).
    pub(super) far: bool,
}

#[derive(Debug)]
struct SentPacket {
    seq: u32,
    tick: NetTick,
    acked: bool,
    records: Vec<SentRecord>,
}

/// An encoded record waiting for its packet.
#[derive(Debug, Clone)]
pub(super) struct OutRecord {
    pub(super) sent: SentRecord,
    pub(super) kind: u64,
    /// `version_ago`, and `base_ago` for an update (the layout is at the top of this file).
    pub(super) version_ago: u64,
    pub(super) base_ago: Option<u64>,
    pub(super) fields: BitWriter,
}

impl OutRecord {
    /// Bits of the record's body.
    fn body_bits(&self) -> usize {
        2 + ago_bits(self.version_ago) + self.base_ago.map_or(0, ago_bits) + self.fields.bit_len()
    }

    /// Bits of the whole record.
    fn bits(&self) -> usize {
        let body = self.body_bits();
        varint_bits(u64::from(self.sent.id.0)) + varint_bits(body as u64) + body
    }

    fn write_to(&self, w: &mut BitWriter) {
        w.write_varint(u64::from(self.sent.id.0));
        w.write_varint(self.body_bits() as u64);
        w.write_bits(self.kind, 2);
        write_ago(w, self.version_ago);
        if let Some(ago) = self.base_ago {
            write_ago(w, ago);
        }
        w.append(&self.fields);
    }
}

/// Smallest tick distance [`write_ago`] writes as a varint.
const AGO_VARINT_FROM: u64 = 33;

/// Bits of [`write_ago`]'s code for `v`.
fn ago_bits(v: u64) -> usize {
    match v {
        0 => 1,
        1..AGO_VARINT_FROM => 7,
        _ => 2 + varint_bits(v - AGO_VARINT_FROM),
    }
}

/// A tick distance (`version_ago`, `base_ago`; protocol 4, ADR-8467): `0` is the current tick (one
/// bit), `10` and five bits are 1..=32 (a baseline a round trip old), `11` and a varint the rest.
fn write_ago(w: &mut BitWriter, v: u64) {
    match v {
        0 => w.write_bool(false),
        1..AGO_VARINT_FROM => {
            w.write_bool(true);
            w.write_bool(false);
            w.write_bits(v - 1, 5);
        }
        _ => {
            w.write_bool(true);
            w.write_bool(true);
            w.write_varint(v - AGO_VARINT_FROM);
        }
    }
}

/// Reads [`write_ago`]'s code.
pub(crate) fn read_ago(r: &mut BitReader) -> Result<u64, DecodeError> {
    if !r.read_bool()? {
        return Ok(0);
    }
    if !r.read_bool()? {
        return Ok(r.read_bits(5)? + 1);
    }
    r.read_varint()?
        .checked_add(AGO_VARINT_FROM)
        .ok_or(DecodeError::Invalid("tick distance"))
}

/// One frame for the transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatePacket {
    /// Unreliable, or reliable for a record larger than one packet.
    pub channel: Channel,
    /// The frame.
    pub bytes: Vec<u8>,
}

/// The acknowledgment a protocol 2 client adds to its `ACK` frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateAck {
    /// The protocol the client speaks.
    pub protocol: u32,
    /// The newest state sequence received.
    pub newest: Option<u32>,
    /// Bit `i` of word `i / 64`: the sequence `i + 1` before `newest` was received.
    /// The distance is `newest.wrapping_sub(seq)`, the same arithmetic [`StateAcks::note`]
    /// stores, so the window stays valid across the `u32` wrap.
    pub bits: [u64; ACK_WORDS],
}

impl StateAck {
    fn covers(&self, seq: u32) -> bool {
        let Some(newest) = self.newest else {
            return false;
        };
        // A raw `seq > newest` treats every pre-wrap sequence as newer than a wrapped newest,
        // so `note` records it and `covers` then ignores it.
        let behind = newest.wrapping_sub(seq);
        if behind == 0 {
            return true;
        }
        if behind > STATE_PACKET_LOG as u32 {
            return false;
        }
        let i = (behind - 1) as usize;
        self.bits.get(i / 64).is_some_and(|w| w & (1 << (i % 64)) != 0)
    }

    /// Writes the part after the tick.
    pub(crate) fn write(&self, w: &mut BitWriter) {
        w.write_varint(u64::from(self.protocol));
        w.write_varint(self.newest.map_or(0, |n| u64::from(n) + 1));
        let words = self.bits.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
        w.write_bits(words as u64, 8);
        for b in self.bits.iter().take(words) {
            w.write_bits(*b, 64);
        }
    }

    /// Reads the part after the tick; `None` when the frame ends at the tick (protocol 1).
    pub(crate) fn read(r: &mut BitReader) -> Result<Option<Self>, DecodeError> {
        if r.remaining_bits() < 8 {
            return Ok(None);
        }
        let protocol = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("protocol"))?;
        let newest = match r.read_varint()? {
            0 => None,
            n => Some(u32::try_from(n - 1).map_err(|_| DecodeError::Invalid("state sequence"))?),
        };
        let words = r.read_bits(8)? as usize;
        if words > ACK_WORDS {
            return Err(DecodeError::TooLong {
                len: words,
                max: ACK_WORDS,
            });
        }
        let mut bits = [0u64; ACK_WORDS];
        for b in bits.iter_mut().take(words) {
            *b = r.read_bits(64)?;
        }
        Ok(Some(Self {
            protocol,
            newest,
            bits,
        }))
    }
}

/// The server's side of one connection's state channel.
#[derive(Debug, Default)]
pub struct StateLink {
    /// How the connection receives state.
    pub wire: ConnWire,
    /// Entropy coding negotiated for this connection (spec 72 § 18, ADR-3315): decided with the
    /// wire, from the server's configured [`crate::entropy::EntropyMode`] and the protocol the
    /// client's first acknowledgment names.
    pub entropy: crate::entropy::EntropyMode,
    /// Counters of the entropy stage (`net.entropy_stats`).
    pub entropy_stats: crate::entropy::EntropyStats,
    pub(super) transport_frame_bytes: Option<usize>,
    /// The transport's send allowance for this connection, bytes per second (spec 120 § 4.2
    /// item 9): the tick budget is `min(project rate, allowance)`.
    pub(super) send_allowance: Option<u32>,
    next_seq: u32,
    log: VecDeque<SentPacket>,
    newest_acked: Option<u32>,
    /// The sequence of the record larger than a packet that is on its way.
    oversize: Option<u32>,
    /// A state MTU below the floor was reported for this connection (once, not every tick).
    mtu_refusal_logged: bool,
    /// Smoothed round trip of state frames and its mean deviation, in ticks (RFC 6298 § 2 on
    /// tick samples; `None` before the first sample).
    srtt: Option<(f32, f32)>,
    /// Counters.
    pub counters: StateCounters,
}

impl StateLink {
    /// Sent frames the server remembers for this connection now (at most [`MAX_STATE_PACKET_LOG`]).
    pub fn log_frames(&self) -> usize {
        self.log.len()
    }

    /// A record larger than one packet was sent and is not acknowledged yet (the next one waits).
    pub fn oversize_pending(&self) -> bool {
        self.oversize
            .is_some_and(|s| self.log.iter().any(|p| p.seq == s && !p.acked))
    }

    /// The smoothed round trip of this connection's state frames in ticks, once one was acknowledged.
    pub fn rtt_ticks(&self) -> Option<f32> {
        self.srtt.map(|(srtt, _)| srtt)
    }

    /// How long an unacknowledged record counts as on its way (spec 120 § 8.1, ADR-8419): the
    /// round trip plus four mean deviations (at least one tick) and two ticks for the send cadence,
    /// in `3..=`[`MAX_IN_FLIGHT_TIMEOUT_TICKS`]; [`IN_FLIGHT_TIMEOUT_TICKS`] before the first sample.
    pub fn in_flight_timeout_ticks(&self) -> u32 {
        match self.srtt {
            None => IN_FLIGHT_TIMEOUT_TICKS,
            Some((srtt, var)) => {
                let t = ((srtt + (4.0 * var).max(1.0)).ceil() as u32).saturating_add(2);
                t.clamp(3, MAX_IN_FLIGHT_TIMEOUT_TICKS)
            }
        }
    }

    /// How long a frame stays in the log unacknowledged once the log is past [`STATE_PACKET_LOG`]:
    /// two in-flight timeouts, at least [`OVERSIZE_TIMEOUT_FACTOR`] times the first-sample timeout,
    /// so the acknowledgment that gives the first round-trip sample still finds its frame.
    fn retention_ticks(&self) -> u32 {
        (2 * self.in_flight_timeout_ticks()).max(OVERSIZE_TIMEOUT_FACTOR * IN_FLIGHT_TIMEOUT_TICKS)
    }

    /// Folds one round-trip sample in ticks (RFC 6298 § 2: gains 1/8 and 1/4).
    pub(super) fn note_rtt(&mut self, sample: u32) {
        let r = sample.max(1) as f32;
        self.srtt = Some(match self.srtt {
            None => (r, r / 2.0),
            Some((srtt, var)) => {
                let var = 0.75 * var + 0.25 * (srtt - r).abs();
                (0.875 * srtt + 0.125 * r, var)
            }
        });
        self.counters.rtt_samples += 1;
    }

    /// Whether a sent packet may still be on its way: not acknowledged, nothing newer acknowledged,
    /// and not older than the in-flight timeout. The record larger than a packet on its way rides
    /// the reliable stream: only its acknowledgment (or [`OVERSIZE_TIMEOUT_FACTOR`] timeouts)
    /// resolves it, whatever newer frames were acknowledged.
    pub(super) fn unresolved(&self, seq: u32, now: NetTick) -> bool {
        let Some(front) = self.log.front() else {
            return false;
        };
        // The log is one contiguous wrapping run. A raw `seq < front.seq` treats every
        // post-wrap sequence as older than a pre-wrap front, so in-flight suppression is
        // off for a full log rotation after the sequence wraps.
        let index = seq.wrapping_sub(front.seq) as usize;
        let Some(p) = self.log.get(index) else {
            return false;
        };
        if p.seq != seq {
            return false;
        }
        let age = now.0.saturating_sub(p.tick.0);
        if self.oversize == Some(seq) {
            return !p.acked && age <= OVERSIZE_TIMEOUT_FACTOR * self.in_flight_timeout_ticks();
        }
        !p.acked
            && self.newest_acked.is_none_or(|n| (seq.wrapping_sub(n) as i32) > 0)
            && age <= self.in_flight_timeout_ticks()
    }

    /// Whether a record larger than one packet may go now (one at a time, spec 72 § 7): the last
    /// one was acknowledged, or it has been on its way for [`OVERSIZE_TIMEOUT_FACTOR`] in-flight
    /// timeouts (spec 120 § 8.1, ADR-8419).
    pub(super) fn oversize_free(&self, now: NetTick) -> bool {
        self.oversize.is_none_or(|s| !self.unresolved(s, now))
    }

    /// The largest record that fits one unreliable frame.
    pub(super) fn payload_bytes(&self, mtu: u32) -> usize {
        let transport = self.transport_frame_bytes.unwrap_or_else(|| {
            // Standalone/loopback replication uses the default sealed endpoint budget.
            crate::connection::Connection::new(crate::connection::LinkConfig::default())
                .max_unreliable_bytes(crate::crypto::SEALED_RESERVED_HEADER_BYTES)
                .saturating_sub(4) // game lane and NetFrame's flag + u16 length
        });
        (mtu as usize)
            .saturating_sub(STATE_PACKET_RESERVE)
            .min(transport)
            .saturating_sub(STATE_HEADER_BYTES)
    }

    fn take_seq(&mut self) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        seq
    }

    fn remember(&mut self, seq: u32, tick: NetTick, records: Vec<SentRecord>) {
        self.counters.packets_sent += 1;
        self.counters.records_sent += records.len() as u64;
        self.log.push_back(SentPacket {
            seq,
            tick,
            acked: false,
            records,
        });
        // The log keeps what may still be acknowledged (ADR-8419): past STATE_PACKET_LOG frames
        // only what is acknowledged or older than the retention goes, so a fast connection on a
        // long round trip does not lose its frames before their acknowledgments arrive.
        let retention = self.retention_ticks();
        while self.log.len() > STATE_PACKET_LOG
            && self.log.front().is_some_and(|p| {
                p.acked
                    || tick.0.saturating_sub(p.tick.0) > retention
                    || self.log.len() > MAX_STATE_PACKET_LOG
            })
        {
            if self.log.pop_front().is_some_and(|p| !p.acked) {
                self.counters.packets_lost += 1;
            }
        }
    }

    /// Packs records in order into frames of at most `mtu` bytes (always at least one frame: the
    /// tick is the client's clock and carries the acknowledgment on). A record larger than a frame
    /// goes alone on the reliable channel. Returns the frames and `(slot, seq)` per record; the
    /// records' field writers go back to `spare`.
    pub(super) fn assemble(
        &mut self,
        tick: NetTick,
        records: Vec<OutRecord>,
        mtu: u32,
        spare: &mut Vec<BitWriter>,
    ) -> (Vec<StatePacket>, Vec<(u32, u32)>) {
        if mtu < MIN_STATE_MTU_BYTES {
            // A setting applied in code can still be below the floor (`from_ron` refuses it).
            // No frame can be built; say so once per connection, not every tick (review row 7).
            if !self.mtu_refusal_logged {
                self.mtu_refusal_logged = true;
                tracing::error!(
                    target: "net",
                    mtu,
                    floor = MIN_STATE_MTU_BYTES,
                    "replication_mtu_too_small: no state frames are sent to this connection"
                );
            }
            return (Vec::new(), Vec::new());
        }
        let payload = self.payload_bytes(mtu);
        let mut frames: Vec<StatePacket> = Vec::new();
        let mut placed: Vec<(u32, u32)> = Vec::with_capacity(records.len());
        let mut body = BitWriter::with_capacity(payload);
        let mut sent: Vec<SentRecord> = Vec::with_capacity(records.len());
        let entropy = self.entropy == crate::entropy::EntropyMode::Range;
        let flush = |link: &mut Self,
                     body: &mut BitWriter,
                     sent: &mut Vec<SentRecord>,
                     placed: &mut Vec<(u32, u32)>,
                     channel: Channel,
                     frames: &mut Vec<StatePacket>| {
            let _span = tracing::trace_span!("net_entropy_encode").entered();
            let seq = link.take_seq();
            let mut w = BitWriter::with_capacity(STATE_HEADER_BYTES + body.bit_len().div_ceil(8));
            // The tag itself rides the negotiated wire: TAG_STATE for every protocol 2 frame
            // (byte-identical to the protocol 2 layout), TAG_STATE_ENTROPY for a connection the
            // server negotiated entropy with (spec 72 § 18, ADR-3315).
            let tag = if entropy {
                crate::entropy::TAG_STATE_ENTROPY
            } else {
                crate::runtime::TAG_STATE
            };
            w.write_bits(u64::from(tag), 8);
            w.write_varint(u64::from(tick.0));
            w.write_varint(u64::from(seq));
            if entropy {
                link.entropy_stats.frames += 1;
                // The stage's input is the record count plus the records, byte-packed.
                let mut plain = BitWriter::with_capacity(body.bit_len().div_ceil(8) + 1);
                plain.write_varint(sent.len() as u64);
                plain.append(body);
                let plain_bytes = plain.finish();
                let (payload_bytes, coded) = crate::entropy::code_or_raw(&plain_bytes);
                link.entropy_stats.bytes_in += plain_bytes.len() as u64;
                if coded {
                    link.entropy_stats.coded += 1;
                    // What the stage costs on the wire: the flag byte and the coded-length varint.
                    let stage_overhead = 1 + varint_bits(payload_bytes.len() as u64) as u64 / 8;
                    link.entropy_stats.bytes_out += payload_bytes.len() as u64 + stage_overhead;
                    w.write_bits(1, 8); // flags: range-coded body follows
                    w.write_varint(payload_bytes.len() as u64);
                    w.write_bytes(&payload_bytes);
                } else {
                    // Coding did not shrink the body: the raw fallback keeps the frame
                    // self-describing and pays one flag byte.
                    link.entropy_stats.raw_fallback += 1;
                    link.entropy_stats.bytes_out += payload_bytes.len() as u64 + 1;
                    w.write_bits(0, 8);
                    w.write_varint(sent.len() as u64);
                    w.append(body);
                }
            } else {
                w.write_varint(sent.len() as u64);
                w.append(body);
            }
            body.clear();
            placed.extend(sent.iter().map(|r| (r.slot, seq)));
            link.remember(seq, tick, std::mem::take(sent));
            frames.push(StatePacket {
                channel,
                bytes: w.finish(),
            });
            seq
        };
        for mut rec in records {
            let bytes = rec.bits().div_ceil(8);
            if bytes > payload {
                // Alone, on the reliable channel: the transport fragments it.
                let mut one = BitWriter::with_capacity(bytes);
                rec.write_to(&mut one);
                let mut list = vec![rec.sent];
                let seq = flush(
                    self,
                    &mut one,
                    &mut list,
                    &mut placed,
                    Channel::ReliableOrdered,
                    &mut frames,
                );
                self.oversize = Some(seq);
                self.counters.oversize_records += 1;
                continue;
            }
            if body.bit_len().div_ceil(8) + bytes > payload {
                flush(
                    self,
                    &mut body,
                    &mut sent,
                    &mut placed,
                    Channel::Unreliable,
                    &mut frames,
                );
            }
            rec.write_to(&mut body);
            if spare.len() < MAX_SPARE_WRITERS && rec.fields.bit_len() <= MAX_SPARE_WRITER_BITS {
                let mut fields = std::mem::take(&mut rec.fields);
                fields.clear();
                spare.push(fields);
            }
            sent.push(rec.sent);
        }
        if !sent.is_empty() || frames.is_empty() {
            flush(
                self,
                &mut body,
                &mut sent,
                &mut placed,
                Channel::Unreliable,
                &mut frames,
            );
        }
        (frames, placed)
    }
}

impl super::ConnReplication {
    /// Applies an acknowledgment: every (entity, version) of a newly acknowledged packet becomes
    /// the connection's baseline if newer than the current one (spec 72 § 4.2). Packets that stay
    /// unacknowledged need nothing: the next delta starts from the older baseline.
    ///
    /// `now` is the server's tick at the acknowledgment: the newest frame it acknowledges for the
    /// first time gives a round-trip sample (state frames are never sent twice under one sequence,
    /// so the sample is unambiguous).
    pub(super) fn apply_state_ack(&mut self, ack: &StateAck, now: Option<NetTick>) {
        let mut packets = std::mem::take(&mut self.link.log);
        let mut sampled = false;
        for p in packets.iter_mut().rev() {
            if p.acked || !ack.covers(p.seq) {
                continue;
            }
            p.acked = true;
            // The record larger than a packet rides the reliable stream: its acknowledgment
            // waits for the transport's resends, so it is no round-trip sample.
            if !sampled
                && self.link.oversize != Some(p.seq)
                && let Some(now) = now
            {
                sampled = true;
                self.link.note_rtt(now.0.saturating_sub(p.tick.0));
            }
            self.link.counters.packets_acked += 1;
            for rec in std::mem::take(&mut p.records) {
                let Some(ce) = self.copy_mut(rec.slot) else {
                    continue;
                };
                // A record for an earlier copy of this slot or id says nothing about this one.
                if ce.id != rec.id || p.tick < ce.born {
                    continue;
                }
                if ce.base.as_ref().is_none_or(|b| newer(&rec.version, b)) {
                    ce.base_number = rec.version.number;
                    ce.base_tick = rec.version.tick;
                    ce.spawn_acked = true;
                    ce.base = Some(rec.version);
                    ce.unmatched = rec.unmatched;
                    ce.base_far = rec.far;
                } else if !rec.far
                    && ce.base_far
                    && ce.base.as_ref().is_some_and(|b| b.number == rec.version.number)
                {
                    // An exact record of the very version the client held in its far form (or
                    // under an older tick, when the baseline moved on without a record): the
                    // client stored it exact at the version's tick, in whichever order the two
                    // arrived (ADR-10124).
                    ce.base_far = false;
                    ce.base_tick = rec.version.tick;
                }
                if rec.teleport && p.tick >= ce.teleport_tick {
                    ce.teleport_due = false;
                }
                if rec.resend && p.tick >= ce.resend_tick {
                    ce.resend_due = false;
                }
            }
        }
        self.link.log = packets;
        if let Some(n) = ack.newest
            && self
                .link
                .newest_acked
                .is_none_or(|a| (n.wrapping_sub(a) as i32) > 0)
        {
            self.link.newest_acked = Some(n);
        }
    }
}

impl super::ConnReplication {
    /// Acknowledges everything sent so far (`ServerReplication::ack_all_state`).
    pub(super) fn ack_everything(&mut self) {
        let Some(newest) = self.link.log.back().map(|p| p.seq) else {
            return;
        };
        self.apply_state_ack(
            &StateAck {
                protocol: crate::settings::NET_PROTOCOL_VERSION,
                newest: Some(newest),
                bits: [u64::MAX; ACK_WORDS],
            },
            None,
        );
    }
}

fn newer(a: &StateVersion, b: &StateVersion) -> bool {
    (a.number.wrapping_sub(b.number) as i32) > 0
}

// ---- client --------------------------------------------------------------------------------

/// Which state frames a client received: the acknowledgment it sends.
#[derive(Debug, Clone, Default)]
pub struct StateAcks {
    newest: Option<u32>,
    bits: [u64; ACK_WORDS],
}

impl StateAcks {
    /// Records a received sequence. Returns false when it was already recorded.
    pub fn note(&mut self, seq: u32) -> bool {
        let Some(newest) = self.newest else {
            self.newest = Some(seq);
            return true;
        };
        // Serial-number arithmetic: the sequence wraps at 2^32, and a forward wrap is a new
        // newest, not an old frame.
        let diff = seq.wrapping_sub(newest) as i32;
        if diff == 0 {
            return false;
        }
        if diff < 0 {
            let i = (diff.unsigned_abs() - 1) as usize;
            return match self.bits.get_mut(i / 64) {
                Some(w) => {
                    let bit = 1 << (i % 64);
                    let fresh = *w & bit == 0;
                    *w |= bit;
                    fresh
                }
                None => false,
            };
        }
        let shift = diff as usize;
        // The old newest becomes bit `shift - 1`.
        let (words, bits) = (shift / 64, shift % 64);
        let old = self.bits;
        self.bits = [0; ACK_WORDS];
        for (i, w) in old.iter().enumerate() {
            let at = i + words;
            if let Some(slot) = self.bits.get_mut(at) {
                *slot |= *w << bits;
            }
            if bits > 0
                && let Some(slot) = self.bits.get_mut(at + 1)
            {
                *slot |= *w >> (64 - bits);
            }
        }
        let i = shift - 1;
        if let Some(w) = self.bits.get_mut(i / 64) {
            *w |= 1 << (i % 64);
        }
        self.newest = Some(seq);
        true
    }

    /// Takes back a recorded sequence older than the newest: the frame counts as not received
    /// (the server then treats it as lost). Returns false when `seq` is the newest received or
    /// outside the window, which cannot be taken back.
    pub fn forget(&mut self, seq: u32) -> bool {
        let Some(newest) = self.newest else {
            return false;
        };
        let diff = seq.wrapping_sub(newest) as i32;
        if diff >= 0 {
            return false;
        }
        let i = (diff.unsigned_abs() - 1) as usize;
        match self.bits.get_mut(i / 64) {
            Some(w) => {
                *w &= !(1 << (i % 64));
                true
            }
            None => false,
        }
    }

    /// The acknowledgment as it goes on the wire.
    pub fn snapshot(&self) -> StateAck {
        StateAck {
            protocol: crate::settings::NET_PROTOCOL_VERSION,
            newest: self.newest,
            bits: self.bits,
        }
    }
}

/// Counters of a client's state channel.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClientStateCounters {
    /// State frames applied.
    pub packets: u64,
    /// State frames seen twice.
    pub duplicates: u64,
    /// Records applied to the world.
    pub applied: u64,
    /// Records older than the newest applied: stored as possible baselines, not applied.
    pub stored_only: u64,
    /// Deltas whose baseline this client no longer holds (each asks for a resync).
    pub baseline_missing: u64,
    /// Records that failed to decode (each asks for a resync).
    pub record_errors: u64,
    /// Records for a despawned id, dropped.
    pub tombstoned: u64,
    /// Spawns refused because the client already held state for its entity cap of entities; each
    /// ends the session with a protocol error (ADR-8760).
    pub over_cap: u64,
    /// Tombstones and held ownership bits dropped early, oldest first, because their map reached
    /// the entity cap (ADR-8760).
    pub cap_evicted: u64,
}

/// One state of one entity on a client.
#[derive(Debug, Clone)]
struct ClientVersion {
    tick: u32,
    parent: Option<NetId>,
    transform: Option<(NetTransform, TransformReplication)>,
    /// The record's integers: the baseline a transform delta is decoded against (ADR-8467).
    tq: Option<QTransform>,
    /// The transform's record carried the steady bit (ADR-8522): the copy extrapolates it for as
    /// long as it is the newest.
    steady: bool,
    /// The transform came from a far record (ADR-10124): the coarse position, a rotation and scale
    /// carried over, no velocity. It is no baseline for a transform delta, and an exact record of
    /// the same version replaces it.
    far: bool,
    comps: Arc<Vec<(u16, ErasedComp)>>,
}

/// The last versions of one entity, oldest first (spec 72 § 4.3).
#[derive(Debug, Default)]
struct EntityRing {
    versions: VecDeque<ClientVersion>,
    applied: Option<u32>,
    /// The newest packet that carried this entity. Version ticks say when the state last changed,
    /// so only packet ticks can be compared with a despawn's tick.
    last_packet: u32,
    /// This client asked for the full state again: the next spawn record replaces what is stored,
    /// even a version it already has (a reference that resolved decodes differently now, and a
    /// copy that was lost is created again).
    full_next: bool,
    /// Versions older than this tick were let go: the server named a baseline this new in a frame
    /// that is [`STATE_BASE_REORDER_TICKS`] behind the newest by now (ADR-10051).
    floor: u32,
    /// `(frame tick, baseline tick)` of a frame that named a baseline: the next floor, once a
    /// frame of this entity [`STATE_BASE_REORDER_TICKS`] newer arrives.
    named: Option<(u32, u32)>,
}

impl EntityRing {
    fn find(&self, tick: u32) -> Option<&ClientVersion> {
        self.versions
            .binary_search_by_key(&tick, |v| v.tick)
            .ok()
            .and_then(|i| self.versions.get(i))
    }

    /// A frame of tick `packet` named the version of tick `base` as this entity's baseline. The
    /// server's baseline only moves forward (an acknowledgment replaces it only with a newer
    /// version, spec 72 § 4.2), so every frame sent after this one names `base` or newer: the
    /// versions before it are needed only by frames sent earlier and still on their way. They go
    /// once a frame [`STATE_BASE_REORDER_TICKS`] newer than the naming one has arrived. A late
    /// frame (older than the newest that carried this entity) names nothing new.
    fn note_named(&mut self, packet: u32, base: u32) {
        if packet < self.last_packet {
            return;
        }
        match self.named {
            None => self.named = Some((packet, base)),
            Some((at, floor)) if packet.saturating_sub(at) >= STATE_BASE_REORDER_TICKS => {
                self.floor = self.floor.max(floor);
                while self.versions.front().is_some_and(|v| v.tick < self.floor) {
                    self.versions.pop_front();
                }
                self.named = Some((packet, base));
            }
            Some(_) => {}
        }
    }

    /// Whether a frame of tick `packet` is late for this entity: a newer frame already carried
    /// it. A delta of a late frame against a version this ring does not hold (let go below the
    /// floor, or cleared by a resync) says nothing is wrong with the copy, which has newer state:
    /// the frame is taken as lost instead of asking for the full state.
    fn late(&self, packet: u32) -> bool {
        packet < self.last_packet
    }

    /// Stores a version. Two records of one server version can differ: each carries what the
    /// connection's audience allowed when it was sent, so what they carry adds up. Returns the
    /// stored version and whether it holds something new.
    fn merge(&mut self, v: ClientVersion) -> (ClientVersion, bool) {
        let at = match self.versions.binary_search_by_key(&v.tick, |o| o.tick) {
            Ok(at) => at,
            Err(at) => {
                self.versions.insert(at, v.clone());
                while self.versions.len() > STATE_VERSION_RING as usize {
                    self.versions.pop_front();
                }
                return (v, true);
            }
        };
        let Some(stored) = self.versions.get_mut(at) else {
            return (v, true);
        };
        let mut comps = stored.comps.as_ref().clone();
        let mut added = false;
        if stored.far && !v.far && v.tq.is_some() {
            // The exact record of a version held in its far form (ADR-10124): it wins in either
            // arrival order, so what the server may delta against is what is stored.
            stored.transform = v.transform;
            stored.tq = v.tq;
            stored.steady = v.steady;
            stored.far = false;
            added = true;
        }
        for (ci, value) in v.comps.iter() {
            if let Err(i) = comps.binary_search_by_key(ci, |(c, _)| *c) {
                comps.insert(i, (*ci, value.clone()));
                added = true;
            }
        }
        if added {
            stored.comps = Arc::new(comps);
        }
        (stored.clone(), added)
    }
}

/// The client's side of the state channel.
#[derive(Debug, Default)]
pub struct ClientStateChannel {
    /// The most entities this client keeps state for, and the bound on its tombstones and held
    /// ownership bits; `None` is [`MAX_CLIENT_ENTITIES`], and a larger value reads as it (ADR-8760).
    pub entity_cap: Option<usize>,
    /// A spawn went past the cap during the frame being applied: the frame is refused.
    cap_refused: bool,
    /// The frame being applied is late for an entity and named a baseline this client does not
    /// hold (a frame reordered past [`STATE_BASE_REORDER_TICKS`]): it is not acknowledged
    /// (ADR-10051).
    frame_retired: bool,
    /// Deltas dropped that way since the session began.
    retired_baselines: u64,
    rings: HashMap<NetId, EntityRing>,
    /// What to acknowledge.
    pub acks: StateAcks,
    /// `id → tick` of recent despawns.
    tombstones: HashMap<NetId, u32>,
    /// A travel's old-level floor (spec 77 § 6.2 step 3, ADR-7295): `TravelBegin` names the
    /// first `NetId` the server had not assigned when the travel began, so on the reliable tick
    /// stream every record of an id below it is stale — ids are never reused — unless the id is
    /// in `travel_free`.
    travel_floor: Option<u32>,
    /// The floor's exceptions: the travel's persistent `NetId`s, plus the ids the server's
    /// despawn records reached (the notice clears the tombstone, as on the state channel).
    travel_free: HashSet<NetId>,
    /// Ownership bits for copies that are not here yet: `id → (owned, predicted, tick)`.
    pending_own: HashMap<NetId, (bool, bool, u32)>,
    /// The newest tick of a state frame (the interpolation clock ticks once per tick).
    pub newest_tick: Option<NetTick>,
    /// `NetNode::local_ticks` when `newest_tick` arrived: steady copies without interpolation
    /// extrapolate to `newest_tick` plus the local ticks since (ADR-8522).
    newest_local: u64,
    /// Counters.
    pub counters: ClientStateCounters,
}

impl ClientStateChannel {
    /// The server tick this client estimates now (ADR-8522): the newest state frame's tick plus
    /// the local ticks since it arrived (`local_ticks` is `NetNode::local_ticks`). It is the tick
    /// a steady copy without interpolation is shown at; `None` before the first state frame.
    pub fn server_tick_estimate(&self, local_ticks: u64) -> Option<f64> {
        let newest = self.newest_tick?;
        Some(f64::from(newest.0) + local_ticks.saturating_sub(self.newest_local) as f64)
    }

    /// Whether this client is on the state channel (a state frame arrived).
    pub fn active(&self) -> bool {
        self.newest_tick.is_some()
    }

    /// Returns the channel to "connected to nothing" for a new session: every map, counter and
    /// tick goes, but the host's [`Self::entity_cap`] stays — it is set once at startup and
    /// survives connects and replay resets exactly like `ClientReplication::level_bind_cap`
    /// (ADR-8850).
    pub(crate) fn reset_for_new_session(&mut self) {
        let entity_cap = self.entity_cap;
        *self = Self {
            entity_cap,
            ..Self::default()
        };
    }

    /// Entities with stored versions, and the versions stored.
    pub fn stored(&self) -> (usize, usize) {
        (
            self.rings.len(),
            self.rings.values().map(|r| r.versions.len()).sum(),
        )
    }

    /// Bytes the stored versions take: each version's record and its component list. The
    /// component values themselves are not counted: a version shares every value that did not
    /// change with the version it was decoded against.
    pub fn stored_bytes(&self) -> usize {
        self.rings
            .values()
            .flat_map(|r| r.versions.iter())
            .map(|v| {
                std::mem::size_of::<ClientVersion>()
                    + 2 * std::mem::size_of::<usize>()
                    + v.comps.capacity() * std::mem::size_of::<(u16, ErasedComp)>()
            })
            .sum()
    }

    /// The most versions one entity has stored.
    pub fn longest_ring(&self) -> usize {
        self.rings.values().map(|r| r.versions.len()).max().unwrap_or(0)
    }

    /// Deltas of a late frame (older than the newest that carried the entity) whose baseline this
    /// client does not hold, in practice a frame reordered past [`STATE_BASE_REORDER_TICKS`]: the
    /// frame is taken as lost (not acknowledged) and asks for no resync (ADR-10051).
    pub fn retired_baselines(&self) -> u64 {
        self.retired_baselines
    }

    /// The entries of the three state-channel maps: `(entities with stored versions, tombstones,
    /// held ownership bits)`. Each stays at or under [`Self::cap`] (ADR-8760).
    pub fn map_lens(&self) -> (usize, usize, usize) {
        (self.rings.len(), self.tombstones.len(), self.pending_own.len())
    }

    /// The entity cap in force: [`Self::entity_cap`], at most [`MAX_CLIENT_ENTITIES`], at least 1.
    pub fn cap(&self) -> usize {
        self.entity_cap
            .unwrap_or(MAX_CLIENT_ENTITIES)
            .clamp(1, MAX_CLIENT_ENTITIES)
    }

    /// Whether a spawn past the cap was refused since the last call (the frame applying it is
    /// refused, and the session ends with a protocol error).
    pub(crate) fn take_cap_refusal(&mut self) -> bool {
        std::mem::take(&mut self.cap_refused)
    }

    /// Notes a refused spawn past the cap.
    pub(crate) fn refuse_over_cap(&mut self, id: NetId) {
        self.counters.over_cap += 1;
        self.cap_refused = true;
        tracing::warn!(target: "net", net_id = id.0, cap = self.cap(), "net: spawn past the client entity cap");
    }

    /// The newest state-frame tick that carried a record of `id`, `None` while nothing of it is
    /// stored (the per-entity reconcile tick of spec 73 § 6.3, ADR-2413).
    pub fn carried_tick(&self, id: NetId) -> Option<NetTick> {
        self.rings.get(&id).map(|r| NetTick(r.last_packet))
    }

    /// The tick of the newest version stored for `id` (the state-frame fuzz builds records against
    /// it).
    pub(crate) fn newest_version_tick(&self, id: NetId) -> Option<u32> {
        self.rings.get(&id)?.versions.back().map(|v| v.tick)
    }

    /// This client is asking the server for an entity's full state.
    pub(crate) fn expect_full(&mut self, id: NetId) {
        if let Some(ring) = self.rings.get_mut(&id) {
            ring.full_next = true;
        }
    }

    /// An ownership change for a copy this client does not have yet.
    pub(super) fn hold_ownership(&mut self, id: NetId, owned: bool, predicted: bool, tick: NetTick) {
        if !self.pending_own.contains_key(&id) && self.pending_own.len() >= self.cap() {
            let evicted = evict_oldest_half(&mut self.pending_own, |(_, _, t)| *t);
            self.counters.cap_evicted += evicted as u64;
        }
        self.pending_own.insert(id, (owned, predicted, tick.0));
    }

    /// A despawn or out-of-scope notice of `tick`. False when a newer copy already took the id's
    /// place: the notice is about the one before it.
    pub(super) fn forget(&mut self, id: NetId, tick: NetTick) -> bool {
        if self.rings.get(&id).is_some_and(|r| r.last_packet > tick.0) {
            return false;
        }
        self.rings.remove(&id);
        self.pending_own.remove(&id);
        if !self.tombstones.contains_key(&id) && self.tombstones.len() >= self.cap() {
            // Expired ones go first; only a map still full of live tombstones loses its oldest
            // half (a frame older than those despawns is the least likely still on the way).
            self.expire_tombstones(tick);
            if self.tombstones.len() >= self.cap() {
                let evicted = evict_oldest_half(&mut self.tombstones, |t| *t);
                self.counters.cap_evicted += evicted as u64;
            }
        }
        self.tombstones.insert(id, tick.0);
        true
    }

    fn expire_tombstones(&mut self, tick: NetTick) {
        self.tombstones
            .retain(|_, t| tick.0.saturating_sub(*t) < TOMBSTONE_TICKS);
    }

    /// A travel began (spec 77 § 6.2 step 3, ADR-7295): every id below `below` belongs to the
    /// old level on the reliable tick stream. `keep` (the `TravelKeep` ids) names what survives.
    /// Replaces the previous floor: a later travel's floor covers everything the earlier one did,
    /// and its own despawn notices re-clear the ids.
    pub(crate) fn begin_travel_floor(&mut self, below: NetId, keep: &[NetId]) {
        self.travel_floor = Some(below.0);
        self.travel_free.clear();
        self.travel_free.extend(keep.iter().copied());
    }

    /// The server's despawn notice for `id` arrived (ADR-7295): it clears the travel's
    /// tombstone for the id, the way the notice replaces the point tombstone on the state
    /// channel. Out-of-scope notices do not clear: the entity lives on the server.
    pub(crate) fn clear_travel_tombstone(&mut self, id: NetId) {
        if self.travel_floor.is_some_and(|floor| id.0 < floor) {
            self.travel_free.insert(id);
        }
    }

    /// Whether a record of `id` at `tick` belongs to an entity this client watched die: a point
    /// tombstone at or past the record's tick (a despawn or out-of-scope notice), or the travel
    /// floor with the id still covered (ADR-7295). A point tombstone that does not cover the
    /// record falls through to the floor: out-of-scope ids re-enter scope, and a later record of
    /// them must not sneak past the travel's floor just because an old notice exists.
    pub(crate) fn is_tombstoned(&self, id: NetId, tick: u32) -> bool {
        if self.tombstones.get(&id).is_some_and(|t| tick <= *t) {
            return true;
        }
        self.travel_floor
            .is_some_and(|floor| id.0 < floor && !self.travel_free.contains(&id))
    }

    fn prune(&mut self, tick: NetTick) {
        if tick.0.is_multiple_of(64) {
            self.expire_tombstones(tick);
        }
    }
}

/// Removes the older half of `map` by the tick `tick_of` reads (ties at the median go too), so a
/// full map frees room for half its size in one pass: O(n) per n/2 inserts. Returns how many went.
fn evict_oldest_half<V>(map: &mut HashMap<NetId, V>, tick_of: impl Fn(&V) -> u32) -> usize {
    let mut ticks: Vec<u32> = map.values().map(&tick_of).collect();
    if ticks.is_empty() {
        return 0;
    }
    let mid = (ticks.len() - 1) / 2;
    let (_, median, _) = ticks.select_nth_unstable(mid);
    let median = *median;
    let before = map.len();
    map.retain(|_, v| tick_of(v) > median);
    before - map.len()
}

/// Reads a frame's header: `(tick, seq)`.
pub(crate) fn read_state_header(r: &mut BitReader) -> Result<(NetTick, u32), DecodeError> {
    let tick = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?;
    let seq = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("state sequence"))?;
    Ok((NetTick(tick), seq))
}

/// Applies one state frame. A bad record is skipped and its entity resynced (spec 72 § 9); only a
/// frame whose framing is broken is an error.
pub(crate) fn client_apply_state_frame(world: &mut World, bytes: &[u8]) -> Result<(), DecodeError> {
    let _span = tracing::trace_span!("net_apply_state").entered();
    // Remove the entropy stage first (spec 72 § 18, ADR-3315): the models reset per frame, so
    // loss, duplication or reordering of other frames cannot corrupt this one, and the record
    // loop below is untouched by the wire format change.
    let frame = crate::entropy::unwrap_frame(bytes)?;
    // The runtime routes only these two tags here; anything else is not a state frame (ADR-8761:
    // the state-frame fuzz found a frame of another tag applied as one).
    if frame.tag != crate::runtime::TAG_STATE && frame.tag != crate::entropy::TAG_STATE_ENTROPY {
        return Err(DecodeError::Invalid("state frame tag"));
    }
    let mut r = BitReader::new(&frame.bytes);
    let _tag = r.read_bits(8)?;
    let (tick, seq) = read_state_header(&mut r)?;
    if frame.tag == crate::entropy::TAG_STATE_ENTROPY {
        let _flags = r.read_bits(8)?; // 0: the body is raw from here on
    }
    let mut client = world.remove_resource::<ClientReplication>().unwrap_or_default();
    let fresh = client.state.acks.note(seq);
    if !fresh {
        client.state.counters.duplicates += 1;
        world.insert_resource(client);
        return Ok(());
    }
    client.state.counters.packets += 1;
    client.state.frame_retired = false;
    client.state.prune(tick);
    let new_tick = client.state.newest_tick.is_none_or(|t| tick > t);
    if new_tick {
        client.state.newest_tick = Some(tick);
        client.state.newest_local = world
            .get_resource::<crate::runtime::NetNode>()
            .map_or(0, |n| n.local_ticks);
    }
    client.applied = Some(client.applied.map_or(tick, |a| a.max(tick)));
    world.insert_resource(client);
    if new_tick {
        crate::interp::on_state_frame(world, tick);
    } else {
        crate::interp::on_late_state_frame(world, tick);
    }
    let registry = world.resource::<ReplicationRegistry>().clone();
    let mut level_index: LevelIndex = None;
    let n = r.read_len(1 << 16)?;
    let mut spawned_now: Vec<NetId> = Vec::new();
    for _ in 0..n {
        let id = read_net_id(&mut r)?;
        let len = r.read_len(r.remaining_bits())?;
        let start = r.bit_pos();
        let result = apply_record(
            world,
            &mut r,
            id,
            tick,
            &registry,
            &mut level_index,
            &mut spawned_now,
        );
        let used = r.bit_pos() - start;
        if world.resource_mut::<ClientReplication>().state.take_cap_refusal() {
            // ADR-8760: the frame is refused whole and the session ends (the caller's protocol
            // error); the copies spawned before the refused record stay until it closes.
            super::after_spawns(world, &spawned_now);
            return Err(DecodeError::Invalid("client entity cap"));
        }
        if result.is_err() || used > len {
            let mut client = world.resource_mut::<ClientReplication>();
            client.state.counters.record_errors += 1;
            client.resync.insert(id);
            tracing::debug!(target: "net", net_id = id.0, "net: bad state record");
        }
        // The next record starts where the length says, whatever this one did.
        if used > len {
            return Err(DecodeError::Invalid("state record length"));
        }
        skip_bits(&mut r, len - used)?;
    }
    {
        // ADR-10051: a late frame that named a version this client does not hold counts as
        // lost. Acknowledged, it could make a version this client could not decode the server's
        // baseline.
        let mut client = world.resource_mut::<ClientReplication>();
        if std::mem::take(&mut client.state.frame_retired) {
            client.state.acks.forget(seq);
        }
    }
    super::after_spawns(world, &spawned_now);
    Ok(())
}

/// A version's component list and what each decoded value referenced.
type DecodedEntries = (Vec<(u16, ErasedComp)>, Vec<(u16, ApplyOutcome)>);

/// The component entries of a record on top of `base`.
fn decode_entries(
    world: &mut World,
    r: &mut BitReader,
    registry: &ReplicationRegistry,
    base: &[(u16, ErasedComp)],
) -> Result<DecodedEntries, DecodeError> {
    let map = world.resource::<NetEntities>().clone();
    // Handle fields resolve through the asset-server context (ADR-2415).
    let asset_cx = world
        .get_resource::<crate::assets::NetAssets>()
        .and_then(|a| a.context());
    let tick = world
        .get_resource::<crate::runtime::NetNode>()
        .map_or(NetTick(0), |n| n.tick);
    let n = super::read_entry_count(r, 1 << 16)?;
    let mut out: Vec<(u16, ErasedComp)> = base.to_vec();
    let mut refs: Vec<(u16, ApplyOutcome)> = Vec::new();
    for _ in 0..n {
        let ci = u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("component"))?;
        let kind = r.read_bits(2)?;
        let len = r.read_len(r.remaining_bits())?;
        let start = r.bit_pos();
        let Some(c) = registry.get(ci) else {
            skip_bits(r, len)?;
            continue;
        };
        let at = out.binary_search_by_key(&ci, |(i, _)| *i);
        match kind {
            ENTRY_REMOVED => {
                if let Ok(i) = at {
                    out.remove(i);
                }
            }
            ENTRY_FULL | ENTRY_DELTA => {
                let cx = CodecContext::new(&map).with_assets(asset_cx.as_deref());
                let value = if kind == ENTRY_FULL {
                    (c.decode_full)(r, &cx)?
                } else {
                    let current = at
                        .ok()
                        .and_then(|i| out.get(i))
                        .ok_or(DecodeError::Invalid("delta for a component the baseline lacks"))?;
                    (c.decode_delta)(&current.1, r, &cx)?
                };
                super::record_asset_misses(world, &cx, c.name, tick);
                let stats = cx.take_stats();
                refs.push((
                    ci,
                    ApplyOutcome {
                        unresolved: stats.unresolved,
                        referenced: stats.referenced,
                    },
                ));
                match at {
                    Ok(i) => {
                        if let Some(slot) = out.get_mut(i) {
                            slot.1 = value;
                        }
                    }
                    Err(i) => out.insert(i, (ci, value)),
                }
                if r.bit_pos() - start != len {
                    return Err(DecodeError::Invalid("component entry length"));
                }
            }
            _ => return Err(DecodeError::Invalid("entry kind")),
        }
    }
    Ok((out, refs))
}

fn read_spawn_kind(r: &mut BitReader) -> Result<SpawnKind, DecodeError> {
    Ok(match r.read_bits(2)? {
        0 => SpawnKind::Bare,
        1 => SpawnKind::Prefab(read_u128(r)?),
        2 => SpawnKind::Level {
            level: read_u128(r)?,
            guid: read_u128(r)?,
        },
        _ => return Err(DecodeError::Invalid("spawn kind")),
    })
}

fn apply_record(
    world: &mut World,
    r: &mut BitReader,
    id: NetId,
    tick: NetTick,
    registry: &ReplicationRegistry,
    level_index: &mut LevelIndex,
    spawned_now: &mut Vec<NetId>,
) -> Result<(), DecodeError> {
    let kind = r.read_bits(2)?;
    let version_ago = u32::try_from(read_ago(r)?).map_err(|_| DecodeError::Invalid("version"))?;
    let version = tick
        .0
        .checked_sub(version_ago)
        .ok_or(DecodeError::Invalid("version tick"))?;
    {
        let mut client = world.resource_mut::<ClientReplication>();
        // A packet from before the despawn, or of an id a travel's floor covers (ADR-7295): on
        // either wire a record of the old level still in flight must not bring a copy back. A
        // later spawn of the entity has a later packet tick, whatever its version's tick says.
        if client.state.is_tombstoned(id, tick.0) {
            client.state.counters.tombstoned += 1;
            return Ok(());
        }
    }
    let result = match kind {
        KIND_SPAWN | KIND_REPEAT => apply_spawn(world, r, id, version, registry, level_index, spawned_now),
        KIND_UPDATE => apply_update(world, r, id, version, tick, registry, false),
        KIND_FAR => apply_update(world, r, id, version, tick, registry, true),
        _ => Err(DecodeError::Invalid("record kind")),
    };
    if let Some(ring) = world.resource_mut::<ClientReplication>().state.rings.get_mut(&id) {
        ring.last_packet = ring.last_packet.max(tick.0);
    }
    result
}

fn client_copy(world: &World, id: NetId) -> Option<Entity> {
    world.resource::<NetEntities>().entity(id)
}

fn apply_spawn(
    world: &mut World,
    r: &mut BitReader,
    id: NetId,
    version: u32,
    registry: &ReplicationRegistry,
    level_index: &mut LevelIndex,
    spawned_now: &mut Vec<NetId>,
) -> Result<(), DecodeError> {
    let kind = read_spawn_kind(r)?;
    let parent = if r.read_bool()? {
        Some(read_net_id(r)?)
    } else {
        None
    };
    let mut owned = r.read_bool()?;
    let mut predicted = r.read_bool()?;
    let mode = transform_mode_from(r.read_bits(2)?);
    let mut steady = false;
    let tq = if mode != TransformReplication::None && r.read_bool()? {
        let teleport = r.read_bool()?;
        steady = r.read_bool()?;
        Some((QTransform::read_abs(mode, r)?, teleport))
    } else {
        None
    };
    let transform = tq.map(|(q, teleport)| (q.to_net(), teleport));
    let (comps, refs) = decode_entries(world, r, registry, &[])?;
    let mut client = world.resource_mut::<ClientReplication>();
    if let Some((o, p, at)) = client.state.pending_own.get(&id).copied()
        && at >= version
    {
        owned = o;
        predicted = p;
    }
    if !client.state.rings.contains_key(&id) && client.state.rings.len() >= client.state.cap() {
        // ADR-8760: a server that spawns past the client's entity cap is broken or hostile.
        client.state.refuse_over_cap(id);
        return Err(DecodeError::TooLong {
            len: client.state.rings.len() + 1,
            max: client.state.cap(),
        });
    }
    client.state.pending_own.remove(&id);
    let ring = client.state.rings.entry(id).or_default();
    if std::mem::take(&mut ring.full_next) {
        ring.versions.clear();
        ring.applied = None;
    }
    let applied_at = ring.applied;
    let previous = applied_at.and_then(|a| ring.find(a)).cloned();
    let (next, added) = ring.merge(ClientVersion {
        tick: version,
        parent,
        transform: transform.map(|(t, _)| (t, mode)),
        tq: tq.map(|(q, _)| q),
        steady,
        far: false,
        comps: Arc::new(comps),
    });
    if applied_at.is_some_and(|a| version < a) {
        client.state.counters.stored_only += 1;
        return Ok(());
    }
    let existing = client_copy(world, id);
    let alive = existing.filter(|e| world.get_entity(*e).is_ok());
    let mut client = world.resource_mut::<ClientReplication>();
    // The same version again, sent before the acknowledgment came back.
    if applied_at == Some(version) && !added && alive.is_some() {
        client.state.counters.stored_only += 1;
        return Ok(());
    }
    if let Some(ring) = client.state.rings.get_mut(&id) {
        ring.applied = Some(version);
    }
    client.state.counters.applied += 1;
    let tick = NetTick(version);
    let previous = previous.filter(|_| alive.is_some());
    // A spawn sent again because its acknowledgment had not come back: to a copy that has it, this
    // is a newer state, not a new copy (no spawn notice, `NetVec` events are the difference).
    let repeat = previous.is_some();
    let e = match alive {
        Some(e) => e,
        None => match create_copy(world, kind, level_index, id) {
            Some(e) => e,
            None => {
                // A level bind kept until its cell spawns (spec 71 § 4.5): the ring holds the
                // state, and the spawn asks for it in full.
                world.resource_mut::<NetEntities>().remove(id);
                return Ok(());
            }
        },
    };
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.insert(id);
        ent.remove::<(NetUnbound, NetOutOfScope)>();
    }
    world.resource_mut::<NetEntities>().insert(id, e);
    set_owned(world, e, owned, predicted);
    set_parent(world, e, parent);
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.insert(NetTransformMode(mode));
    }
    crate::interp::adopt_named_profile(world, e);
    let interp = crate::interp::is_interpolated_proxy(world, e);
    if !interp {
        crate::interp::flush_steps(world, e);
    } else if alive.is_some() && !repeat {
        // A spawn record is the full current state: older queued step values must not land on
        // top of it later.
        crate::interp::drop_steps(world, e);
    }
    // The version ring keeps the wire frame (it is the delta baseline); the world gets this
    // client's local frame (spec 78 § 14), as on the tick stream.
    let transform = transform.map(|(t, teleport)| (super::to_local_frame(world, e, t), teleport));
    if let Some((t, teleport)) = transform {
        if interp && alive.is_some() {
            // A resync or a copy back in scope: the shown pose moves through the buffer.
            note_transform(world, e, mode, teleport);
        } else {
            write_transform(world, e, mode, &t, teleport);
        }
        if interp {
            crate::interp::buffer_transform(world, e, proxy_state(tick, &t, teleport, steady));
        } else {
            super::set_steady(world, e, tick, &t, steady);
        }
    }
    if interp && alive.is_none() {
        crate::interp::hide_until_shown(world, e, tick);
    }
    install(
        world,
        e,
        tick,
        registry,
        previous.as_ref(),
        &next,
        &refs,
        !repeat,
        interp,
    );
    if alive.is_some() && previous.is_none() {
        // A spawn record for a copy with no stored state: components it no longer carries go.
        for (ci, c) in registry.components().iter().enumerate() {
            if next
                .comps
                .binary_search_by_key(&(ci as u16), |(i, _)| *i)
                .is_err()
                && (c.has)(world, e)
            {
                (c.remove)(world, e);
            }
        }
    }
    super::attach_waiting_children(world, id, e);
    if alive.is_none() {
        world.resource_mut::<NetEvents>().spawned.push(NetSpawned {
            entity: e,
            net_id: id,
        });
    }
    spawned_now.push(id);
    Ok(())
}

fn apply_update(
    world: &mut World,
    r: &mut BitReader,
    id: NetId,
    version: u32,
    packet: NetTick,
    registry: &ReplicationRegistry,
    far: bool,
) -> Result<(), DecodeError> {
    let base_ago = u32::try_from(read_ago(r)?).map_err(|_| DecodeError::Invalid("baseline"))?;
    let base_tick = version
        .checked_sub(base_ago)
        .ok_or(DecodeError::Invalid("baseline tick"))?;
    let (base, late, shown) = {
        let client = world.resource::<ClientReplication>();
        let ring = client.state.rings.get(&id);
        (
            ring.and_then(|ring| ring.find(base_tick)).cloned(),
            ring.is_some_and(|ring| ring.late(packet.0)),
            // What the copy shows now, when that is newer than the baseline (a far record keeps
            // its rotation, ADR-10124).
            ring.and_then(|ring| ring.applied.filter(|a| *a > base_tick).and_then(|a| ring.find(a)))
                .and_then(|v| v.tq),
        )
    };
    let Some(base) = base else {
        let mut client = world.resource_mut::<ClientReplication>();
        if late {
            // ADR-10051: a late frame (reordered past the margin) against a version let go
            // since. The copy has newer state, so no resync: the frame counts as lost.
            client.state.retired_baselines += 1;
            client.state.frame_retired = true;
            return Ok(());
        }
        // Spec 72 § 4.3: a delta against a version this client no longer has.
        client.state.counters.baseline_missing += 1;
        client.resync.insert(id);
        return Ok(());
    };
    // A far update (kind 3, ADR-10124) has no parent field and the far record for a transform.
    let parent_field = if !far && r.read_bool()? {
        Some(if r.read_bool()? {
            Some(read_net_id(r)?)
        } else {
            None
        })
    } else {
        None
    };
    // The transform: a delta against the baseline's record, or an absolute record (spec 120
    // § 8.2, ADR-8467). The baseline is looked up first, so a delta needs no guess.
    let mut steady = base.steady;
    let tq = if far {
        let bq = base.tq.ok_or(DecodeError::Invalid(
            "far record against a baseline without a transform",
        ))?;
        // The baseline's cell, in either of its forms; no steady bit: a far copy is not gated.
        steady = false;
        Some((QTransform::read_far(&bq, r)?.carrying(shown.as_ref()), false))
    } else if r.read_bool()? {
        let teleport = r.read_bool()?;
        steady = r.read_bool()?;
        let q = if r.read_bool()? {
            // A far version's rotation and velocity are not what the server holds for it: the
            // server never sends a delta against one (ADR-10124), so this record is refused and
            // the copy asks for its full state.
            if base.far {
                return Err(DecodeError::Invalid("transform delta against a far baseline"));
            }
            let bq = base.tq.ok_or(DecodeError::Invalid(
                "transform delta against a baseline without a transform",
            ))?;
            // The predictor's time is the record's distance from its baseline (ADR-8522).
            QTransform::read_delta(&bq, base_ago, r)?
        } else {
            QTransform::read_abs(transform_mode_from(r.read_bits(2)?), r)?
        };
        Some((q, teleport))
    } else {
        None
    };
    let t = tq.map(|(q, teleport)| (q.to_net(), teleport, q.mode));
    let (comps, refs) = decode_entries(world, r, registry, &base.comps)?;
    let next = ClientVersion {
        tick: version,
        parent: parent_field.unwrap_or(base.parent),
        transform: t.as_ref().map(|(v, _, m)| (*v, *m)).or(base.transform),
        tq: tq.map(|(q, _)| q).or(base.tq),
        steady,
        // A record without a transform keeps the baseline's, in the form the baseline has it.
        far: if tq.is_some() { far } else { base.far },
        comps: Arc::new(comps),
    };
    let teleport = t.as_ref().is_some_and(|(_, tp, _)| *tp);
    let mut client = world.resource_mut::<ClientReplication>();
    let Some(ring) = client.state.rings.get_mut(&id) else {
        return Ok(());
    };
    let applied_at = ring.applied;
    let previous = applied_at.and_then(|a| ring.find(a)).cloned();
    ring.note_named(packet.0, base_tick);
    let (next, added) = ring.merge(next);
    // The same version again adds nothing unless it carries what the audience hid before, or a
    // teleport the first record did not have.
    if applied_at.is_some_and(|a| version < a) || (applied_at == Some(version) && !added && !teleport) {
        client.state.counters.stored_only += 1;
        return Ok(());
    }
    ring.applied = Some(version);
    client.state.counters.applied += 1;
    let tick = NetTick(version);
    let e = world
        .resource::<NetEntities>()
        .entity(id)
        .filter(|e| world.get_entity(*e).is_ok());
    let Some(e) = e else {
        // A copy this client lost: ask for full state. A level bind waiting for its cell asks when
        // the cell spawns.
        let mut client = world.resource_mut::<ClientReplication>();
        if !client.is_level_bind_buffered(id) {
            client.resync.insert(id);
        }
        return Ok(());
    };
    if previous.as_ref().is_none_or(|p| p.parent != next.parent) {
        set_parent(world, e, next.parent);
    }
    let interp = crate::interp::is_interpolated_proxy(world, e);
    if !interp {
        crate::interp::flush_steps(world, e);
    }
    // A delta from an older baseline may leave the transform out: the version still says where
    // the entity is, and it may differ from what the last applied version said.
    let moved = t.is_some()
        || match (
            &next.transform,
            previous.as_ref().and_then(|p| p.transform.as_ref()),
        ) {
            (Some(a), Some(b)) => a.0 != b.0,
            (Some(_), None) => true,
            _ => false,
        };
    if moved && let Some((v, mode)) = next.transform {
        let v = super::to_local_frame(world, e, v);
        if interp {
            note_transform(world, e, mode, teleport);
            crate::interp::buffer_transform(world, e, proxy_state(tick, &v, teleport, next.steady));
        } else {
            write_transform(world, e, mode, &v, teleport);
            super::set_steady(world, e, tick, &v, next.steady);
        }
    }
    install(
        world,
        e,
        tick,
        registry,
        previous.as_ref(),
        &next,
        &refs,
        false,
        interp,
    );
    Ok(())
}

/// Puts a version's components on a copy: what differs from the version applied before it.
#[allow(clippy::too_many_arguments)] // ADR-1950: the copy, its two versions and the apply mode.
fn install(
    world: &mut World,
    e: Entity,
    tick: NetTick,
    registry: &ReplicationRegistry,
    previous: Option<&ClientVersion>,
    next: &ClientVersion,
    refs: &[(u16, ApplyOutcome)],
    spawn: bool,
    interp: bool,
) {
    let map = world.resource::<NetEntities>().clone();
    let max_refs = world
        .get_resource::<crate::settings::NetSettings>()
        .map_or(4096, |s| s.replication.max_pending_refs as usize);
    if interp {
        world.resource_mut::<ClientReplication>().step_capture = Some((e, u64::from(tick.0), spawn));
    }
    let before: &[(u16, ErasedComp)] = previous.map_or(&[], |p| p.comps.as_slice());
    for (ci, value) in next.comps.iter() {
        let Some(c) = registry.get(*ci) else {
            continue;
        };
        let same = before
            .binary_search_by_key(ci, |(i, _)| *i)
            .ok()
            .and_then(|at| before.get(at))
            .is_some_and(|(_, v)| Arc::ptr_eq(v, value));
        if same {
            continue;
        }
        let cx = CodecContext::new(&map);
        (c.install)(world, e, value, &cx, spawn);
    }
    for (ci, _) in before {
        if next.comps.binary_search_by_key(ci, |(i, _)| *i).is_err()
            && let Some(c) = registry.get(*ci)
        {
            (c.remove)(world, e);
        }
    }
    if interp {
        world.resource_mut::<ClientReplication>().step_capture = None;
    }
    for (ci, outcome) in refs {
        record_refs(world, e, *ci, outcome.clone(), tick, max_refs);
    }
}

#[cfg(test)]
mod tests {
    /// The sequence wraps at 2^32: frame 0 after `u32::MAX` is the new newest, not a duplicate,
    /// and the frame before the wrap is an old one inside the window.
    #[test]
    fn state_acks_follow_the_sequence_across_the_wrap() {
        let mut a = StateAcks::default();
        assert!(a.note(u32::MAX - 1));
        assert!(a.note(u32::MAX));
        assert!(a.note(0), "a forward wrap is a new newest");
        assert!(a.note(1));
        assert_eq!(a.snapshot().newest, Some(1));
        assert!(!a.note(0), "now a duplicate");
        assert!(!a.note(u32::MAX), "recorded before the wrap");
        assert!(!a.note(u32::MAX - 1));
        // A frame skipped across the wrap fills its hole once.
        assert!(a.note(3));
        assert!(a.note(2));
        assert!(!a.note(2));
    }

    use super::*;

    /// ADR-8760: ownership bits for copies that never come stay bounded: a full map drops its
    /// older half, counted.
    #[test]
    fn cap_evicts_the_oldest_half_of_held_ownership() {
        let mut c = ClientStateChannel {
            entity_cap: Some(16),
            ..ClientStateChannel::default()
        };
        for i in 0..100u32 {
            c.hold_ownership(NetId(i), true, false, NetTick(i));
            assert!(c.map_lens().2 <= 16, "{i}: {:?}", c.map_lens());
        }
        assert!(c.counters.cap_evicted > 0);
        assert_eq!(c.counters.over_cap, 0, "bookkeeping, not a refusal");
        assert!(c.pending_own.contains_key(&NetId(99)), "the newest stays");
        assert!(!c.pending_own.contains_key(&NetId(0)), "the oldest went");
        // An id already held is updated in place, whatever the count.
        let before = c.counters.cap_evicted;
        c.hold_ownership(NetId(99), false, true, NetTick(100));
        assert_eq!(c.counters.cap_evicted, before);
    }

    /// ADR-8760: a despawn flood keeps the tombstones at the cap: expired ones go first, then the
    /// older half; a map under the cap keeps every tombstone until it expires.
    #[test]
    fn cap_evicts_the_oldest_half_of_tombstones() {
        let mut c = ClientStateChannel {
            entity_cap: Some(16),
            ..ClientStateChannel::default()
        };
        for i in 0..10u32 {
            assert!(c.forget(NetId(i), NetTick(i)));
        }
        assert_eq!(c.map_lens().1, 10);
        assert_eq!(c.counters.cap_evicted, 0, "under the cap nothing goes early");
        for i in 10..1000u32 {
            assert!(c.forget(NetId(i), NetTick(i)));
            assert!(c.map_lens().1 <= 16);
        }
        assert!(c.counters.cap_evicted > 0);
        assert!(c.tombstones.contains_key(&NetId(999)));
        // Expiry first: tombstones older than TOMBSTONE_TICKS make room without an eviction.
        let mut d = ClientStateChannel {
            entity_cap: Some(4),
            ..ClientStateChannel::default()
        };
        for i in 0..4u32 {
            d.forget(NetId(i), NetTick(i));
        }
        d.forget(NetId(50), NetTick(TOMBSTONE_TICKS + 10));
        assert_eq!(d.counters.cap_evicted, 0, "expired, not evicted");
        assert_eq!(d.map_lens().1, 1);
    }

    /// ADR-8760, wording corrected by ADR-8850: the default cap is `MAX_ENTITIES` of the
    /// standalone snapshot-delta codec — a codec constant the live replication wire never had as
    /// a limit before the state channel adopted it as its own bound. A larger `entity_cap` reads
    /// as that constant; a smaller one is honored.
    #[test]
    fn the_default_cap_is_the_snapshot_codecs_entity_limit() {
        let mut c = ClientStateChannel::default();
        assert_eq!(c.cap(), MAX_CLIENT_ENTITIES);
        c.entity_cap = Some(usize::MAX);
        assert_eq!(c.cap(), MAX_CLIENT_ENTITIES, "a larger value reads as the limit");
        c.entity_cap = Some(0);
        assert_eq!(c.cap(), 1);
    }

    #[test]
    fn acks_track_a_window() {
        let mut a = StateAcks::default();
        assert!(a.note(10));
        assert!(!a.note(10));
        assert!(a.note(12));
        assert!(a.note(11), "late");
        assert!(!a.note(11));
        let s = a.snapshot();
        assert_eq!(s.newest, Some(12));
        assert!(s.covers(12) && s.covers(11) && s.covers(10));
        assert!(!s.covers(9) && !s.covers(13));
        // A jump over word boundaries keeps the old bits.
        assert!(a.note(200));
        let s = a.snapshot();
        assert!(s.covers(200) && s.covers(12) && s.covers(10));
        assert!(!s.covers(100));
        // Beyond the window the old bits are gone.
        assert!(a.note(200 + STATE_PACKET_LOG as u32 + 8));
        let s = a.snapshot();
        assert!(!s.covers(200));
        assert!(!a.note(10), "too old to record");
    }

    /// A point tombstone that does not cover a record's tick must not hide the record: the
    /// travel floor still applies to it. Covers both orders of the floor and the point
    /// tombstone (review rev-kimi-27 finding 2): id X gets an out-of-scope notice at t0 (point
    /// tombstone), re-enters scope with a spawn frame at t1 > t0 in flight when `TravelBegin`
    /// lands, and the travel's floor covers X: the stale frame must be dropped by the floor,
    /// not waved through by the uncovered point tombstone.
    #[test]
    fn a_point_tombstone_that_does_not_cover_the_record_falls_through_to_the_travel_floor() {
        let id = NetId(7);
        // Point tombstone first, travel floor second.
        let mut channel = ClientStateChannel::default();
        assert!(channel.forget(id, NetTick(50)));
        channel.begin_travel_floor(NetId(100), &[]);
        assert!(
            channel.is_tombstoned(id, 40),
            "a record the point tombstone covers stays hidden"
        );
        assert!(
            channel.is_tombstoned(id, 60),
            "a record past the point tombstone falls through to the travel floor"
        );
        // Travel floor first, point tombstone second: the same fall-through.
        let mut channel = ClientStateChannel::default();
        channel.begin_travel_floor(NetId(100), &[]);
        assert!(channel.forget(id, NetTick(50)));
        assert!(channel.is_tombstoned(id, 40));
        assert!(
            channel.is_tombstoned(id, 60),
            "the floor applies whatever order the tombstone and the floor arrived in"
        );
        // The travel's kept ids survive the floor either way.
        let mut channel = ClientStateChannel::default();
        channel.begin_travel_floor(NetId(100), &[id]);
        assert!(!channel.is_tombstoned(id, 60), "a kept id is not tombstoned");
    }

    /// `StateAcks::note` already records a sequence across the `u32` wrap. `covers` and
    /// `apply_state_ack` have to use that same distance, or a pre-wrap packet stays unacked once
    /// `newest` has wrapped and the server keeps the older baseline.
    #[test]
    fn audit_net2_state_ack_covers_across_the_wrap() {
        let mut acks = StateAcks::default();
        assert!(acks.note(u32::MAX - 1));
        assert!(acks.note(u32::MAX));
        assert!(acks.note(0));
        assert!(acks.note(1));
        let snap = acks.snapshot();
        assert_eq!(snap.newest, Some(1));
        assert!(snap.covers(1) && snap.covers(0));
        assert!(
            snap.covers(u32::MAX),
            "u32::MAX is 2 behind newest 1, inside the 256-deep ack window"
        );
        assert!(snap.covers(u32::MAX - 1));
        assert!(!snap.covers(u32::MAX - 2), "a hole across the wrap stays open");

        let version = Arc::new(super::super::StateVersion {
            tick: NetTick(3),
            number: 4,
            parent: None,
            transform: None,
            comps: Arc::new(Vec::new()),
        });
        let mut conn = super::super::ConnReplication::default();
        conn.link.next_seq = u32::MAX;
        let seq_max = conn.link.take_seq();
        conn.link.remember(
            seq_max,
            NetTick(3),
            vec![SentRecord {
                slot: 0,
                id: NetId(7),
                version,
                unmatched: Vec::new(),
                teleport: true,
                resend: true,
                far: false,
            }],
        );
        let seq_zero = conn.link.take_seq();
        conn.link.remember(seq_zero, NetTick(4), Vec::new());
        let seq_one = conn.link.take_seq();
        conn.link.remember(seq_one, NetTick(5), Vec::new());
        assert_eq!((seq_max, seq_zero, seq_one), (u32::MAX, 0, 1));
        conn.insert(super::super::ConnEntity {
            id: NetId(7),
            slot: 0,
            base: None,
            base_number: 0,
            base_tick: NetTick(0),
            unmatched: Vec::new(),
            owned: false,
            predicted: false,
            last_sent: NetTick(3),
            out_of_scope: false,
            full_again: false,
            teleport_due: true,
            resend_due: true,
            born: NetTick(1),
            teleport_tick: NetTick(3),
            resend_tick: NetTick(3),
            spawn_acked: false,
            sent_seq: u32::MAX,
            sent_version: 4,
            flight: Vec::new(),
            gated: false,
            base_far: false,
        });
        conn.apply_state_ack(&snap, None);
        let packet = conn
            .link
            .log
            .iter()
            .find(|p| p.seq == u32::MAX)
            .expect("the pre-wrap packet is still in the log");
        assert!(packet.acked, "sequence u32::MAX is acked when newest is 1");
        let copy = conn.copy(0).expect("copy");
        assert!(copy.spawn_acked, "the acked spawn becomes the baseline");
        assert!(!copy.teleport_due && !copy.resend_due);
        assert_eq!(copy.base_number, 4);
        assert_eq!(conn.link.newest_acked, Some(1));
    }

    /// A packet whose sequence wrapped stays in flight while the log front is still pre-wrap.
    /// Treating `seq < front.seq` as "already gone" lets a second reliable oversize record out
    /// while the first (sequence 0) is still in the log.
    #[test]
    fn audit_net2_in_flight_stays_unresolved_across_the_wrap() {
        let mut link = StateLink {
            next_seq: u32::MAX - 10,
            ..StateLink::default()
        };
        let mut sent_at = 0;
        for t in 0..20 {
            let seq = link.take_seq();
            if seq == 0 {
                sent_at = t;
                link.oversize = Some(seq);
            }
            link.remember(seq, NetTick(t), Vec::new());
        }
        assert_eq!(link.log.front().map(|p| p.seq), Some(u32::MAX - 10));
        assert!(
            link.log.iter().any(|p| p.seq == 0 && !p.acked),
            "sequence 0 is still in the 256-deep log"
        );
        let now = NetTick(sent_at);
        assert!(
            link.unresolved(0, now),
            "sequence 0 is younger than the in-flight timeout and nothing newer is acked"
        );
        assert!(
            !link.oversize_free(now),
            "the oversize record on sequence 0 is still on its way"
        );
    }

    #[test]
    fn ack_extension_round_trips_and_protocol_1_reads_nothing() {
        let mut a = StateAcks::default();
        for s in [3, 5, 90] {
            a.note(s);
        }
        let mut w = BitWriter::new();
        a.snapshot().write(&mut w);
        let bytes = w.finish();
        let got = StateAck::read(&mut BitReader::new(&bytes)).expect("reads");
        assert_eq!(got, Some(a.snapshot()));
        assert_eq!(StateAck::read(&mut BitReader::new(&[])).expect("reads"), None);
        // Words beyond the window are refused.
        let mut w = BitWriter::new();
        w.write_varint(2);
        w.write_varint(1);
        w.write_bits(200, 8);
        assert!(StateAck::read(&mut BitReader::new(&w.finish())).is_err());
    }

    /// Review row 7 (ack-poll lane): a state MTU below the floor, set in code, builds no frame and
    /// is reported once per connection instead of every tick.
    #[test]
    fn an_mtu_below_the_floor_is_refused_and_reported_once() {
        let mut link = StateLink::default();
        for tick in 0..3 {
            let (frames, placed) = link.assemble(
                NetTick(tick),
                Vec::new(),
                MIN_STATE_MTU_BYTES - 1,
                &mut Vec::new(),
            );
            assert!(frames.is_empty() && placed.is_empty());
            assert!(link.mtu_refusal_logged);
        }
        let (frames, _) = link.assemble(NetTick(3), Vec::new(), MIN_STATE_MTU_BYTES, &mut Vec::new());
        assert_eq!(frames.len(), 1, "the floor itself still carries the tick");
    }

    #[test]
    fn frames_respect_the_mtu_and_oversize_goes_reliable() {
        let mut link = StateLink::default();
        let version = Arc::new(StateVersion {
            tick: NetTick(1),
            number: 0,
            parent: None,
            transform: None,
            comps: Arc::new(Vec::new()),
        });
        let rec = |slot: u32, bytes: usize| {
            let mut bits = BitWriter::new();
            bits.write_bytes(&vec![7; bytes]);
            OutRecord {
                sent: SentRecord {
                    slot,
                    id: NetId(slot + 1),
                    version: version.clone(),
                    unmatched: Vec::new(),
                    teleport: false,
                    resend: false,
                    far: false,
                },
                kind: KIND_UPDATE,
                version_ago: 0,
                base_ago: Some(1),
                fields: bits,
            }
        };
        let mut spare = Vec::new();
        let (frames, placed) = link.assemble(
            NetTick(5),
            (0..30).map(|i| rec(i, 100)).collect(),
            1200,
            &mut spare,
        );
        assert!(frames.len() >= 3, "{}", frames.len());
        assert!(frames.iter().all(|f| f.channel == Channel::Unreliable));
        assert!(
            frames
                .iter()
                .all(|f| f.bytes.len() <= 1200 - STATE_PACKET_RESERVE)
        );
        assert_eq!(placed.len(), 30);
        assert_eq!(spare.len(), 30, "field writers come back for the next records");
        assert!(spare.iter().all(BitWriter::is_empty));
        let (frames, placed) = link.assemble(NetTick(6), vec![rec(40, 5000), rec(41, 10)], 1200, &mut spare);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].channel, Channel::ReliableOrdered);
        assert_eq!(frames[1].channel, Channel::Unreliable);
        assert_eq!(placed.len(), 2);
        assert_eq!(spare.len(), 31, "the oversize record's buffer is not kept");
        assert!(!link.oversize_free(NetTick(6)), "one at a time");
        // A record larger than a packet rides the reliable stream: past the in-flight timeout it
        // is still being resent, not lost (ADR-8419; the base freed the slot here).
        assert!(!link.oversize_free(NetTick(6 + IN_FLIGHT_TIMEOUT_TICKS + 1)));
        assert!(link.oversize_free(NetTick(6 + OVERSIZE_TIMEOUT_FACTOR * IN_FLIGHT_TIMEOUT_TICKS + 1)));
        // An empty send tick still sends a frame.
        let (frames, _) = link.assemble(NetTick(7), Vec::new(), 1200, &mut spare);
        assert_eq!(frames.len(), 1);
    }

    /// The layout at the top of this file, and a size that matches what is written.
    #[test]
    fn a_record_writes_what_it_measures() {
        let version = Arc::new(StateVersion {
            tick: NetTick(1),
            number: 0,
            parent: None,
            transform: None,
            comps: Arc::new(Vec::new()),
        });
        for (id, version_ago, base_ago, field_bits) in [
            (1u32, 0u64, None, 0u32),
            (300, 200, Some(70_000u64), 5),
            (128, 32, Some(33), 9),
            (u32::MAX - 1, 1, Some(0), 64),
        ] {
            let mut fields = BitWriter::new();
            fields.write_bits(0x1234_5678_9abc_def0, field_bits);
            let rec = OutRecord {
                sent: SentRecord {
                    slot: 0,
                    id: NetId(id),
                    version: version.clone(),
                    unmatched: Vec::new(),
                    teleport: false,
                    resend: false,
                    far: false,
                },
                kind: KIND_REPEAT,
                version_ago,
                base_ago,
                fields,
            };
            let mut w = BitWriter::new();
            rec.write_to(&mut w);
            assert_eq!(w.bit_len(), rec.bits());
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_varint().expect("id"), u64::from(id));
            let body = r.read_varint().expect("bits") as usize;
            let start = r.bit_pos();
            assert_eq!(r.read_bits(2).expect("kind"), KIND_REPEAT);
            assert_eq!(read_ago(&mut r).expect("version_ago"), version_ago);
            if let Some(ago) = base_ago {
                assert_eq!(read_ago(&mut r).expect("base_ago"), ago);
            }
            assert_eq!(body - (r.bit_pos() - start), field_bits as usize);
        }
    }

    /// Spec 120 § 8.1 (ADR-8419): the in-flight timeout follows the measured round trip, from the
    /// fixed first-sample value; a long round trip moves it past the old 30 ticks.
    #[test]
    fn the_in_flight_timeout_follows_the_round_trip() {
        let mut link = StateLink::default();
        assert_eq!(link.in_flight_timeout_ticks(), IN_FLIGHT_TIMEOUT_TICKS);
        assert_eq!(link.rtt_ticks(), None);
        for _ in 0..40 {
            link.note_rtt(39);
        }
        let rtt = link.rtt_ticks().expect("sampled");
        assert!((rtt - 39.0).abs() < 0.01, "{rtt}");
        let t = link.in_flight_timeout_ticks();
        assert!((42..=46).contains(&t), "39-tick round trip: timeout {t}");
        // A short, steady round trip gives a short timeout.
        let mut fast = StateLink::default();
        for _ in 0..40 {
            fast.note_rtt(2);
        }
        assert!(
            fast.in_flight_timeout_ticks() <= 6,
            "{}",
            fast.in_flight_timeout_ticks()
        );
        // Jitter widens it.
        let mut jittery = StateLink::default();
        for i in 0..40 {
            jittery.note_rtt(if i % 2 == 0 { 10 } else { 30 });
        }
        assert!(
            jittery.in_flight_timeout_ticks() > 30,
            "{}",
            jittery.in_flight_timeout_ticks()
        );
        // Clamped.
        let mut huge = StateLink::default();
        huge.note_rtt(u32::MAX);
        assert_eq!(huge.in_flight_timeout_ticks(), MAX_IN_FLIGHT_TIMEOUT_TICKS);
        assert_eq!(huge.counters.rtt_samples, 1);
    }

    /// ADR-8419: past `STATE_PACKET_LOG` frames the log keeps what may still be acknowledged (younger
    /// than its retention), up to `MAX_STATE_PACKET_LOG`; the base kept 256 whatever was in flight.
    #[test]
    fn the_log_keeps_the_frames_in_flight() {
        let mut link = StateLink::default();
        for _ in 0..8 {
            link.note_rtt(77);
        }
        // Five frames a tick for 100 ticks: 500 frames, all younger than two timeouts.
        for t in 0..100u32 {
            for _ in 0..5 {
                let seq = link.take_seq();
                link.remember(seq, NetTick(t), Vec::new());
            }
        }
        assert_eq!(link.log.len(), 500);
        assert_eq!(link.counters.packets_lost, 0);
        assert!(
            link.unresolved(0, NetTick(70)),
            "the first frame is still on its way at tick 70"
        );
        // Acknowledged frames at the front go first.
        for p in link.log.iter_mut().take(300) {
            p.acked = true;
        }
        let seq = link.take_seq();
        link.remember(seq, NetTick(100), Vec::new());
        assert_eq!(
            link.log.len(),
            STATE_PACKET_LOG,
            "acknowledged frames leave down to the floor"
        );
        assert_eq!(link.counters.packets_lost, 0, "acknowledged frames are not lost");
        // The hard bound holds however young the frames are.
        let mut flood = StateLink::default();
        for _ in 0..(MAX_STATE_PACKET_LOG + 50) {
            let seq = flood.take_seq();
            flood.remember(seq, NetTick(0), Vec::new());
        }
        assert_eq!(flood.log.len(), MAX_STATE_PACKET_LOG);
        assert_eq!(flood.counters.packets_lost, 50);
    }

    /// ADR-8419: the reliable record larger than a packet is resolved by its own acknowledgment,
    /// not by a newer frame's (the base freed the slot as soon as any later frame was
    /// acknowledged, so a second large record went while the first was still being resent).
    #[test]
    fn a_newer_acknowledgment_does_not_free_the_oversize_slot() {
        let mut link = StateLink::default();
        for t in 0..4u32 {
            let seq = link.take_seq();
            if t == 1 {
                link.oversize = Some(seq);
            }
            link.remember(seq, NetTick(t), Vec::new());
        }
        let mut acks = StateAcks::default();
        acks.note(3);
        acks.note(2);
        let mut conn = super::super::ConnReplication {
            link,
            ..Default::default()
        };
        conn.apply_state_ack(&acks.snapshot(), Some(NetTick(10)));
        assert!(
            !conn.link.oversize_free(NetTick(10)),
            "sequence 1 is still on its way"
        );
        assert!(
            !conn.link.unresolved(0, NetTick(10)),
            "sequence 0 is lost (a newer one arrived)"
        );
        assert_eq!(conn.link.counters.rtt_samples, 1, "sequence 3 gave a sample");
        acks.note(1);
        conn.apply_state_ack(&acks.snapshot(), Some(NetTick(12)));
        assert!(conn.link.oversize_free(NetTick(12)));
        assert_eq!(
            conn.link.counters.rtt_samples, 1,
            "the reliable record's acknowledgment is no round-trip sample"
        );
    }

    #[test]
    fn the_log_is_bounded_and_counts_what_left_it() {
        let mut link = StateLink::default();
        for t in 0..(STATE_PACKET_LOG as u32 + 10) {
            link.assemble(NetTick(t), Vec::new(), 1200, &mut Vec::new());
        }
        assert_eq!(link.log.len(), STATE_PACKET_LOG);
        assert_eq!(link.counters.packets_lost, 10);
        assert!(!link.unresolved(0, NetTick(300)), "left the log");
    }

    /// A record with a compressible body (mostly-zero bytes, like quantized state).
    fn compressible_record(slot: u32, bytes: usize) -> OutRecord {
        let version = Arc::new(StateVersion {
            tick: NetTick(1),
            number: 0,
            parent: None,
            transform: None,
            comps: Arc::new(Vec::new()),
        });
        let mut bits = BitWriter::new();
        // Predominantly zero bytes with a slow byte counter: stable high bits, like absolute
        // quantized positions on the wire.
        for i in 0..bytes {
            bits.write_bits(u64::from(i as u8 / 16), 8);
        }
        OutRecord {
            sent: SentRecord {
                slot,
                id: NetId(slot + 1),
                version,
                unmatched: Vec::new(),
                teleport: false,
                resend: false,
                far: false,
            },
            kind: KIND_UPDATE,
            version_ago: 1,
            base_ago: Some(1),
            fields: bits,
        }
    }

    /// The entropy-off frame, built by hand from the layout at the top of this file: this is the
    /// pinned protocol 2 layout the off mode must stay byte-identical to (spec 72 § 18).
    fn hand_built_frame(tick: u32, seq: u32, records: &[OutRecord]) -> Vec<u8> {
        let mut body = BitWriter::new();
        for rec in records {
            rec.write_to(&mut body);
        }
        let mut w = BitWriter::new();
        w.write_bits(u64::from(crate::runtime::TAG_STATE), 8);
        w.write_varint(u64::from(tick));
        w.write_varint(u64::from(seq));
        w.write_varint(records.len() as u64);
        w.append(&body);
        w.finish()
    }

    fn records_for(n: u32, bytes: usize) -> Vec<OutRecord> {
        (0..n).map(|i| compressible_record(i, bytes)).collect()
    }

    #[test]
    fn off_frames_stay_byte_identical_to_the_protocol_2_layout() {
        let mut link = StateLink::default();
        assert_eq!(link.entropy, crate::entropy::EntropyMode::Off);
        let (frames, _) = link.assemble(NetTick(42), records_for(8, 40), 1200, &mut Vec::new());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].channel, Channel::Unreliable);
        assert_eq!(frames[0].bytes, hand_built_frame(42, 0, &records_for(8, 40)));
    }

    #[test]
    fn entropy_frames_unwrap_to_the_identical_plain_frame() {
        let records = records_for(8, 40);
        let mut off = StateLink::default();
        let (plain, _) = off.assemble(NetTick(42), records.clone(), 1200, &mut Vec::new());
        let mut on = StateLink {
            entropy: crate::entropy::EntropyMode::Range,
            ..StateLink::default()
        };
        let (coded, _) = on.assemble(NetTick(42), records, 1200, &mut Vec::new());
        assert_eq!(coded.len(), 1);
        assert_eq!(coded[0].bytes[0], crate::entropy::TAG_STATE_ENTROPY);
        assert!(
            coded[0].bytes.len() < plain[0].bytes.len(),
            "compressible records must shrink: {} vs {}",
            coded[0].bytes.len(),
            plain[0].bytes.len()
        );
        let unwrapped = crate::entropy::unwrap_frame(&coded[0].bytes).expect("unwraps");
        assert_eq!(unwrapped.tag, crate::entropy::TAG_STATE_ENTROPY);
        let mut plain_bytes = unwrapped.bytes.into_owned();
        plain_bytes[0] = crate::runtime::TAG_STATE;
        // The raw form keeps one flags byte after the one-byte tick and seq varints; the client
        // skips it (see client_apply_state_frame), so strip it for the byte comparison.
        plain_bytes.remove(3);
        assert_eq!(plain_bytes, plain[0].bytes, "the decode is transparent");
        assert!(on.entropy_stats.frames > 0);
        assert!(on.entropy_stats.saved() > 0);
    }

    #[test]
    fn entropy_frames_unwrap_independently_of_loss_reorder_and_duplication() {
        let mut link = StateLink {
            entropy: crate::entropy::EntropyMode::Range,
            ..StateLink::default()
        };
        let frames: Vec<Vec<u8>> = (0..6)
            .map(|t| {
                link.assemble(NetTick(t), records_for(6, 32), 1200, &mut Vec::new())
                    .0
                    .remove(0)
                    .bytes
            })
            .collect();
        // Deliver in a scrambled order with a duplicate; each frame must unwrap to exactly the
        // bytes it would unwrap to in order: the per-frame models carry no state across frames.
        let in_order: Vec<Vec<u8>> = frames
            .iter()
            .map(|f| {
                crate::entropy::unwrap_frame(f)
                    .expect("unwraps")
                    .bytes
                    .into_owned()
            })
            .collect();
        for (i, idx) in [4usize, 0, 0, 3, 1, 5, 2].into_iter().enumerate() {
            let late = crate::entropy::unwrap_frame(&frames[idx]).expect("unwraps");
            assert_eq!(late.bytes.as_ref(), in_order[idx].as_slice(), "step {i}");
        }
    }

    #[test]
    fn corrupt_entropy_frames_error_never_panic() {
        let mut link = StateLink {
            entropy: crate::entropy::EntropyMode::Range,
            ..StateLink::default()
        };
        let (frames, _) = link.assemble(NetTick(9), records_for(8, 120), 1200, &mut Vec::new());
        let frame = &frames[0].bytes;
        assert_eq!(frame[0], crate::entropy::TAG_STATE_ENTROPY);
        for cut in 0..frame.len() {
            let _ = crate::entropy::unwrap_frame(&frame[..cut]);
        }
        // An unknown flags value is refused.
        let mut bad_flags = frame.clone();
        bad_flags[2] = 7; // inside the seq varint or just after; either way it must not panic
        let _ = crate::entropy::unwrap_frame(&bad_flags);
        // Truncation of the coded payload is an error, not a panic.
        let mut r = BitReader::new(frame);
        let _tag = r.read_bits(8).expect("tag");
        let _tick = r.read_varint().expect("tick");
        let _seq = r.read_varint().expect("seq");
        assert_eq!(r.read_bits(8).expect("flags"), 1, "the fixture codes");
        let len = r.read_varint().expect("len") as usize;
        let mut hostile = frame[..r.bit_pos() / 8].to_vec();
        hostile.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
        assert!(crate::entropy::unwrap_frame(&hostile).is_err());
        let _ = len;
    }

    #[test]
    fn the_wire_and_entropy_negotiation_follow_the_first_ack() {
        use crate::replication::ServerReplication;
        let ack = |protocol: u32| StateAck {
            protocol,
            newest: Some(0),
            bits: [0; ACK_WORDS],
        };
        // A protocol 2 client on a range-configured server: the wire upgrades, entropy stays off.
        let mut rep = ServerReplication::default();
        rep.add_conn_on(crate::model::PeerId(1), ConnWire::Undecided);
        rep.ack_state(
            crate::model::PeerId(1),
            NetTick(1),
            Some(ack(2)),
            crate::entropy::EntropyMode::Range,
        );
        let link = rep.state_link(crate::model::PeerId(1)).expect("link");
        assert_eq!(link.wire, ConnWire::Acked);
        assert_eq!(link.entropy, crate::entropy::EntropyMode::Off);
        // A protocol 3 client: entropy on.
        let mut rep = ServerReplication::default();
        rep.add_conn_on(crate::model::PeerId(2), ConnWire::Undecided);
        rep.ack_state(
            crate::model::PeerId(2),
            NetTick(1),
            Some(ack(crate::settings::NET_PROTOCOL_VERSION)),
            crate::entropy::EntropyMode::Range,
        );
        assert_eq!(
            rep.state_link(crate::model::PeerId(2)).expect("link").entropy,
            crate::entropy::EntropyMode::Range
        );
        // A protocol 3 client on an off-configured server: off.
        let mut rep = ServerReplication::default();
        rep.add_conn_on(crate::model::PeerId(3), ConnWire::Undecided);
        rep.ack_state(
            crate::model::PeerId(3),
            NetTick(1),
            Some(ack(crate::settings::NET_PROTOCOL_VERSION)),
            crate::entropy::EntropyMode::Off,
        );
        assert_eq!(
            rep.state_link(crate::model::PeerId(3)).expect("link").entropy,
            crate::entropy::EntropyMode::Off
        );
        // No state acknowledgment (protocol 1): the reliable stream, entropy off.
        let mut rep = ServerReplication::default();
        rep.add_conn_on(crate::model::PeerId(4), ConnWire::Undecided);
        rep.ack_state(
            crate::model::PeerId(4),
            NetTick(1),
            None,
            crate::entropy::EntropyMode::Range,
        );
        let link = rep.state_link(crate::model::PeerId(4)).expect("link");
        assert_eq!(link.wire, ConnWire::Stream);
        assert_eq!(link.entropy, crate::entropy::EntropyMode::Off);
    }

    fn version(tick: u32) -> ClientVersion {
        ClientVersion {
            tick,
            parent: None,
            transform: None,
            tq: None,
            steady: false,
            far: false,
            comps: Arc::new(Vec::new()),
        }
    }

    /// ADR-10051: a ring lets go of the versions below a baseline the server named once a frame
    /// of the entity [`STATE_BASE_REORDER_TICKS`] newer than the naming one arrives, and never
    /// of the named baseline or anything newer. A late frame names nothing.
    #[test]
    fn a_ring_lets_go_below_a_named_baseline_after_the_margin() {
        let mut ring = EntityRing::default();
        // One version per tick; the frame of tick `t` names the version of `t - 5` (a 5-tick
        // round trip), as `apply_update` does before it stores the frame's version.
        for t in 1..=100u32 {
            if t > 5 {
                assert!(ring.find(t - 5).is_some(), "tick {t}: the named baseline is held");
                ring.note_named(t, t - 5);
            }
            ring.merge(version(t));
            ring.last_packet = ring.last_packet.max(t);
            // The floor follows the named baseline by one to two margins.
            let oldest = ring.versions.front().map_or(0, |v| v.tick);
            if t > 5 {
                assert!(
                    oldest <= t - 5,
                    "tick {t}: the named baseline is the oldest or newer"
                );
            }
            if t > 5 + 2 * STATE_BASE_REORDER_TICKS {
                assert!(
                    oldest >= t - 5 - 2 * STATE_BASE_REORDER_TICKS,
                    "tick {t}: versions older than two margins below the baseline are gone (oldest {oldest})"
                );
                assert!(ring.versions.len() <= (5 + 2 * STATE_BASE_REORDER_TICKS + 1) as usize);
            }
        }
        let floor = ring.floor;
        assert!(floor >= 100 - 5 - 2 * STATE_BASE_REORDER_TICKS);
        // A frame reordered inside the margin still finds its baseline; one from before the floor
        // names a version let go, and is late.
        let inside = 100 - STATE_BASE_REORDER_TICKS + 1;
        assert!(ring.find(inside - 5).is_some());
        assert!(ring.find(floor - 1).is_none());
        assert!(ring.late(floor + 2));
        // The newest frame is never late: a missing baseline there is a real loss of state.
        assert!(!ring.late(100));
        assert!(!ring.late(101));
        // A late frame moves neither the floor nor the pending one.
        let named = ring.named;
        ring.note_named(50, 45);
        assert_eq!(ring.named, named);
        assert_eq!(ring.floor, floor);
        // Without a named baseline (acknowledgments stopped) the count cap holds.
        let mut ring = EntityRing::default();
        for t in 1..=2 * STATE_VERSION_RING {
            ring.merge(version(t));
        }
        assert_eq!(ring.versions.len(), STATE_VERSION_RING as usize);
    }

    /// ADR-10051: a frame can be taken back from the acknowledgment (it then reads as lost),
    /// except the newest received.
    #[test]
    fn an_acknowledged_frame_can_be_taken_back() {
        let mut acks = StateAcks::default();
        for seq in [5u32, 6, 7, 9] {
            assert!(acks.note(seq));
        }
        assert!(acks.snapshot().covers(6));
        assert!(acks.forget(6));
        let ack = acks.snapshot();
        assert!(!ack.covers(6));
        assert!(ack.covers(5) && ack.covers(7) && ack.covers(9));
        // The frame may arrive again and count again.
        assert!(acks.note(6));
        assert!(acks.snapshot().covers(6));
        // The newest cannot be taken back, nor one outside the window, nor anything before the
        // first frame.
        assert!(!acks.forget(9));
        assert!(!acks.forget(10));
        assert!(!acks.forget(9u32.wrapping_sub(STATE_PACKET_LOG as u32 + 1)));
        assert!(!StateAcks::default().forget(1));
        // Across the sequence wrap.
        let mut acks = StateAcks::default();
        assert!(acks.note(u32::MAX));
        assert!(acks.note(1));
        assert!(acks.forget(u32::MAX));
        assert!(!acks.snapshot().covers(u32::MAX));
    }

    /// The per-version cost the ring's memory numbers are built from (ADR-10051): printed for the
    /// report, and bounded so a field added to a version shows up here.
    #[test]
    fn a_stored_version_is_small() {
        let bytes = std::mem::size_of::<ClientVersion>();
        println!("state-ring: size_of::<ClientVersion>() = {bytes} bytes");
        assert!(bytes <= 192, "a stored version is {bytes} bytes");
    }
}
