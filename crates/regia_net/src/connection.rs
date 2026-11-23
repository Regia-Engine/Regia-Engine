// SPDX-License-Identifier: MPL-2.0
//! One UDP flow carrying a reliable-ordered channel and an unreliable channel.
//!
//! [`Connection`] is sans-IO: it turns queued messages into packet bodies
//! ([`Connection::poll`]) and packet bodies back into messages ([`Connection::receive`]), with
//! time passed in as milliseconds. The server and client endpoints prepend their own header and
//! move the bytes through a socket.
//!
//! Design (public sources, see `docs/provenance.md`). Transport v2 (wire 5, spec 120 § 4.1):
//! every packet gets the next 62-bit packet number, never reused, sent truncated to 1, 2 or 4
//! bytes and decoded against the largest number received (RFC 9000 § 17.1, Appendix A). A
//! retransmission is a new packet carrying the old fragment (RFC 9002 § 2), so an
//! acknowledgment is never ambiguous. Acknowledgments are frames with the largest number
//! received, the ack delay and up to 32 ranges (RFC 9000 § 19.3), optionally with receive
//! timestamps for 16 packets (negotiated in band by `LinkConfig::receive_timestamps`), so one
//! reply names a whole burst. Reliable messages get a 16-bit message id, stay queued until a
//! packet carrying them is acknowledged, and are delivered in id order (G. Fiedler, "Reliable
//! Ordered Messages", 2016). Reliable messages larger than one packet are split into fragments
//! that the ordered stream reassembles. Message ids compare as RFC 1982 serial numbers.
//!
//! Loss, pacing and recovery are one policy (ADR-7995, ADR-8184):
//!
//! - **Acknowledgments** (RFC 9000 § 13.2): a packet with records is ack-eliciting; an
//!   acknowledgment-only packet is not and is never acknowledged for its own sake. A receiver
//!   acknowledges every second ack-eliciting packet, or [`MAX_ACK_DELAY_MS`] after the first
//!   unacknowledged one, and at once when a packet arrives out of order, opens a gap or is a
//!   lone ping (a keepalive or probe). Every packet that has room carries the current
//!   acknowledgment frame; ranges older than one frame names leave first in frames of their own.
//! - **Loss from evidence** (RFC 9002 § 6.1): a packet is lost once a packet sent three or more
//!   later was acknowledged, or once a later packet was acknowledged and it has been out for
//!   9/8 of the larger of the smoothed and latest round trip (at least 1 ms). Silence alone never
//!   declares loss. A packet acknowledged after it was declared lost undoes the loss (DO27 rule
//!   A7: reordering is not loss) and widens both thresholds to the reordering just seen (RFC 9002
//!   § 6.1.1 allows it; the reordering window of RFC 8985).
//! - **Round trips** (RFC 9002 § 5): a sample comes only from the largest newly acknowledged
//!   packet when it is ack-eliciting and was never declared lost, minus the peer's ack delay
//!   (clamped to [`MAX_ACK_DELAY_MS`], and only when the rest stays above the minimum). The
//!   minimum round trip is kept over [`MIN_RTT_WINDOW_MS`].
//! - **Resends.** A fragment whose packet was declared lost is queued again at once, but never
//!   sooner than one round trip after its last transmission, and at most
//!   [`MAX_RESEND_PACKETS_PER_POLL`] resent packets leave per poll.
//! - **Redundant copies.** A reliable fragment of at most [`REDUNDANT_COPY_MAX_BYTES`] rides
//!   once more in the next packet that leaves, with room, at least 1 ms and at most
//!   [`REDUNDANT_COPY_WINDOW_MS`] after its first transmission (the redundancy idea of RFC 2198
//!   applied to small reliable messages; spec 120 § 6.1 does the same for inputs). A lost first
//!   copy then costs one packet interval, not a round trip and a half. No datagram is sent for
//!   it, and it is not a resend. Each repair (a resend after a loss verdict or a probe timeout)
//!   takes one copy of its own in the same way, within [`REDUNDANT_COPY_WINDOW_MS`] or one tick
//!   if that is longer (ADR-8400): a fragment already lost once is on a lossy path, and a lost
//!   repair costs another round trip. After a loss verdict the copy leaves in a packet of its own
//!   at the next poll if no other packet carries it (a game that went quiet still gets it).
//! - **Probe timeout** (RFC 9002 § 6.2): with reliable data in flight and no ack-eliciting packet
//!   sent for `srtt + max(4 rttvar, 1 ms) + max_ack_delay` (within the configured bounds and never
//!   under 9/8 of the round trip), doubled for each consecutive expiry up to
//!   `LinkConfig::resend_backoff_max_ms` and reset by any acknowledgment, one or two probe packets
//!   carry the oldest unacknowledged fragments (the least probed first), never the whole window.
//!   Keepalives are ack-eliciting pings, so a live but idle path keeps measuring itself.
//! - **In-flight window.** Reliable bytes sent and not yet acknowledged or declared lost are
//!   bounded by a window that follows what the path delivers: twice the bytes acknowledged within
//!   one round trip (the delivery-rate idea of BBR, Cardwell et al. 2016), never below the first
//!   burst unless three window-limited round trips in a row delivered less than half of it.
//!   Random loss does not shrink it; a peer that acknowledges almost nothing is held to
//!   [`RECOVERY_INITIAL_PACKETS`] packets per round trip.
//! - **Outage recovery.** Once the path has been measured, two consecutive probe timeouts,
//!   reliable data unacknowledged for as long as two consecutive timeouts would take, or losses
//!   found after that much silence, mean an outage (the persistent-congestion idea of RFC
//!   9002 § 7.6): the window collapses, one poll sends at most [`RECOVERY_INITIAL_PACKETS`]
//!   packets with reliable data, doubling each acknowledged round trip (RFC 5681 § 3.1), and one
//!   second carries at most the larger of [`RECOVERY_PACKETS_PER_SECOND`] and half the packet
//!   rate delivered before the outage. Recovery ends early, with the old window, when the peer
//!   turns out to have received everything (a stalled process, not a dead path).
//!
//! Packet body layout (wire 6; multi-byte fixed fields little-endian, varints per RFC 9000 § 16).
//! `arrival` is new in wire 6 (ADR-10050): the low 16 bits of the receiver's clock, in
//! milliseconds, when the largest acknowledged packet arrived. The sender reads its packets'
//! one-way delay from it (`cc::OneWayDelay`): the rate controller cuts an app-limited sender for
//! a queue only when that delay shows one on the way out:
//!
//! ```text
//! flags u8 | packet_number (1, 2 or 4 bytes) | ack_frame? | record*
//! flags bits 0-1 = packet number length (0: 1 byte, 1: 2 bytes, 2: 4 bytes)
//!       bit 2    = an ack frame follows
//!       bit 3    = the ack frame carries receive timestamps
//!       bit 4    = the sender asks for receive timestamps
//!       bit 5    = the ack frame repeats older ranges (no round-trip sample from it)
//! ack_frame = largest varint | ack_delay varint (us >> 3) | arrival u16 | extra_ranges varint
//!           | first_range varint | (gap varint | length varint)*
//!           | [timestamp_count u8 | (packet_offset varint | zigzag_us varint)*]
//! record = 0x01 len u16 bytes             unreliable message
//!        | 0x02 id u16 len u16 bytes      reliable message, last (or only) fragment
//!        | 0x82 id u16 len u16 bytes      reliable fragment, more follow
//!        | 0x03                           ping (ack-eliciting, no data)
//!        | 0x00 zero*                     padding to the end of the packet (path MTU probes,
//!                                         spec 120 § 4.3; only sent with `mtu_probing` on)
//! ```
//!
//! Reassembly is bounded (spec 120 § 5.2, ADR-7981): a reliable fragment longer than one packet
//! of this connection's configuration fails the connection; fragments stored ahead of a gap are
//! capped per connection ([`LinkConfig::connection_buffer_bytes`]) and, through a
//! [`ReassemblyBudget`], across every connection of a server; and a gap (or a message missing its
//! next fragment) that the peer has not filled for `max(4 x RTO, 2 s)` of its own traffic fails
//! the connection as [`LinkFailure::ReassemblyStalled`]. An honest sender never trips any of them:
//! it stores at most one window of fragments, and it repairs a missing one about every two RTOs
//! through loss detection, so the deadline is stretched (at most 4x) by the peer's measured
//! packet loss, the repairs it could lose in a row (ADR-8521).

use std::collections::{HashMap, VecDeque};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ack::{AckFrame, AckTimestamp, Inserted, RecvRanges};
use crate::wire::{WireError, WireReader, WireWriter};

/// Which channel a message travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Channel {
    /// Delivered at most once, possibly out of order or not at all; never resent.
    Unreliable,
    /// One reliable stream of the connection's stream table (spec 120 § 5.1): delivered exactly
    /// once, ordered within the stream (or not at all, for an unordered stream), and never held
    /// back by a loss on another stream.
    Stream(StreamId),
}

impl Channel {
    /// The game RPC stream, `Stream(StreamId::GAME_RPC)`: game messages, delivered exactly once,
    /// in send order, resent until acknowledged. It replaces the v0.6 alias `Channel::Reliable`,
    /// removed in v0.7 (spec 120 § 12.3, ADR-10067); a stream-2 message is reported as this
    /// value whichever form sent it, and it can be matched as a pattern.
    pub const GAME_RPC: Self = Self::Stream(StreamId::GAME_RPC);

    /// The reliable stream this channel sends on; `None` for [`Channel::Unreliable`].
    pub fn stream(self) -> Option<StreamId> {
        match self {
            Self::Unreliable => None,
            Self::Stream(s) => Some(s),
        }
    }

    /// Whether messages on this channel are resent until acknowledged.
    pub fn is_reliable(self) -> bool {
        self.stream().is_some()
    }

    /// The channel a message received on `stream` is reported on: `Stream(stream)` for every
    /// stream, stream 2 included ([`Channel::GAME_RPC`]).
    pub fn of_stream(stream: StreamId) -> Self {
        Self::Stream(stream)
    }
}

/// A reliable stream of a connection (spec 120 § 5.1). Ids 0-7 are the engine's
/// ([`ENGINE_STREAMS`]); a game declares up to 24 more in `LinkConfig::streams`, ids 8-31.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StreamId(pub u8);

impl StreamId {
    /// Session control, kicks, travel and path validation answers (ordered, priority 0).
    pub const CONTROL: Self = Self(0);
    /// The reliable input lane (ADR-7896) and large commands (unordered, priority 1).
    pub const INPUTS: Self = Self(1);
    /// Reliable game messages ([`Channel::GAME_RPC`]; ordered, priority 2).
    pub const GAME_RPC: Self = Self(2);
    /// Replication's reliable frames: spawns, despawns, oversize records, ownership, and join
    /// snapshots: a game's world blob for a joining client goes here, ahead of state frames
    /// (ordered, priority 2; ADR-8624).
    pub const REPLICATION: Self = Self(3);
    /// Lockstep membership, delay changes and desync probes (ordered, priority 1).
    pub const LOCKSTEP: Self = Self(4);
    /// Chat and spectator text (ordered, priority 3).
    pub const CHAT: Self = Self(5);
    /// Session streams, save transfers and other large transfers nothing waits on (ordered,
    /// priority 4, the lowest: it only fills what the path carries without a queue). A join
    /// snapshot does not go here: a join would wait behind state frames ([`Self::REPLICATION`]).
    pub const BULK: Self = Self(6);
    /// `ReliableUnordered` messages (unordered, priority 2).
    pub const UNORDERED_RPC: Self = Self(7);
    /// The first id a game may declare.
    pub const FIRST_GAME: u8 = ENGINE_STREAMS.len() as u8;

    /// The stream's index in the stream table.
    pub fn index(self) -> usize {
        usize::from(self.0)
    }
}

/// How one reliable stream delivers and how much it may hold (spec 120 § 5.1, § 5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamSpec {
    /// Delivered in send order within the stream; `false` delivers each message as soon as all
    /// its fragments are in, past any gap.
    pub ordered: bool,
    /// 0 is the highest. New data and repairs of a higher-priority stream leave first; streams at
    /// [`BACKGROUND_PRIORITY`] or lower only fill what the path carries without a queue.
    pub priority: u8,
    /// Bytes of fragments the receiver stores ahead of a gap on this stream, and the flow-control
    /// window it grants the sender (RFC 9000 § 4.1 `MAX_STREAM_DATA`). Storing more is the protocol
    /// error [`LinkFailure::StreamOverCap`]; an honest sender never sends past the window.
    pub window_bytes: usize,
    /// Most messages this stream starts sending in any one second (spec 120 § 4.5 A2): messages
    /// over it wait in the queue and leave as the window moves, and the stream's
    /// [`StreamStatus::msgs_capped`] counts the polls it held one back. `0` is no cap (the bulk
    /// stream, which flow control and the path's bandwidth-delay product already bound).
    /// Missing from a serialized table, it takes [`DEFAULT_STREAM_MSGS_PER_S`].
    #[serde(default = "default_stream_msgs_per_s")]
    pub msgs_per_s: u32,
    /// Share of new data this stream gets against the other streams of its priority (weighted
    /// round robin by fragment, spec 120 § 5.3): a stream of weight 2 sends two fragments for
    /// every one of a weight-1 stream while both have data. `0` counts as 1. Missing from a
    /// serialized table, it takes 1.
    #[serde(default = "default_stream_weight")]
    pub weight: u8,
}

/// Default [`StreamSpec::msgs_per_s`]: 240 messages per second (four per tick at 60 Hz, eight at
/// 30 Hz), far above what a game sends on purpose and far below a dirty-object flood.
pub const DEFAULT_STREAM_MSGS_PER_S: u32 = 240;
/// [`StreamSpec::msgs_per_s`] of the chat stream: 30 messages per second.
pub const CHAT_STREAM_MSGS_PER_S: u32 = 30;

fn default_message_caps() -> bool {
    true
}

fn default_stream_msgs_per_s() -> u32 {
    DEFAULT_STREAM_MSGS_PER_S
}

fn default_stream_weight() -> u8 {
    1
}

impl StreamSpec {
    /// An ordered stream of `priority` with the default window, message cap and weight.
    pub const fn ordered(priority: u8) -> Self {
        Self {
            ordered: true,
            priority,
            window_bytes: DEFAULT_STREAM_WINDOW_BYTES,
            msgs_per_s: DEFAULT_STREAM_MSGS_PER_S,
            weight: 1,
        }
    }

    /// An unordered stream of `priority` with the default window, message cap and weight.
    pub const fn unordered(priority: u8) -> Self {
        Self {
            ordered: false,
            priority,
            window_bytes: DEFAULT_STREAM_WINDOW_BYTES,
            msgs_per_s: DEFAULT_STREAM_MSGS_PER_S,
            weight: 1,
        }
    }

    /// The same stream with a cap of `msgs_per_s` messages per second (`0`: no cap).
    pub const fn with_msgs_per_s(mut self, msgs_per_s: u32) -> Self {
        self.msgs_per_s = msgs_per_s;
        self
    }

    /// The same stream with a round-robin weight of `weight` within its priority.
    pub const fn with_weight(mut self, weight: u8) -> Self {
        self.weight = weight;
        self
    }
}

/// The class of traffic a reliable stream carries, the name the per-class counters report under
/// (spec 120 § 4.5 A2): the engine's streams by role, every game-declared stream as `game`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamClass {
    /// Stream 0.
    Control,
    /// Stream 1.
    Inputs,
    /// Stream 2.
    GameRpc,
    /// Stream 3.
    Replication,
    /// Stream 4.
    Lockstep,
    /// Stream 5.
    Chat,
    /// Stream 6.
    Bulk,
    /// Stream 7.
    UnorderedRpc,
    /// A stream the game declared (ids 8 and up).
    Game,
}

impl StreamClass {
    /// The class of `stream`.
    pub fn of(stream: StreamId) -> Self {
        match stream.0 {
            0 => Self::Control,
            1 => Self::Inputs,
            2 => Self::GameRpc,
            3 => Self::Replication,
            4 => Self::Lockstep,
            5 => Self::Chat,
            6 => Self::Bulk,
            7 => Self::UnorderedRpc,
            _ => Self::Game,
        }
    }

    /// The class name: `control`, `inputs`, `game_rpc`, `replication`, `lockstep`, `chat`,
    /// `bulk`, `unordered_rpc` or `game`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Inputs => "inputs",
            Self::GameRpc => "game_rpc",
            Self::Replication => "replication",
            Self::Lockstep => "lockstep",
            Self::Chat => "chat",
            Self::Bulk => "bulk",
            Self::UnorderedRpc => "unordered_rpc",
            Self::Game => "game",
        }
    }
}

/// What one reliable stream holds now ([`Connection::stream_status`]; the agent protocol's
/// `streams` rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamStatus {
    /// The stream.
    pub stream: StreamId,
    /// Its table entry.
    pub spec: StreamSpec,
    /// Fragments queued to send and not yet acknowledged (the admission count).
    pub queued_fragments: usize,
    /// Bytes the peer's flow-control credit still allows past what was sent.
    pub send_credit_bytes: u64,
    /// Received bytes stored ahead of a gap (at most `spec.window_bytes`).
    pub buffered_bytes: usize,
    /// Bytes of the message being reassembled in order.
    pub assembling_bytes: usize,
    /// Peer traffic, in milliseconds, this stream has waited on a missing fragment (it fails
    /// the connection at `Connection::reassembly_stall_ms`).
    pub stall_ms: u64,
    /// The class the stream's counters are named by (spec 120 § 4.5 A2).
    #[serde(default = "default_stream_class")]
    pub class: StreamClass,
    /// Polls in which the stream's [`StreamSpec::msgs_per_s`] cap held a new message back.
    #[serde(default)]
    pub msgs_capped: u64,
    /// Messages the stream started sending in the last second (at most its cap).
    #[serde(default)]
    pub msgs_last_second: u32,
}

fn default_stream_class() -> StreamClass {
    StreamClass::Game
}

/// Default [`StreamSpec::window_bytes`]: 256 KiB (spec 120 § 5.2).
pub const DEFAULT_STREAM_WINDOW_BYTES: usize = 256 * 1024;
/// The bulk stream's window: 1 MiB (spec 120 § 5.2).
pub const BULK_STREAM_WINDOW_BYTES: usize = 1024 * 1024;
/// How long background (bulk) data may wait behind other unreliable data before one packet of it
/// goes first (spec 120 § 5.3, ADR-8407): join snapshots ride the bulk stream, and a congested
/// link whose state frames take the whole allowance must not hold a join forever.
pub const BACKGROUND_STARVE_MS: u64 = 250;
/// Streams of this priority or lower are background traffic: new data on them only fills the
/// path's measured bandwidth-delay product, so they never build a queue in front of gameplay.
pub const BACKGROUND_PRIORITY: u8 = 4;
/// Reliable streams one connection may have: 8 engine streams and up to 24 a game declares.
pub const MAX_STREAMS: usize = 32;
/// The engine's streams, ids 0-7 (spec 120 § 5.1).
pub const ENGINE_STREAMS: [StreamSpec; 8] = [
    StreamSpec::ordered(0),
    StreamSpec::unordered(1),
    StreamSpec::ordered(2),
    StreamSpec::ordered(2),
    StreamSpec::ordered(1),
    StreamSpec::ordered(3).with_msgs_per_s(CHAT_STREAM_MSGS_PER_S),
    StreamSpec {
        ordered: true,
        priority: BACKGROUND_PRIORITY,
        window_bytes: BULK_STREAM_WINDOW_BYTES,
        msgs_per_s: 0,
        weight: 1,
    },
    StreamSpec::unordered(2),
];

/// Bytes every packet reserves for its fixed header: the flags byte, a packet number of up to four
/// bytes and four bytes so a small acknowledgment frame fits next to a full record. An
/// acknowledgment frame takes the room the records leave; when even its smallest form does not
/// fit, it goes in a packet of its own in the same poll.
pub const PACKET_HEADER_BYTES: usize = 9;
const FLAG_PN_LEN: u8 = 0x03;
const FLAG_ACK: u8 = 0x04;
const FLAG_ACK_TIMESTAMPS: u8 = 0x08;
const FLAG_WANT_TIMESTAMPS: u8 = 0x10;
const FLAG_ACK_OLDER: u8 = 0x20;
const FLAG_KNOWN: u8 = FLAG_PN_LEN | FLAG_ACK | FLAG_ACK_TIMESTAMPS | FLAG_WANT_TIMESTAMPS | FLAG_ACK_OLDER;
/// Extra acknowledgment frames, for ranges older than the newest frame names, one poll may send
/// (each in an acknowledgment-only packet, oldest first).
const MAX_OLDER_ACK_FRAMES: usize = 4;
const RECORD_UNRELIABLE: u8 = 0x01;
const RECORD_RELIABLE: u8 = 0x02;
const RECORD_PING: u8 = 0x03;
const RECORD_PADDING: u8 = 0x00;
const RECORD_MORE: u8 = 0x80;
/// A reliable fragment on stream `tag & RECORD_STREAM_ID` (`0x40 | stream`, plus
/// [`RECORD_FIRST`] and [`RECORD_MORE`]). Stream 2 is also written as the shorter-named
/// [`RECORD_RELIABLE`] form, which carries no first flag (stream 2 is ordered).
const RECORD_STREAM: u8 = 0x40;
/// The fragment starts a message (what an unordered stream needs to deliver past a gap).
const RECORD_FIRST: u8 = 0x20;
const RECORD_STREAM_ID: u8 = 0x1F;
/// Flow-control credit (RFC 9000 § 19.10 `MAX_STREAM_DATA`): `stream u8 | max_data varint`, the
/// stream offset, in bytes, up to which the sender may send.
const RECORD_CREDIT: u8 = 0x04;
const UNRELIABLE_OVERHEAD: usize = 3;
const RELIABLE_OVERHEAD: usize = 5;
/// Reliable receive window, in fragments. A protocol constant, not a setting, so both ends agree;
/// `LinkConfig::max_reliable_in_flight` is clamped to it. Must stay well below 32768 so an old id
/// can never look like a new one.
pub const RELIABLE_WINDOW: usize = 1024;
/// A packet whose number is more than this far ahead of the largest one received is refused,
/// unless [`RESYNC_AFTER`] such packets arrive in a row (a genuine long outage).
pub const MAX_FORWARD_JUMP: u64 = 1024;
/// Consecutive far-ahead packets after which the receiver accepts the new sequence position.
pub const RESYNC_AFTER: u32 = 8;
/// Records one packet may carry. A packet with more is malformed, so a small datagram cannot cost a
/// thousand allocations; [`Connection::poll`] never packs more.
pub const MAX_RECORDS_PER_PACKET: usize = 128;
/// Largest value of `LinkConfig::max_reliable_message_bytes`, whatever the configuration says:
/// reassembly grows to it on a peer's say-so.
pub const MAX_RELIABLE_MESSAGE_LIMIT: usize = 16 * 1024 * 1024;
/// Messages waiting in [`Connection::next_message`]. Past it unreliable messages are dropped and a
/// reliable one fails the connection (it cannot be dropped).
pub const MAX_RECEIVED_MESSAGES: usize = 8192;
/// Sent-packet records kept for acknowledgement.
const SENT_WINDOW: usize = 1024;
/// Queued unreliable record bytes, including framing, per priority class (ADR-7949).
pub const MAX_UNRELIABLE_QUEUE_BYTES: usize = 64 * 1024;
/// Resolved packets (acked or lost) the loss fraction averages over.
const LOSS_SAMPLES: usize = 128;
/// Packets carrying reliable data one [`Connection::poll`] may send when a connection starts
/// recovering from an outage (ADR-7950; an initial budget inspired by RFC 5681 § 3.1). Also the
/// smallest in-flight window, in packets (ADR-7995).
pub const RECOVERY_INITIAL_PACKETS: usize = 4;
/// Reliable-bearing packets in any rolling second during recovery when nothing is known about the
/// path's earlier delivery rate (ADR-7950). Keepalives remain independent so a full data budget
/// cannot prevent acknowledgment traffic.
pub const RECOVERY_PACKETS_PER_SECOND: usize = 32;
/// Default of `LinkConfig::resend_backoff_max_ms`: the longest a timed-out fragment waits before
/// its next resend during an outage (ADR-7950).
pub const DEFAULT_RESEND_BACKOFF_MAX_MS: u64 = 8_000;
/// Doublings of the probe timeout (2^16 x the timeout is far past any cap).
const MAX_BACKOFF_SHIFT: u32 = 16;
/// A sent packet is lost once a packet sent this many later was acknowledged (the packet
/// threshold of RFC 9002 § 6.1.1; spec 120 § 4.1 item 4).
pub const LOSS_PACKET_THRESHOLD: u16 = 3;
/// Packets carrying resent reliable data one [`Connection::poll`] may produce, whether a loss
/// verdict or a timer asked for them (ADR-7995): a peer's acknowledgments cannot turn one poll
/// into a burst of repeats.
pub const MAX_RESEND_PACKETS_PER_POLL: usize = 8;
/// A poll's loss repairs are spread over this share of the round trip (ADR-10125): a quarter. A
/// poll sends [`MAX_RESEND_PACKETS_PER_POLL`] packets of repairs divided by the number of ticks in
/// a quarter of the round trip, at least one. Up to seven ticks of round trip that is the whole
/// burst, as before; from twenty ticks on it is one repair packet per poll, each the first packet
/// of its own poll.
pub const REPAIR_SPREAD_ROUND_TRIP_DIVISOR: u64 = 4;
/// While loss repairs leave one per poll (a round trip of twenty ticks or more), ordinary
/// unreliable data waits for them, for at most the round trip over this at a time (ADR-10125): a
/// quarter. The state of a tick that waited is dropped as any stale state is; the next tick sends
/// a newer one.
pub const REPAIR_QUIET_ROUND_TRIP_DIVISOR: u64 = 4;
/// On a path that is not measured yet, or whose round trip spreads repairs
/// ([`Connection::repair_packets_per_poll`] below [`MAX_RESEND_PACKETS_PER_POLL`]), a reliable
/// message of more than one fragment is cut into fragments of a packet's fragment room over this (ADR-10189): half
/// fragments, two to a packet. A fragment can only be repaired whole, and a queue that counts
/// bytes and is kept full takes a half-size packet far more often than a full one.
pub const SMALL_FRAGMENT_DIVISOR: usize = 2;
/// Half fragments use twice the fragment ids. A message is cut small only while its stream then
/// holds at most `LinkConfig::max_reliable_in_flight` over this in queued fragments (ADR-10189);
/// a longer backlog keeps full fragments, so the small cut never costs a sender more than an
/// eighth of the ids it had.
pub const SMALL_FRAGMENT_QUEUE_DIVISOR: usize = 4;
/// Steps of the repeat-repair stagger (ADR-10088): a repair after two or more losses in a row
/// leaves 0 to 7 steps later than one round trip.
pub const REPEAT_REPAIR_STAGGER_SLOTS: u64 = 8;
/// One stagger step is the round trip over this: at most 7/32 of a round trip in all.
pub const REPEAT_REPAIR_STAGGER_DIVISOR: u64 = 32;
/// The stagger step advances by this many slots per loss (coprime with the slot count, so eight
/// losses in a row visit every slot).
const REPEAT_REPAIR_STAGGER_STRIDE: u64 = 3;
/// Ranges one acknowledgment frame carries (spec 120 § 4.1 item 2, ADR-8184): one frame names the
/// newest 32 runs of received packets, so a burst of any length is acknowledged by one reply.
pub const MAX_ACK_RANGES: usize = crate::ack::MAX_ACK_FRAME_RANGES;
/// The longest a receiver holds an acknowledgment for an ack-eliciting packet, and the most of
/// a peer's reported ack delay a round-trip sample subtracts (RFC 9000 § 13.2.1, spec 120 § 4.1
/// item 2).
pub const MAX_ACK_DELAY_MS: u64 = 10;
/// Ack-eliciting packets after which a receiver acknowledges at once (RFC 9000 § 13.2.2).
pub const ACK_ELICITING_THRESHOLD: u32 = 2;
/// The window the minimum round trip is kept over (spec 120 § 4.1 item 6).
pub const MIN_RTT_WINDOW_MS: u64 = 10_000;
/// Probe packets one probe timeout may send (RFC 9002 § 6.2.4).
pub const MAX_PROBE_PACKETS: usize = 2;
/// A reliable fragment of at most this many bytes rides once more, as a redundant copy, in the
/// next packet that leaves within [`REDUNDANT_COPY_WINDOW_MS`] of its first transmission and has
/// room (ADR-8184), and so does each repair of it (ADR-8400). A lost first copy then costs one
/// packet interval instead of a round trip and a half; no datagram is sent for it.
pub const REDUNDANT_COPY_MAX_BYTES: usize = 256;
/// See [`REDUNDANT_COPY_MAX_BYTES`].
pub const REDUNDANT_COPY_WINDOW_MS: u64 = 20;
/// Redundant copies one packet carries.
const MAX_COPIES_PER_PACKET: usize = 4;
/// A packet declared lost stays acknowledgeable until the newest acknowledged packet is this many
/// packet numbers past it, so a late acknowledgment can still undo the loss.
const LATE_ACK_WINDOW: u64 = 1024;
/// The packet threshold never adapts past this (a path that reorders further is judged by time).
const MAX_REORDER_THRESHOLD: u64 = 128;
/// Arrivals whose receive time waits for the next acknowledgment frame.
const MAX_PENDING_TIMESTAMPS: usize = crate::ack::MAX_ACK_TIMESTAMPS;
/// Receive timestamps the peer reported that wait in [`Connection::take_receive_timestamps`].
pub const MAX_RECEIVE_TIMESTAMPS: usize = 1024;
/// Out-of-order arrivals remembered so a confirmed acknowledgment frame never stops naming a
/// packet that filled one of its gaps after it left (ADR-8423). When more arrive between a frame
/// and its confirmation, that confirmation is skipped and a later one trims instead.
const LATE_ARRIVALS_KEPT: usize = 64;
/// The extra time a path's observed reordering may add to the loss delay, in milliseconds.
const MAX_REORDER_WINDOW_MS: u64 = 1_000;
/// How long a delivery-rate sample counts toward the background streams' path estimate.
const RATE_WINDOW_MS: u64 = 2_000;
/// Background streams hold back when the queueing delay (latest minus minimum round trip) is
/// over this many milliseconds, or over a sixteenth of the minimum round trip if that is more.
const BACKGROUND_QUEUE_MS: f64 = 2.0;
/// Window-limited round trips whose delivery decides a smaller in-flight window.
const WINDOW_ROUNDS: usize = 3;
/// The first in-flight window is never smaller than this many packets, whatever
/// `LinkConfig::max_packets_per_poll` says (about the initial window of RFC 6928).
const INITIAL_WINDOW_PACKETS: usize = 16;

/// One queued unreliable message.
#[derive(Debug)]
struct OutUnreliable {
    id: u64,
    bytes: Vec<u8>,
    /// The first poll that saw it: it expires one tick later.
    first_poll_ms: Option<u64>,
}
/// Default [`LinkConfig::connection_buffer_bytes`]: 2 MiB (spec 120 § 5.2).
pub const DEFAULT_CONNECTION_BUFFER_BYTES: usize = 2 * 1024 * 1024;
/// A reassembly gap is a stall after at least this long, in milliseconds (spec 120 § 5.2).
pub const REASSEMBLY_STALL_MIN_MS: u64 = 2_000;
/// A reassembly gap is a stall after this many retransmission timeouts, when that is longer than
/// [`REASSEMBLY_STALL_MIN_MS`].
pub const REASSEMBLY_STALL_RTO_MULTIPLE: u64 = 4;
/// Repair attempts of an honest sender the base stall deadline (`max(4 x RTO, 2 s)`) covers: a
/// sender finds a lost fragment by loss detection and resends it about every two RTOs (ADR-8521).
pub const REASSEMBLY_STALL_BASE_ATTEMPTS: u64 = 2;
/// Most repair attempts the stall deadline grants at any measured loss: the deadline is never
/// more than `REASSEMBLY_STALL_MAX_ATTEMPTS / REASSEMBLY_STALL_BASE_ATTEMPTS` (4) times the base,
/// whatever a peer's skipped packet numbers claim (ADR-8521).
pub const REASSEMBLY_STALL_MAX_ATTEMPTS: u64 = 8;
/// The stall deadline grants the repair attempts an honest sender could lose in a row with a chance
/// above this, at the peer's measured packet loss (ADR-8521).
pub const REASSEMBLY_STALL_HONEST_MISS: f64 = 1e-5;
/// Peer packet numbers the inbound loss behind the stall deadline is measured over (ADR-8521).
pub const INBOUND_LOSS_WINDOW: u64 = 256;

/// Tuning for one connection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkConfig {
    /// Largest packet body, endpoint header included (keep under the path MTU; 1200 is safe on
    /// IPv4 and IPv6).
    pub max_packet_bytes: usize,
    /// Lower bound of the probe timeout before backoff, in milliseconds (also its value before
    /// the first round-trip sample).
    pub resend_min_ms: u64,
    /// Upper bound of the probe timeout computed from the estimator, in milliseconds. On a path
    /// whose measured round trip is longer, the probe timeout is still 9/8 of that round trip
    /// (ADR-7995): a timer never undercuts the path.
    pub resend_max_ms: u64,
    /// A keepalive (a ping the peer acknowledges) is sent when nothing else went out for this
    /// long; an acknowledgment that is due anyway serves as one (ADR-8184).
    pub keepalive_ms: u64,
    /// Largest reliable message; larger sends are refused.
    pub max_reliable_message_bytes: usize,
    /// Reliable fragment admission cap; more sends are refused (backpressure).
    /// Clamped to `1..=RELIABLE_WINDOW`. Reframing unsent work after a transport size refusal may
    /// temporarily exceed this queue cap; the transmitted window remains bounded.
    pub max_reliable_in_flight: usize,
    /// Packets one [`Connection::poll`] may produce (acknowledgment-only packets for ranges older
    /// than one frame names, at most four, come on top). Times `max_packet_bytes`, it is also
    /// the first in-flight window of reliable data, which is never under sixteen packets
    /// (ADR-7995).
    pub max_packets_per_poll: usize,
    /// Longest probe timeout while nothing is acknowledged, in milliseconds: each consecutive
    /// expiry doubles it up to this (ADR-7950, ADR-8184, RFC 9002 § 6.2.1). Never below the probe
    /// timeout itself.
    #[serde(default = "default_resend_backoff_max_ms")]
    pub resend_backoff_max_ms: u64,
    /// Ask the peer to report receive timestamps for up to 16 packets in each acknowledgment frame
    /// (spec 120 § 4.1 item 3, for the delay-gradient signal of the rate controller). Negotiated
    /// in band: each side reports them only to a peer that asks. Read them with
    /// [`Connection::take_receive_timestamps`]. Off by default.
    #[serde(default)]
    pub receive_timestamps: bool,
    /// Path MTU discovery for this side's packets (spec 120 § 4.3, RFC 8899, ADR-8300): padded
    /// probes of 1,280, 1,400 and 1,452 bytes, confirmed only by an acknowledgment, raise the
    /// packet size above `max_packet_bytes` (the base); a black hole drops back to it. Reliable
    /// fragments stay cut for the base size; unreliable messages and packing use the probed size
    /// ([`Connection::max_unreliable_bytes`] reads it live). Off by default: with it off nothing on
    /// the wire changes.
    #[serde(default)]
    pub mtu_probing: bool,
    /// The send-rate controller and pacer (spec 120 § 4.2, ADR-8301). On by default: datagrams
    /// leave at 1.25x the controller's target, the first burst being the initial window.
    #[serde(default)]
    pub rate: crate::cc::RateConfig,
    /// Most bytes of reliable fragments stored ahead of a gap (received out of order, waiting for
    /// an earlier one). A peer that makes the connection store more fails it as
    /// [`LinkFailure::ReassemblyOverCap`]. The cap in force is never above one receive window of
    /// full fragments ([`RELIABLE_WINDOW`] x the largest fragment of `max_packet_bytes`), which is
    /// the most an honest sender can have outstanding; the default (2 MiB) is above that for every
    /// packet size, so only the window bounds it. In-order reassembly is bounded separately by
    /// `max_reliable_message_bytes`. Spec 120 § 5.2. Missing from a serialized config, it takes
    /// the default.
    #[serde(default = "default_connection_buffer_bytes")]
    pub connection_buffer_bytes: usize,
    /// The game's reliable streams, ids 8 and up in order (at most 24; spec 120 § 5.1). Both ends
    /// of a session must declare the same table, like the protocol itself. Empty by default.
    #[serde(default)]
    pub streams: Vec<StreamSpec>,
    /// Apply each stream's [`StreamSpec::msgs_per_s`] cap (spec 120 § 4.5 A2, ADR-8407). On by
    /// default; `false` lets every stream start any number of messages per second (a transfer
    /// tool or a throughput test that knows what it sends). Missing from a serialized config, it
    /// takes the default.
    #[serde(default = "default_message_caps")]
    pub message_caps: bool,
}

fn default_resend_backoff_max_ms() -> u64 {
    DEFAULT_RESEND_BACKOFF_MAX_MS
}

fn default_connection_buffer_bytes() -> usize {
    DEFAULT_CONNECTION_BUFFER_BYTES
}

impl Default for LinkConfig {
    fn default() -> Self {
        Self {
            max_packet_bytes: 1200,
            resend_min_ms: 100,
            resend_max_ms: 1_000,
            keepalive_ms: 100,
            max_reliable_message_bytes: 256 * 1024,
            max_reliable_in_flight: 1024,
            max_packets_per_poll: 64,
            resend_backoff_max_ms: DEFAULT_RESEND_BACKOFF_MAX_MS,
            receive_timestamps: false,
            mtu_probing: false,
            rate: crate::cc::RateConfig::default(),
            connection_buffer_bytes: DEFAULT_CONNECTION_BUFFER_BYTES,
            streams: Vec::new(),
            message_caps: true,
        }
    }
}

/// Why a [`Connection`] failed for good (spec 120 § 5.2, § 11.2; ADR-7981). Each is a protocol
/// violation by the peer: an honest sender never causes one. After it every
/// [`Connection::receive`] is an error and the stored fragments are freed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkFailure {
    /// A reassembled reliable message grew past `LinkConfig::max_reliable_message_bytes`.
    MessageTooLong,
    /// More delivered messages waited than [`MAX_RECEIVED_MESSAGES`]; a reliable one cannot be
    /// dropped.
    InboxFull,
    /// A reliable fragment longer than one packet of this connection's configuration can carry
    /// ([`Connection::max_fragment_bytes`]).
    FragmentTooLong,
    /// Fragments stored ahead of a gap passed the connection's cap
    /// ([`Connection::reassembly_cap_bytes`]).
    ReassemblyOverCap,
    /// The next expected fragment did not arrive for `max(4 x RTO, 2 s)` of the peer's traffic
    /// while later fragments, or the start of a message, waited for it.
    ReassemblyStalled,
    /// Fragments stored ahead of a gap on one stream passed that stream's window
    /// ([`StreamSpec::window_bytes`]): the peer sent past the flow-control credit it was given
    /// (spec 120 § 5.2).
    StreamOverCap,
    /// A reliable fragment named a stream the connection's table does not declare.
    UnknownStream,
}

impl LinkFailure {
    /// Stable lowercase name (`message_too_long`, `inbox_full`, `fragment_too_long`,
    /// `reassembly_over_cap`, `reassembly_stalled`, `stream_over_cap`, `unknown_stream`), for
    /// logs and the agent protocol.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::MessageTooLong => "message_too_long",
            Self::InboxFull => "inbox_full",
            Self::FragmentTooLong => "fragment_too_long",
            Self::ReassemblyOverCap => "reassembly_over_cap",
            Self::ReassemblyStalled => "reassembly_stalled",
            Self::StreamOverCap => "stream_over_cap",
            Self::UnknownStream => "unknown_stream",
        }
    }
}

/// Bytes of out-of-order reliable fragments that many connections may store together: one server's
/// share across its peers (spec 120 § 5.2, `server_buffer_bytes`). Pass it to
/// [`Connection::receive_within`]. A packet that would store past it is refused whole, as if lost
/// (nothing in it is acknowledged, so an honest sender resends it later), and counted in
/// [`LinkStats::reassembly_refused`]; a packet that fills a gap always gets in, because it frees
/// more than it stores. The connection that would overflow it is not failed for that alone: the
/// budget is shared, so an honest peer must not pay for another peer's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReassemblyBudget {
    limit: usize,
    used: usize,
}

impl ReassemblyBudget {
    /// A budget of `limit` bytes.
    pub fn new(limit: usize) -> Self {
        Self { limit, used: 0 }
    }

    /// No shared limit (each connection's own cap still applies).
    pub fn unlimited() -> Self {
        Self::new(usize::MAX)
    }

    /// The limit, in bytes.
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes stored now by the connections that use it.
    pub fn used(&self) -> usize {
        self.used
    }

    /// Returns a dropped connection's bytes ([`LinkStats::reliable_buffered_bytes`]).
    pub fn release(&mut self, bytes: usize) {
        self.used = self.used.saturating_sub(bytes);
    }

    fn apply(&mut self, before: usize, after: usize) {
        self.used = self.used.saturating_sub(before).saturating_add(after);
    }
}

/// Why a send was refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SendError {
    /// The message exceeds the channel's size limit.
    #[error("message of {len} bytes exceeds the {max}-byte limit of this channel")]
    TooLarge {
        /// Message length.
        len: usize,
        /// Channel limit.
        max: usize,
    },
    /// A reliable fragment window or unreliable byte queue is full.
    #[error("send queue is full ({0} records queued)")]
    Backpressure(usize),
    /// The peer is not connected.
    #[error("peer is not connected")]
    NotConnected,
    /// The channel names a stream the connection's table does not declare.
    #[error("stream {0} is not declared")]
    UnknownStream(u8),
    /// The per-second reliable message cap for this peer is used up.
    #[error("reliable message rate limit reached")]
    RateLimited,
}

/// Counters and estimates for one connection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct LinkStats {
    /// Packets sent.
    pub packets_sent: u64,
    /// Packets received (duplicates excluded).
    pub packets_received: u64,
    /// Sent packets the peer acknowledged.
    pub packets_acked: u64,
    /// Sent packets counted lost: a later packet was acknowledged and this one was not, by the
    /// packet or time threshold (ADR-7995). A packet acknowledged after all is taken back out.
    pub packets_lost: u64,
    /// Bytes sent (packet bodies, endpoint header excluded).
    pub bytes_sent: u64,
    /// Bytes received.
    pub bytes_received: u64,
    /// Reliable fragments sent again, after a loss verdict or a timeout.
    pub reliable_resends: u64,
    /// Unreliable messages dropped after their queue deadline expired (one tick after the first
    /// poll that could not send them), or stranded by a transport budget that shrank below them.
    pub unreliable_dropped: u64,
    /// Duplicate packets ignored.
    pub duplicates: u64,
    /// Smoothed round-trip time in milliseconds (0 until the first sample).
    pub rtt_ms: f64,
    /// Round-trip variation in milliseconds, the jitter estimate: RFC 6298's `RTTVAR` filter seeded
    /// from the samples (0 at the first sample, not `RTT / 2`), so a fresh link does not report a
    /// jitter of half its round trip (spec 120 § 9.4, ADR-8219). The retransmission timeout keeps
    /// RFC 6298's own `RTTVAR`.
    pub jitter_ms: f64,
    /// Fraction of the last 128 resolved packets that were lost.
    pub loss_fraction: f32,
    /// Send rate over the last full second, kilobits per second.
    pub send_kbps: f64,
    /// Receive rate over the last full second, kilobits per second.
    pub recv_kbps: f64,
    /// Reliable fragments waiting for acknowledgement.
    pub reliable_in_flight: usize,
    /// Packets refused for jumping too far ahead of the sequence.
    pub out_of_window: u64,
    /// Unreliable messages dropped on receipt because too many waited.
    pub inbound_dropped: u64,
    /// Times the connection entered recovery: a fragment timed out twice in a row with nothing
    /// acknowledged, or losses were found after that much silence (ADR-7950, ADR-7995).
    #[serde(default)]
    pub recoveries: u64,
    /// True while the connection paces reliable packets after an outage (ADR-7950).
    #[serde(default)]
    pub recovering: bool,
    /// Polls that could send nothing, not even an acknowledgment, because the transport's
    /// datagram budget was below the 256 bytes this protocol needs (ADR-7995). A peer behind such
    /// a transport times out; this counter says why.
    #[serde(default)]
    pub transport_stalls: u64,
    /// The smallest round trip seen over the last 10 s, milliseconds (0 until the first sample;
    /// spec 120 § 4.1 item 6, for the rate controller).
    #[serde(default)]
    pub min_rtt_ms: f64,
    /// Packets declared lost that were acknowledged after all (reordering, not loss; DO27 rule
    /// A7). Each one was taken back out of [`Self::packets_lost`].
    #[serde(default)]
    pub spurious_losses: u64,
    /// Probe timeouts that expired (RFC 9002 § 6.2), each sending one or two probe packets.
    #[serde(default)]
    pub probe_timeouts: u64,
    /// Acknowledgment-only packets sent (not ack-eliciting).
    #[serde(default)]
    pub ack_only_sent: u64,
    /// Redundant copies of small reliable fragments sent in packets that left anyway (not
    /// counted in [`Self::reliable_resends`]).
    #[serde(default)]
    pub redundant_copies: u64,
    /// The path MTU, every header and the seal included: the size path MTU discovery has
    /// confirmed with `mtu_probing` on (spec 120 § 4.3), else `max_packet_bytes`. While a lost
    /// large packet has the size in doubt data goes at the base size for about a round trip; the
    /// figure stays (ADR-10097). 0 in stats built by hand.
    #[serde(default)]
    pub path_mtu: u32,
    /// Path MTU probe packets sent (search and confirmation; ADR-8300).
    #[serde(default)]
    pub mtu_probes_sent: u64,
    /// Path MTU probe packets declared lost (a packet sent after them was acknowledged).
    #[serde(default)]
    pub mtu_probes_lost: u64,
    /// Path MTU black holes detected; each one dropped the packet size back to the base.
    #[serde(default)]
    pub mtu_black_holes: u64,
    /// Where path MTU discovery stands (`disabled` with `mtu_probing` off).
    #[serde(default)]
    pub mtu_search: crate::pmtud::MtuSearch,
    /// Bytes of acknowledgment frames sent (a part of [`Self::bytes_sent`]): what the
    /// acknowledgments cost on this link (ADR-8423).
    #[serde(default)]
    pub ack_frame_bytes: u64,
    /// Queueing delay over the minimum round trip in the rate controller's latest round,
    /// milliseconds (spec 120 § 4.2, § 13.1 `PeerQuality::queue_delay_ms`).
    #[serde(default)]
    pub queue_delay_ms: f64,
    /// Bytes per second the rate controller lets the game's own traffic use now: the target (or
    /// the shared uplink's fair share, if lower) minus what the essential class used (spec 120
    /// § 4.2 item 9, § 13.1 `PeerQuality::send_allowance_bps`). `u32::MAX` with the controller
    /// off.
    #[serde(default)]
    pub send_allowance_bps: u32,
    /// The rate controller's target, bytes per second (`u32::MAX` with it off).
    #[serde(default)]
    pub rate_target_bps: u32,
    /// The rate controller's state.
    #[serde(default)]
    pub rate_state: crate::cc::RateState,
    /// Rate cuts so far (delay- or loss-driven).
    #[serde(default)]
    pub rate_cuts: u64,
    /// Polls in which the pacer held data back (the sender wanted more than the target).
    #[serde(default)]
    pub paced_holds: u64,
    /// The path failed to carry the essential floor at over 50 % loss for the circuit-breaker time
    /// (spec 120 § 4.2 item 4): the endpoint reports the peer interrupted.
    #[serde(default)]
    pub rate_breaker: bool,
    /// Bytes of reliable fragments stored ahead of a gap now (spec 120 § 5.2; capped by
    /// [`Connection::reassembly_cap_bytes`]).
    #[serde(default)]
    pub reliable_buffered_bytes: usize,
    /// Bytes of the reliable message being reassembled in order now (capped by
    /// `LinkConfig::max_reliable_message_bytes`).
    #[serde(default)]
    pub reliable_assembling_bytes: usize,
    /// Packets refused whole because storing their fragments would pass a shared
    /// [`ReassemblyBudget`] (treated as lost; the sender resends them).
    #[serde(default)]
    pub reassembly_refused: u64,
    /// Peer traffic, in milliseconds, counted against the reassembly stall now (0 with no gap);
    /// the link fails when it reaches [`LinkStats::reassembly_stall_deadline_ms`] (ADR-8521).
    #[serde(default)]
    pub reassembly_stall_ms: u64,
    /// The reassembly stall deadline in force, as the latest stall check enforced it:
    /// `max(4 x RTO, 2 s)`, stretched up to 4x by the peer's measured packet loss (ADR-8521).
    /// Before the first round-trip sample the RTO in it is never below the wait of this end's
    /// oldest unanswered ack-eliciting packet, up to `LinkConfig::resend_max_ms` (ADR-8658;
    /// reported since ADR-8859). [`Connection::reassembly_stall_ms`] is the same deadline without
    /// that bound.
    #[serde(default)]
    pub reassembly_stall_deadline_ms: u64,
    /// Why the connection failed, if it did (`reassembly_stalled`, ...).
    #[serde(default)]
    pub failure: Option<LinkFailure>,
    /// Reliable messages delivered while another stream, or an earlier message of the same
    /// unordered stream, still waited for a missing fragment: deliveries a single ordered stream
    /// would have held back (spec 120 § 5.1).
    #[serde(default)]
    pub delivered_past_gap: u64,
    /// Polls in which a stream had new data that its flow-control credit held back.
    #[serde(default)]
    pub stream_blocked: u64,
    /// Polls in which background-stream data (bulk) waited because the path's bandwidth-delay
    /// product was full.
    #[serde(default)]
    pub background_held: u64,
    /// Flow-control credit records (`MAX_STREAM_DATA`) sent.
    #[serde(default)]
    pub credits_sent: u64,
    /// Packets the socket refused (a full send buffer, `WouldBlock`, or a send error): they never
    /// left, so they are not counted as sent and spent no packet number (spec 120 § 4.3).
    #[serde(default)]
    pub socket_drops: u64,
}

/// One receive timestamp the peer reported for a packet this side sent (spec 120 § 4.1 item 3):
/// the input of a one-way delay-gradient estimate. Times are each side's own clock: only the
/// differences between packets mean anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiveTimestamp {
    /// The packet number.
    pub packet: u64,
    /// When this side sent it, milliseconds on this side's clock.
    pub sent_ms: u64,
    /// When the peer received it, microseconds relative to the arrival of another packet the
    /// same report named (comparable within one report, see [`Self::report`]).
    pub received_us: i64,
    /// Which acknowledgment frame carried it (counts up): arrival times compare only within
    /// one report.
    pub report: u64,
}

/// Work a connection did, counted in records examined: what the scale tests assert instead of
/// wall time (ADR-10081).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct WorkCounters {
    /// Sent-packet records the loss detector examined.
    pub(crate) loss_scan_records: u64,
}

#[derive(Debug)]
struct SentPacket {
    seq: u64,
    sent_ms: u64,
    /// It carried records (a ping, data or messages): the peer acknowledges it promptly.
    ack_eliciting: bool,
    /// Keys ([`OutFragment::key`]) of the fragments it carried.
    reliable_ids: Vec<u64>,
    /// Flow-control credit it carried: (stream, max_data).
    credits: Vec<(u8, u64)>,
    acked: bool,
    /// Declared lost by the loss detector. It stays acknowledgeable: a late acknowledgment still
    /// acknowledges its fragments and undoes the loss.
    lost: bool,
    /// Position of this packet's loss sample, to undo it.
    loss_sample: u64,
    /// Reliable bytes acknowledged when it left: the base of its delivery sample.
    delivered_at_send: u64,
    /// It carried reliable data.
    data: bool,
    /// It carried a fragment that had been sent before: no round-trip sample is taken from it
    /// (Karn's rule, kept from the ack-poll lane although every packet has its own sequence).
    resent: bool,
    /// Datagram bytes (endpoint header included), for path MTU black-hole detection.
    size: u16,
    /// A path MTU probe (a padded ping, ADR-8300): no loss sample, no loss count.
    mtu_probe: bool,
    /// A path validation probe sent on a candidate path (ADR-8299): created lost, and its
    /// acknowledgment is no sample of the validated path.
    path_probe: bool,
    /// What the acknowledgment frame it carried named, if any: once the peer acknowledges this
    /// packet, those numbers need not be named again (RFC 9000 § 13.2.4, ADR-8423).
    ack_named: Option<FrameNamed>,
    /// Body bytes, for the rate controller's delivery rate.
    bytes: usize,
}

/// What the rate controller's current feedback round has seen so far.
#[derive(Debug, Default, Clone)]
struct RateRound {
    start_ms: Option<u64>,
    acked_bytes: u64,
    sent_bytes: u64,
    acked_packets: u32,
    lost_packets: u32,
    /// Smallest round-trip sample of the round (0 = none).
    min_rtt: f64,
    /// Smallest one-way delay sample of the round on `cc::OneWayDelay`'s scale (0 = none).
    min_one_way: f64,
    /// Delay samples the round took.
    delay_samples: u32,
    /// Polls in which the pacer held data back.
    held: u64,
    /// Bytes of the essential class and of acknowledgment-only packets.
    essential_bytes: u64,
}

/// The span of received packet numbers one sent acknowledgment frame named, and how many
/// arrivals this side had counted when it was built (RFC 9000 § 13.2.4, ADR-8423).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameNamed {
    /// The oldest number the frame named (the smallest of its last range).
    lo: u64,
    /// The frame's largest acknowledged number.
    hi: u64,
    /// `Connection::arrivals` when the frame was built: arrivals counted from here on came
    /// after it and are not covered by it, even inside `lo..=hi`.
    arrivals: u64,
}

#[derive(Debug)]
struct OutFragment {
    /// Position in the connection's send queue (never reused, never on the wire): sent packets
    /// name fragments by it, whatever their stream.
    key: u64,
    /// Stream index.
    stream: u8,
    /// Fragment id within the stream (on the wire).
    id: u16,
    /// Stream offset just past this fragment's bytes (flow control).
    end_offset: u64,
    bytes: Vec<u8>,
    /// The fragment starts a message.
    first: bool,
    more: bool,
    last_sent_ms: Option<u64>,
    /// Number of the packet that carried the latest transmission.
    last_seq: u64,
    acked: bool,
    /// The packet that carried the latest transmission was declared lost: send again.
    lost: bool,
    /// Counted in the in-flight bytes: sent, not acknowledged, not declared lost.
    in_flight: bool,
    /// Probe transmissions of this fragment since [`Self::backoff_epoch`]: a probe takes the
    /// least probed fragments first, so no fragment is repeated while others wait.
    timeouts: u8,
    /// The connection's ack epoch when [`Self::timeouts`] was last counted: an acknowledgment
    /// since then resets the count without touching every fragment.
    backoff_epoch: u64,
    /// When it was first sent.
    first_sent_ms: Option<u64>,
    /// A redundant copy has been sent for the latest transmission that takes one (the first, and
    /// each repair): at most one per transmission.
    copied: bool,
    /// Loss verdicts in a row on this fragment's transmissions (saturating; an acknowledgment
    /// ends the fragment, so nothing resets it). From the second on, its repairs are staggered
    /// (ADR-10088).
    losses: u8,
    /// Cut to a half fragment (ADR-10189). It shares a packet with other half fragments only,
    /// as a full fragment has a packet to itself.
    small_cut: bool,
}

impl OutFragment {
    fn wire_bytes(&self) -> usize {
        self.bytes.len() + RELIABLE_OVERHEAD
    }
}

/// Send side of one reliable stream.
#[derive(Debug, Clone, Default)]
struct SendStream {
    /// The id the next queued fragment gets.
    next_id: u16,
    /// Stream offset past every queued byte.
    queued_end: u64,
    /// The peer's flow-control credit: no fragment ending past it is sent.
    max_data: u64,
    /// Id and start offset of the oldest fragment not yet acknowledged (`next_id` and
    /// `queued_end` when none waits).
    floor_id: u16,
    floor_offset: u64,
    /// Stream offset past the newest fragment sent.
    sent_end: u64,
    /// When each message started in the last second left (its first fragment's first
    /// transmission), oldest first: the [`StreamSpec::msgs_per_s`] window.
    started_ms: VecDeque<u64>,
    /// Polls in which the message cap held a new message back.
    msgs_capped: u64,
}

/// One received fragment waiting on a stream.
#[derive(Debug)]
struct InFragment {
    bytes: Vec<u8>,
    /// Its length (kept after an unordered delivery frees `bytes`).
    len: usize,
    first: bool,
    more: bool,
    /// Already delivered past a gap (unordered stream): only a marker until the gap fills.
    delivered: bool,
}

/// Receive side of one reliable stream.
#[derive(Debug, Default)]
struct RecvStream {
    next_expected: u16,
    stored: HashMap<u16, InFragment>,
    /// Bytes of undelivered fragments in `stored`.
    buffered: usize,
    assembling: Vec<u8>,
    /// Stream offset delivered in order (flow control: the credit is this plus the window).
    consumed: u64,
    /// The newest credit put on the wire, and the newest one a packet the peer acknowledged
    /// carried.
    advertised: u64,
    acked_advert: u64,
    /// When the newest credit last left: unacknowledged after a probe timeout, it leaves again
    /// (a receiver that only sends acknowledgments never learns that its credit was lost).
    advert_sent_ms: u64,
    /// Peer traffic, in milliseconds, since this stream last made progress while something waited.
    stall_ms: u64,
}

/// A reliable candidate's place in a poll's send order: class (0 control, 1 foreground repair,
/// 2 foreground new data, 3 background), stream priority, round-robin round, stream, queue index.
type SendKey = (u8, u8, u32, u8, usize);

/// Why a reliable fragment is due.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    /// Never sent.
    New,
    /// Its last packet was declared lost.
    Lost,
    /// A probe timeout carries it (no verdict yet).
    Timer,
}

/// Recovery pacing after an outage (ADR-7950, ADR-7995).
#[derive(Debug, Clone)]
struct Recovery {
    /// Packets carrying reliable data one poll may send.
    budget: usize,
    /// The first packet sent at the current budget: its acknowledgment ends the round trip.
    round_start_seq: u64,
    /// First acknowledgment since the outage; pacing is kept for at least a second after it.
    restored_ms: Option<u64>,
    /// Reliable-bearing packets one rolling second may carry.
    rate_cap: usize,
    /// The window before the outage, restored when nothing turns out to be lost.
    prior_window: usize,
    /// Reliable data was declared lost since the outage began.
    lost_data: bool,
    /// Key of the first fragment that had never been sent when the outage began: the backlog ends
    /// before it.
    backlog_end: u64,
}

/// The in-flight window of reliable bytes (ADR-7995).
#[derive(Debug, Clone)]
struct Window {
    /// Reliable bytes that may be in flight.
    bytes: usize,
    /// Reliable bytes acknowledged since the connection began.
    delivered: u64,
    /// First packet number of the current round trip.
    round_end_seq: u64,
    /// Most bytes delivered within one packet's round trip, this round.
    round_best: usize,
    /// The window held due data back during this round.
    round_limited: bool,
    /// `round_best` of the latest consecutive window-limited rounds.
    limited_rounds: VecDeque<usize>,
}

/// What one acknowledgment packet changed.
#[derive(Debug, Default)]
struct AckOutcome {
    /// Packets newly acknowledged.
    newly_acked: usize,
    /// The smallest `delivered_at_send` among newly acknowledged packets that carried reliable
    /// data.
    delivery_base: Option<u64>,
    /// A packet of the current round was acknowledged.
    round_done: bool,
}

/// True when the packet `body` (endpoint header stripped) carries an acknowledgment frame that
/// names packet number `seq` in one of its ranges (ADR-8299 on transport v2, ADR-8184). Reads only
/// the flags, the packet number and the frame, so it decides a path probe before the packet is
/// processed; a truncated or malformed body is `false`.
pub fn packet_acknowledges(body: &[u8], seq: u64) -> bool {
    let mut r = WireReader::new(body);
    let Ok(flags) = r.u8() else {
        return false;
    };
    if flags & FLAG_ACK == 0 || flags & !FLAG_KNOWN != 0 {
        return false;
    }
    let skipped = match flags & FLAG_PN_LEN {
        0 => r.u8().map(|_| ()),
        1 => r.u16().map(|_| ()),
        2 => r.u32().map(|_| ()),
        _ => return false,
    };
    if skipped.is_err() {
        return false;
    }
    AckFrame::decode(&mut r, flags & FLAG_ACK_TIMESTAMPS != 0).is_ok_and(|frame| frame.contains(seq))
}

/// True when `a` is newer than `b` under 16-bit wrapping (RFC 1982).
pub fn seq_greater(a: u16, b: u16) -> bool {
    a != b && a.wrapping_sub(b) < 0x8000
}

/// Reliable and unreliable channels over one flow. See the module docs.
#[derive(Debug)]
pub struct Connection {
    config: LinkConfig,
    /// The next packet number (62-bit, never reused).
    local_seq: u64,
    sent: VecDeque<SentPacket>,
    /// Records examined so far (ADR-10081).
    work: WorkCounters,
    /// The packet body buffer [`Connection::poll_transmit`] reuses (ADR-10081); empty between
    /// polls except for its capacity.
    body_scratch: Vec<u8>,
    /// Where the loss detector starts (ADR-10081): every tracked packet numbered below this is
    /// acknowledged or declared lost. Neither verdict is taken back (a late acknowledgment turns
    /// lost into acknowledged), so the detector never has to look at those records again.
    loss_scan_from: u64,
    /// The largest packet number the peer acknowledged, and when that packet left.
    largest_acked: Option<u64>,
    largest_acked_sent_ms: u64,
    /// Packet numbers received (fresh arrivals only).
    recv: RecvRanges,
    /// Packet numbers received that no acknowledgment frame has named yet.
    unreported: RecvRanges,
    /// Packet numbers this side's acknowledgment frames still name: every fresh arrival, less
    /// what a frame the peer is known to have received named (RFC 9000 § 13.2.4, ADR-8423).
    /// Without it every frame repeats the newest 32 ranges of the whole connection.
    ack_ranges: RecvRanges,
    /// Fresh arrivals counted so far (the index of the next one).
    arrivals: u64,
    /// Arrivals below the largest number received at the time (they can fill a gap inside a
    /// frame already sent): arrival index, packet number. Bounded to [`LATE_ARRIVALS_KEPT`].
    late_arrivals: VecDeque<(u64, u64)>,
    /// Late arrivals with an index below this may have been dropped from `late_arrivals`: a
    /// frame built before it is never used to stop naming numbers.
    late_forgotten: u64,
    /// When the largest packet number received arrived.
    largest_recv_ms: u64,
    /// Something arrived that no acknowledgment frame has named yet.
    ack_pending: bool,
    /// Ack-eliciting packets received since the last acknowledgment frame went out.
    ack_eliciting_unacked: u32,
    /// The latest time the owed acknowledgment may leave (first unacknowledged ack-eliciting
    /// arrival plus [`MAX_ACK_DELAY_MS`]).
    ack_deadline_ms: Option<u64>,
    /// An out-of-order arrival or a second ack-eliciting packet: acknowledge in the next poll.
    ack_urgent: bool,
    /// The peer asked for receive timestamps (it set the flag on its latest packet).
    peer_wants_timestamps: bool,
    /// Arrivals not yet reported in a frame with timestamps: packet number, arrival time.
    pending_timestamps: VecDeque<(u64, u64)>,
    /// Receive timestamps the peer reported for this side's packets.
    receive_timestamps: VecDeque<ReceiveTimestamp>,
    /// Consecutive probe timeouts since the last acknowledgment (RFC 9002 § 6.2.1).
    pto_count: u32,
    /// When the latest ack-eliciting packet left.
    last_ack_eliciting_ms: Option<u64>,
    /// Round-trip samples of the last [`MIN_RTT_WINDOW_MS`], as a monotonic queue of
    /// (time, sample): the front is the window's minimum.
    min_rtt: VecDeque<(u64, f64)>,
    /// Every queued reliable fragment of every stream, in key order.
    reliable_out: VecDeque<OutFragment>,
    /// The key the next queued fragment gets.
    next_key: u64,
    /// Acknowledged fragments not at the front of `reliable_out` (removed in batches).
    acked_interior: usize,
    /// The stream table: the engine's eight, then the game's.
    stream_table: Vec<StreamSpec>,
    send_streams: Vec<SendStream>,
    recv_streams: Vec<RecvStream>,
    /// Sum of every stream's `buffered`.
    buffered_bytes: usize,
    /// Path estimate for background streams: the largest delivery rate of the last
    /// [`RATE_WINDOW_MS`] (bytes per millisecond, monotonic max queue of (time, rate)).
    delivery_rates: VecDeque<(u64, f64)>,
    /// Background streams left start-up (a queue was seen): their window grows by one packet per
    /// round trip from then on.
    background_steady: bool,
    /// When the last packet that parsed arrived.
    last_rx_ms: Option<u64>,
    /// The clock of the latest reassembly stall check: `LinkStats::reassembly_stall_deadline_ms`
    /// reports the deadline as that check enforced it (`stats()` has no clock of its own,
    /// ADR-8859).
    stall_checked_ms: u64,
    /// An authenticated packet that did not parse arrived during the current stall (ADR-8764):
    /// the measured loss no longer stretches the stall deadline until no stream waits.
    unparseable_in_stall: bool,
    unreliable_out: VecDeque<OutUnreliable>,
    /// Essential unreliable data (inputs, tick sync, state acknowledgments): packed first.
    essential_out: VecDeque<OutUnreliable>,
    /// Queued bytes per class: ordinary, essential.
    unreliable_bytes: [usize; 2],
    next_unreliable_id: u64,
    /// Ids of the unreliable messages the latest poll packed.
    packed_unreliable: Vec<u64>,
    unreliable_lifetime_ms: u64,
    /// The first packet number received: inbound loss is measured from it (ADR-8521).
    first_remote_seq: Option<u64>,
    received: VecDeque<(Channel, Vec<u8>)>,
    last_send_ms: Option<u64>,
    srtt: Option<f64>,
    rttvar: f64,
    /// The latest round-trip sample.
    latest_rtt: f64,
    /// A sample far above the estimate arrived then: only a packet sent later is smoothed in.
    rtt_probe_after_ms: Option<u64>,
    /// `rttvar`'s filter seeded with 0 instead of `R / 2`: the reported jitter.
    jitter: f64,
    /// Raw round-trip observations so far and the newest one, for the quality tracker's
    /// percentiles (ADR-8182).
    rtt_observed: (u64, f64),
    loss_window: VecDeque<bool>,
    /// Loss samples pushed so far (the position of the next one).
    loss_pushed: u64,
    rate_window_start: Option<u64>,
    window_sent: u64,
    window_recv: u64,
    /// Reliable-bearing packets acknowledged in the current rate window, and the rate of the two
    /// windows before it (packets per second).
    window_data_acked: u64,
    data_pps: [usize; 2],
    stats: LinkStats,
    far_ahead_streak: u32,
    failure: Option<LinkFailure>,
    /// Bumped by every acknowledgment that names a new packet (resets every fragment's backoff).
    ack_epoch: u64,
    /// When a packet was last newly acknowledged.
    last_ack_ms: Option<u64>,
    /// Reliable bytes sent and neither acknowledged nor declared lost.
    in_flight_bytes: usize,
    window: Window,
    recovery: Option<Recovery>,
    /// Reliable-bearing packets sent in the last second while recovering.
    paced_ms: VecDeque<u64>,
    /// Probe packets sent in the last second.
    timer_ms: VecDeque<u64>,
    /// When the packets with loss repairs of the last half tick left: polls less than half a tick
    /// apart share one repair budget (ADR-10125).
    repair_ms: VecDeque<u64>,
    /// When ordinary unreliable data last left: a spread repair wants the poll before it quiet
    /// (ADR-10125).
    ordinary_sent_ms: Option<u64>,
    /// Packets ahead that mean loss: [`LOSS_PACKET_THRESHOLD`], widened by observed reordering.
    reorder_threshold: u64,
    /// Acknowledgment frames that carried receive timestamps.
    timestamp_reports: u64,
    /// Extra loss delay from observed reordering, in milliseconds.
    reorder_window_ms: u64,
    /// Next interruption probe and its exponential step (spec 120 § 10.2).
    interrupted_probe: Option<(u64, u8)>,
    /// The transport's datagram budget is below the protocol minimum: nothing can be sent.
    transport_stalled: bool,
    transport_stall_logged: bool,
    /// Path MTU discovery (spec 120 § 4.3); inert unless `LinkConfig::mtu_probing`.
    pmtu: crate::pmtud::PathMtu,
    /// Endpoint header length of the current poll (a sent packet's datagram size is its body
    /// plus this).
    header_len: usize,
    /// What this side's ack-eliciting packets cost on the wire for each body byte (ADR-10097).
    wire_share: crate::cc::WireShare,
    /// When the first ack-eliciting packet since the latest newly acknowledged one left, and when
    /// a packet of the peer's last arrived (ADR-10097, [`Self::unanswered_ms`]).
    unanswered_since_ms: Option<u64>,
    heard_ms: Option<u64>,
    /// When background (bulk) data last left, or last had nothing to send: background data
    /// that waits longer than [`BACKGROUND_STARVE_MS`] goes ahead of other unreliable data in
    /// the next poll's first packet (spec 120 § 5.3 anti-starvation, ADR-8407).
    background_progress_ms: u64,
    /// The send-rate controller and its pacers (spec 120 § 4.2).
    rate: crate::cc::RateController,
    pacer: crate::cc::Pacer,
    /// Holds the average to the target (gain 1.0, two ticks deep): the pacer at 1.25x only
    /// spreads, so a saturating sender never offers more than the target.
    budget: crate::cc::Pacer,
    essential_pacer: crate::cc::Pacer,
    /// The excess burst allowance, bytes (ADR-8859; the excess bucket of RFC 2697's single-rate
    /// meter): what the budget bucket could not hold while the sender used less than its target,
    /// up to `PACING_BURST_PACKETS` packets. A foreground reliable message that the pacer let
    /// start draws on it to finish in the same poll when the buckets are spent. A sender that
    /// fills its target never earns any; a shared uplink's share earns none (the aggregate
    /// hands a peer's unused share to the others).
    message_credit: f64,
    round: RateRound,
    /// A shared uplink's fair share for this peer (bytes per second), if lower than the target.
    rate_cap: Option<f64>,
    /// This sender splits a shared uplink among its peers (`set_shared_uplink`): whether or not a
    /// share is in force, its unused target belongs to the aggregate, so it earns no excess
    /// allowance (ADR-10063).
    shared_uplink: bool,
    trendline: crate::cc::TrendlineFilter,
    /// The one-way delay of this side's packets, from the peer's arrival stamps (ADR-10050).
    one_way: crate::cc::OneWayDelay,
    /// Bytes per second of the essential class, smoothed over rounds.
    essential_rate: f64,
    /// Small fragments repaired (resent) within the repair copy window: the fragment's send-queue
    /// key (ids are per stream since ADR-8302), when, and whether a loss verdict caused it. Each takes one redundant copy, as a first transmission does; after
    /// a loss verdict the copy leaves in a packet of its own if no other packet leaves.
    recent_repairs: VecDeque<(u64, u64, bool)>,
}

impl Connection {
    /// A fresh connection. Out-of-range settings are clamped: packets to 256..=2048 bytes (the
    /// receive buffer), at least one packet per poll, the reliable window to
    /// 1..=[`RELIABLE_WINDOW`], a resend timeout of at least 1 ms, the largest reliable message to
    /// [`MAX_RELIABLE_MESSAGE_LIMIT`].
    pub fn new(config: LinkConfig) -> Self {
        let config = LinkConfig {
            max_packet_bytes: config
                .max_packet_bytes
                .clamp(256, crate::socket::MAX_DATAGRAM_BYTES),
            max_packets_per_poll: config.max_packets_per_poll.max(1),
            max_reliable_in_flight: config.max_reliable_in_flight.clamp(1, RELIABLE_WINDOW),
            resend_min_ms: config.resend_min_ms.max(1),
            max_reliable_message_bytes: config.max_reliable_message_bytes.min(MAX_RELIABLE_MESSAGE_LIMIT),
            ..config
        };
        let mut stream_table = ENGINE_STREAMS.to_vec();
        stream_table.extend(
            config
                .streams
                .iter()
                .take(MAX_STREAMS - ENGINE_STREAMS.len())
                .copied(),
        );
        let window = Window {
            bytes: config
                .max_packets_per_poll
                .max(INITIAL_WINDOW_PACKETS)
                .saturating_mul(config.max_packet_bytes),
            delivered: 0,
            round_end_seq: 0,
            round_best: 0,
            round_limited: false,
            limited_rounds: VecDeque::new(),
        };
        // The first burst: the initial window, and no more than a lowered start rate or a first
        // flight allows (ADR-10145, ADR-10156; the default start rate allows all of it).
        let initial_window = config
            .max_packets_per_poll
            .max(INITIAL_WINDOW_PACKETS)
            .saturating_mul(config.max_packet_bytes)
            .min(config.rate.start_burst_bytes(config.max_packet_bytes));
        Self {
            rate: crate::cc::RateController::new(config.rate.clone()),
            pacer: crate::cc::Pacer::new(initial_window),
            budget: crate::cc::Pacer::with_gain(initial_window, 1.0),
            essential_pacer: crate::cc::Pacer::with_gain(
                crate::cc::PACING_BURST_PACKETS.saturating_mul(config.max_packet_bytes),
                1.0,
            ),
            message_credit: 0.0,
            round: RateRound::default(),
            rate_cap: None,
            shared_uplink: false,
            trendline: crate::cc::TrendlineFilter::new(),
            one_way: crate::cc::OneWayDelay::new(),
            essential_rate: 0.0,
            recent_repairs: VecDeque::new(),
            far_ahead_streak: 0,
            failure: None,
            ack_epoch: 0,
            last_ack_ms: None,
            in_flight_bytes: 0,
            window,
            recovery: None,
            paced_ms: VecDeque::new(),
            timer_ms: VecDeque::new(),
            repair_ms: VecDeque::new(),
            ordinary_sent_ms: None,
            reorder_threshold: u64::from(LOSS_PACKET_THRESHOLD),
            timestamp_reports: 0,
            reorder_window_ms: 0,
            interrupted_probe: None,
            transport_stalled: false,
            transport_stall_logged: false,
            pmtu: crate::pmtud::PathMtu::new(config.mtu_probing, config.max_packet_bytes),
            header_len: 0,
            wire_share: crate::cc::WireShare::default(),
            unanswered_since_ms: None,
            heard_ms: None,
            background_progress_ms: 0,
            config,
            local_seq: 0,
            sent: VecDeque::new(),
            work: WorkCounters::default(),
            body_scratch: Vec::new(),
            loss_scan_from: 0,
            largest_acked: None,
            largest_acked_sent_ms: 0,
            recv: RecvRanges::default(),
            unreported: RecvRanges::default(),
            ack_ranges: RecvRanges::default(),
            arrivals: 0,
            late_arrivals: VecDeque::new(),
            late_forgotten: 0,
            largest_recv_ms: 0,
            ack_pending: false,
            ack_eliciting_unacked: 0,
            ack_deadline_ms: None,
            ack_urgent: false,
            peer_wants_timestamps: false,
            pending_timestamps: VecDeque::new(),
            receive_timestamps: VecDeque::new(),
            pto_count: 0,
            last_ack_eliciting_ms: None,
            min_rtt: VecDeque::new(),
            reliable_out: VecDeque::new(),
            next_key: 0,
            acked_interior: 0,
            send_streams: stream_table
                .iter()
                .map(|spec| SendStream {
                    max_data: spec.window_bytes as u64,
                    ..SendStream::default()
                })
                .collect(),
            recv_streams: stream_table
                .iter()
                .map(|spec| RecvStream {
                    advertised: spec.window_bytes as u64,
                    acked_advert: spec.window_bytes as u64,
                    ..RecvStream::default()
                })
                .collect(),
            stream_table,
            buffered_bytes: 0,
            first_remote_seq: None,
            delivery_rates: VecDeque::new(),
            background_steady: false,
            last_rx_ms: None,
            stall_checked_ms: 0,
            unparseable_in_stall: false,
            unreliable_out: VecDeque::new(),
            essential_out: VecDeque::new(),
            unreliable_bytes: [0; 2],
            next_unreliable_id: 0,
            packed_unreliable: Vec::new(),
            unreliable_lifetime_ms: 17,
            received: VecDeque::new(),
            last_send_ms: None,
            srtt: None,
            rttvar: 0.0,
            latest_rtt: 0.0,
            rtt_probe_after_ms: None,
            jitter: 0.0,
            rtt_observed: (0, 0.0),
            loss_window: VecDeque::new(),
            loss_pushed: 0,
            rate_window_start: None,
            window_sent: 0,
            window_recv: 0,
            window_data_acked: 0,
            data_pps: [0; 2],
            stats: LinkStats::default(),
        }
    }

    /// The far end moved to a validated new path (ADR-8299; RFC 9000 § 9.4): the round-trip
    /// estimate starts again from the new path's first sample and the old path's loss window is
    /// forgotten. Sequence numbers, acknowledgments, the reliable stream and any outage-recovery
    /// pacing (ADR-7950: the startup-like ramp the new path should get anyway) carry on. The
    /// endpoints call it when the far end's IP address changed; a port-only change (a NAT
    /// rebinding) keeps the estimate (RFC 9000 § 9.4; ADR-8422).
    pub fn path_changed(&mut self) {
        self.srtt = None;
        self.rttvar = 0.0;
        self.loss_window.clear();
    }

    /// True when packet `body` (endpoint header stripped) carries a packet number above every one
    /// received so far (or is the first): an address-change candidate on a plain link must be the
    /// far end's newest traffic (ADR-8299). The truncated number is decoded against the largest
    /// received, as [`Connection::receive`] does (RFC 9000 § 17.1); a truncated body is `false`.
    pub fn is_newer_packet(&self, body: &[u8]) -> bool {
        let mut r = WireReader::new(body);
        let Ok(flags) = r.u8() else {
            return false;
        };
        if flags & !FLAG_KNOWN != 0 {
            return false;
        }
        let (truncated, pn_len) = match flags & FLAG_PN_LEN {
            0 => (r.u8().map(u64::from), 1),
            1 => (r.u16().map(u64::from), 2),
            2 => (r.u32().map(u64::from), 4),
            _ => return false,
        };
        let Ok(truncated) = truncated else {
            return false;
        };
        let largest = self.recv.largest();
        let seq = crate::ack::decode_packet_number(largest, truncated, pn_len);
        largest.is_none_or(|l| seq > l)
    }

    /// The configuration.
    pub fn config(&self) -> &LinkConfig {
        &self.config
    }

    /// Longest reliable fragment a packet of this configuration can carry (no endpoint header).
    /// A longer one received is a protocol violation ([`LinkFailure::FragmentTooLong`]); both ends
    /// of a session must use the same `max_packet_bytes`.
    pub fn max_fragment_bytes(&self) -> usize {
        self.config
            .max_packet_bytes
            .saturating_sub(PACKET_HEADER_BYTES + RELIABLE_OVERHEAD)
    }

    /// The cap on fragments stored ahead of a gap: `connection_buffer_bytes`, never above one
    /// window of full fragments.
    pub fn reassembly_cap_bytes(&self) -> usize {
        self.config
            .connection_buffer_bytes
            .min(RELIABLE_WINDOW * self.max_fragment_bytes())
    }

    /// How long a reassembly gap may stay unfilled while the peer keeps sending, in milliseconds
    /// of its traffic (spec 120 § 5.2, ADR-8521): `max(4 x RTO, 2 s)` times
    /// [`Connection::reassembly_stall_factor`]. A peer whose packets all arrive gets the base
    /// deadline; on a lossy path an honest sender may lose several repair attempts in a row.
    pub fn reassembly_stall_ms(&self) -> u64 {
        self.reassembly_stall_base_ms()
            .saturating_mul(self.reassembly_stall_attempts())
            / REASSEMBLY_STALL_BASE_ATTEMPTS
    }

    /// The stall deadline with no measured loss: `max(4 x RTO, 2 s)` (spec 120 § 5.2).
    fn reassembly_stall_base_ms(&self) -> u64 {
        (REASSEMBLY_STALL_RTO_MULTIPLE * self.resend_timeout_ms()).max(REASSEMBLY_STALL_MIN_MS)
    }

    /// The base stall deadline as it stands at `now_ms`. Before the first round-trip sample the
    /// RTO is only the configured minimum, but the path's round trip is at least as long as this
    /// end's oldest unanswered ack-eliciting packet has waited: the RTO the deadline is built from
    /// is never below that wait, up to `LinkConfig::resend_max_ms` (ADR-8658). A link whose
    /// acknowledgments cross a deep queue (an honest sender's first window left, the rest waits
    /// for them) is not failed at the 2 s floor; a receiver that has asked the path nothing yet
    /// keeps it.
    fn reassembly_stall_base_at(&self, now_ms: u64) -> u64 {
        let mut rto = self.resend_timeout_ms();
        if self.srtt.is_none()
            && let Some(sent) = self.sent.iter().find(|p| p.ack_eliciting && !p.acked)
        {
            let waited = now_ms.saturating_sub(sent.sent_ms).min(self.config.resend_max_ms);
            rto = rto.max(waited);
        }
        (REASSEMBLY_STALL_RTO_MULTIPLE * rto).max(REASSEMBLY_STALL_MIN_MS)
    }

    /// The stall deadline the check enforces at `now_ms`: the base as it stands then
    /// (`reassembly_stall_base_at`, with the unanswered-packet bound) stretched by the peer's
    /// measured loss, like [`Connection::reassembly_stall_ms`].
    fn reassembly_stall_deadline_at(&self, now_ms: u64) -> u64 {
        self.reassembly_stall_base_at(now_ms)
            .saturating_mul(self.reassembly_stall_attempts())
            / REASSEMBLY_STALL_BASE_ATTEMPTS
    }

    /// How far the peer's measured packet loss stretches the stall deadline, from 1 (no loss) to
    /// 4 (ADR-8521): the repair attempts an honest sender could lose in a row with a chance above
    /// [`REASSEMBLY_STALL_HONEST_MISS`], over the [`REASSEMBLY_STALL_BASE_ATTEMPTS`] the base
    /// deadline covers.
    pub fn reassembly_stall_factor(&self) -> f32 {
        self.reassembly_stall_attempts() as f32 / REASSEMBLY_STALL_BASE_ATTEMPTS as f32
    }

    /// Repair attempts the stall deadline grants: the smallest `n` with `p^n` at most
    /// [`REASSEMBLY_STALL_HONEST_MISS`], `p` the peer's inbound loss, clamped to
    /// `REASSEMBLY_STALL_BASE_ATTEMPTS..=REASSEMBLY_STALL_MAX_ATTEMPTS`. Products only, so the
    /// result is the same on every platform.
    fn reassembly_stall_attempts(&self) -> u64 {
        if self.unparseable_in_stall {
            // The stretch is for an honest sender's lost repairs; a sender whose authenticated
            // packets do not parse is broken or hostile, and it chooses its own packet numbers.
            return REASSEMBLY_STALL_BASE_ATTEMPTS;
        }
        let (holes, span) = self.inbound_holes();
        // One lost packet in the window says nothing about the path.
        let lost = holes.saturating_sub(1);
        if lost == 0 || span == 0 {
            return REASSEMBLY_STALL_BASE_ATTEMPTS;
        }
        let p = lost as f64 / span as f64;
        let mut miss = p;
        let mut attempts = 1;
        while miss > REASSEMBLY_STALL_HONEST_MISS && attempts < REASSEMBLY_STALL_MAX_ATTEMPTS {
            miss *= p;
            attempts += 1;
        }
        attempts.clamp(REASSEMBLY_STALL_BASE_ATTEMPTS, REASSEMBLY_STALL_MAX_ATTEMPTS)
    }

    /// Peer packet numbers that never arrived among the newest [`INBOUND_LOSS_WINDOW`] since the
    /// first one received, and how many numbers that window spans: `(holes, span)`.
    fn inbound_holes(&self) -> (u64, u64) {
        let (Some(largest), Some(first)) = (self.recv.largest(), self.first_remote_seq) else {
            return (0, 0);
        };
        let lo = largest
            .saturating_sub(INBOUND_LOSS_WINDOW - 1)
            .max(first.min(largest));
        let span = largest - lo + 1;
        // Ranges are newest first and disjoint, so the window holds at most 128 of them and every
        // one is remembered (`RecvRanges` keeps 256).
        let got: u64 = self
            .recv
            .newest(INBOUND_LOSS_WINDOW as usize)
            .take_while(|&(_, hi)| hi >= lo)
            .map(|(a, b)| (b + 1).saturating_sub(a.max(lo)))
            .sum();
        (span.saturating_sub(got), span)
    }

    /// Why the connection failed, if it did.
    pub fn failure(&self) -> Option<LinkFailure> {
        self.failure
    }

    /// Bytes of reliable fragments stored ahead of a gap now
    /// ([`LinkStats::reliable_buffered_bytes`], without building the stats).
    pub fn reliable_buffered_bytes(&self) -> usize {
        self.buffered_bytes
    }

    /// Fails the connection and frees what reassembly held.
    fn fail(&mut self, failure: LinkFailure) {
        self.failure = Some(failure);
        for st in &mut self.recv_streams {
            st.stored = HashMap::new();
            st.buffered = 0;
            st.assembling = Vec::new();
        }
        self.buffered_bytes = 0;
    }

    /// The stream table: the engine's eight streams, then the game's (`LinkConfig::streams`).
    pub fn streams(&self) -> &[StreamSpec] {
        &self.stream_table
    }

    /// [`Connection::stream_status`] of every declared stream, in id order.
    pub fn stream_statuses(&self) -> Vec<StreamStatus> {
        (0..self.stream_table.len())
            .filter_map(|i| self.stream_status(StreamId(i as u8)))
            .collect()
    }

    /// What one stream holds now, `None` for a stream the table does not declare.
    pub fn stream_status(&self, stream: StreamId) -> Option<StreamStatus> {
        let i = stream.index();
        let spec = *self.stream_table.get(i)?;
        let send = self.send_streams.get(i)?;
        let recv = self.recv_streams.get(i)?;
        Some(StreamStatus {
            stream,
            spec,
            queued_fragments: usize::from(send.next_id.wrapping_sub(send.floor_id)),
            send_credit_bytes: send.max_data.saturating_sub(send.sent_end),
            buffered_bytes: recv.buffered,
            assembling_bytes: recv.assembling.len(),
            stall_ms: recv.stall_ms,
            class: StreamClass::of(stream),
            msgs_capped: send.msgs_capped,
            msgs_last_second: send.started_ms.len() as u32,
        })
    }

    /// Largest unreliable message that fits one packet behind an endpoint header of
    /// `header_bytes`.
    /// With `LinkConfig::mtu_probing` on this follows the probed path MTU: callers that size
    /// frames (replication, input redundancy) read it again for every frame.
    pub fn max_unreliable_bytes(&self, header_bytes: usize) -> usize {
        self.packet_bytes()
            .saturating_sub(header_bytes + PACKET_HEADER_BYTES + UNRELIABLE_OVERHEAD)
    }

    /// The datagram size data packets use now: the probed path MTU with `mtu_probing` on (the
    /// base while a loss has put it in doubt), else `max_packet_bytes`.
    fn packet_bytes(&self) -> usize {
        if self.pmtu.enabled() {
            self.pmtu.packet_size().min(crate::socket::MAX_DATAGRAM_BYTES)
        } else {
            self.config.max_packet_bytes
        }
    }

    /// The path MTU, every header and the seal included (spec 120 § 4.3): the size the search
    /// has confirmed with `LinkConfig::mtu_probing` on, else `max_packet_bytes`. A lost large
    /// packet puts the size in doubt: until a confirmation probe answers (about a round trip)
    /// data goes at the base size, and the path MTU stays what it was (ADR-10097; it read the
    /// base size for those moments before). Only a black hole lowers it.
    pub fn path_mtu(&self) -> usize {
        if self.pmtu.enabled() {
            self.pmtu.plpmtu().min(crate::socket::MAX_DATAGRAM_BYTES)
        } else {
            self.config.max_packet_bytes
        }
    }

    /// Where path MTU discovery stands ([`crate::pmtud::MtuSearch::Disabled`] when it is off).
    pub fn mtu_search(&self) -> crate::pmtud::MtuSearch {
        self.pmtu.state()
    }

    fn fragment_bytes(&self, header_bytes: usize) -> usize {
        self.config
            .max_packet_bytes
            .saturating_sub(header_bytes + PACKET_HEADER_BYTES + RELIABLE_OVERHEAD)
            .max(1)
    }

    /// The small cut (ADR-10189): two of these fragments, each with its record header, fill the
    /// room of one full fragment.
    fn small_fragment_bytes(&self, header_bytes: usize) -> usize {
        (self
            .fragment_bytes(header_bytes)
            .saturating_sub(RELIABLE_OVERHEAD)
            / SMALL_FRAGMENT_DIVISOR)
            .max(1)
    }

    /// Whether messages queued now are cut small (ADR-10189): the path is not measured yet, or
    /// the path itself is long enough to spread repairs. A measured short path keeps full
    /// fragments.
    ///
    /// The path's own length is its shortest round trip of the last [`MIN_RTT_WINDOW_MS`], not
    /// the round trip now (ADR-10231): a queue this sender built on a short path makes its round
    /// trip long too, and that queue is the rate controller's to drain, not a full queue of
    /// other traffic to squeeze half packets into. An upload over a 256 kbit/s uplink that cut
    /// its messages small behind its own queue kept a stale round trip and target for five
    /// seconds and stood a second 1.3 s queue ten seconds in.
    pub fn cuts_small_fragments(&self) -> bool {
        self.path_round_trip_ms()
            .is_none_or(|rtt| self.repair_packets_at(rtt) < MAX_RESEND_PACKETS_PER_POLL)
    }

    /// The path's own round trip (ADR-10231): the shortest sample of the last
    /// [`MIN_RTT_WINDOW_MS`], never over the round trip now; `None` before the first sample.
    fn path_round_trip_ms(&self) -> Option<f64> {
        let now = self.round_trip_ms()?;
        Some(self.min_rtt.front().map_or(now, |(_, min)| now.min(*min)))
    }

    /// The fragment size a message of `len` bytes is cut to when its stream already holds
    /// `queued` fragments: the small cut for a message of more than one fragment on an unmeasured
    /// or long path while the stream has ids to spare, else the full fragment (ADR-10189). A
    /// message that fits one fragment is never cut: it may share a packet with the tick's
    /// records, which half fragments do not.
    fn cut_bytes(&self, len: usize, queued: usize, header_bytes: usize) -> usize {
        let full = self.fragment_bytes(header_bytes);
        let small = self.small_fragment_bytes(header_bytes);
        if len > full
            // A half that small would take a redundant copy every time it is sent (ADR-8400).
            && small > REDUNDANT_COPY_MAX_BYTES
            && self.cuts_small_fragments()
            && queued + len.div_ceil(small)
                <= self.config.max_reliable_in_flight / SMALL_FRAGMENT_QUEUE_DIVISOR
        {
            small
        } else {
            full
        }
    }

    /// Queues a message. `header_bytes` is the endpoint header the packets will carry (it bounds
    /// the unreliable size and the fragment size).
    ///
    /// Reliable channels queue on their stream ([`Channel::stream`]); a stream the table does not
    /// declare is [`SendError::UnknownStream`]. Each stream admits at most
    /// `LinkConfig::max_reliable_in_flight` fragments not yet acknowledged.
    pub fn send(&mut self, channel: Channel, bytes: Vec<u8>, header_bytes: usize) -> Result<(), SendError> {
        match channel.stream() {
            None => self.queue_unreliable(bytes, header_bytes, false).map(|_| ()),
            Some(stream) => {
                let s = stream.index();
                if s >= self.stream_table.len() {
                    return Err(SendError::UnknownStream(stream.0));
                }
                let max = self.config.max_reliable_message_bytes;
                if bytes.len() > max {
                    return Err(SendError::TooLarge {
                        len: bytes.len(),
                        max,
                    });
                }
                let frag = self.fragment_bytes(header_bytes);
                let count = bytes.len().div_ceil(frag).max(1);
                if count > self.config.max_reliable_in_flight {
                    return Err(SendError::TooLarge {
                        len: bytes.len(),
                        max: frag * self.config.max_reliable_in_flight,
                    });
                }
                let st = &self.send_streams[s];
                let queued = usize::from(st.next_id.wrapping_sub(st.floor_id));
                if queued + count > self.config.max_reliable_in_flight {
                    return Err(SendError::Backpressure(queued));
                }
                // The admission checks above count full fragments, as ever; the cut may be smaller.
                let full = frag;
                let frag = self.cut_bytes(bytes.len(), queued, header_bytes);
                let pushed_from = self.next_key;
                // One fragment (including an empty message) keeps the caller's buffer. A longer
                // message keeps that allocation as the first fragment and copies each tail
                // fragment out once. Repeated `split_off` would copy the remaining tail at every
                // step and leave each head holding the whole tail's capacity until it is acked:
                // quadratic in the fragment count (corr-net, ADR-5580).
                if bytes.len() <= frag {
                    self.push_fragment(s, bytes, true, false);
                } else {
                    let mut head = bytes;
                    let tails: Vec<Vec<u8>> = head
                        .get(frag..)
                        .unwrap_or_default()
                        .chunks(frag)
                        .map(<[u8]>::to_vec)
                        .collect();
                    head.truncate(frag);
                    let last = tails.len();
                    self.push_fragment(s, head, true, true);
                    for (i, tail) in tails.into_iter().enumerate() {
                        self.push_fragment(s, tail, false, i + 1 < last);
                    }
                }
                if frag < full {
                    for f in self
                        .reliable_out
                        .iter_mut()
                        .rev()
                        .take_while(|f| f.key >= pushed_from)
                    {
                        f.small_cut = true;
                    }
                }
                Ok(())
            }
        }
    }

    /// Queues essential unreliable data ahead of reliable traffic (inputs, tick sync or state
    /// acknowledgments). Each priority class holds at most [`MAX_UNRELIABLE_QUEUE_BYTES`].
    /// Unsent records expire one tick after their first poll; no unreliable record is resent.
    pub fn send_essential(&mut self, bytes: Vec<u8>, header_bytes: usize) -> Result<(), SendError> {
        self.queue_unreliable(bytes, header_bytes, true).map(|_| ())
    }

    /// Sets the unreliable queue lifetime to one tick at `tick_rate_hz` (17 ms by default).
    pub fn set_tick_rate(&mut self, tick_rate_hz: u32) {
        self.unreliable_lifetime_ms = 1000u64.div_ceil(u64::from(tick_rate_hz.max(1)));
    }

    pub(crate) fn queue_unreliable(
        &mut self,
        bytes: Vec<u8>,
        header_bytes: usize,
        essential: bool,
    ) -> Result<u64, SendError> {
        let max = self.max_unreliable_bytes(header_bytes);
        if bytes.len() > max {
            return Err(SendError::TooLarge {
                len: bytes.len(),
                max,
            });
        }
        let queue = if essential {
            &mut self.essential_out
        } else {
            &mut self.unreliable_out
        };
        let held = self.unreliable_bytes[usize::from(essential)];
        if held + bytes.len() + UNRELIABLE_OVERHEAD > MAX_UNRELIABLE_QUEUE_BYTES {
            return Err(SendError::Backpressure(queue.len()));
        }
        self.unreliable_bytes[usize::from(essential)] += bytes.len() + UNRELIABLE_OVERHEAD;
        let id = self.next_unreliable_id;
        self.next_unreliable_id = self.next_unreliable_id.wrapping_add(1);
        queue.push_back(OutUnreliable {
            id,
            bytes,
            first_poll_ms: None,
        });
        Ok(id)
    }

    /// Whether the latest poll packed the unreliable message `id`.
    pub(crate) fn was_packed(&self, id: u64) -> bool {
        self.packed_unreliable.contains(&id)
    }

    pub(crate) fn cancel_essential(&mut self, id: u64) {
        self.essential_out.retain(|m| {
            if m.id == id {
                self.unreliable_bytes[1] -= m.bytes.len() + UNRELIABLE_OVERHEAD;
                false
            } else {
                true
            }
        });
    }

    /// Drops unreliable messages that waited a tick since the first poll that saw them.
    fn expire_unreliable(&mut self, now_ms: u64) {
        self.packed_unreliable.clear();
        for (i, queue) in [&mut self.unreliable_out, &mut self.essential_out]
            .into_iter()
            .enumerate()
        {
            let before = queue.len();
            queue.retain_mut(|m| {
                let keep = now_ms.saturating_sub(*m.first_poll_ms.get_or_insert(now_ms))
                    < self.unreliable_lifetime_ms;
                if !keep {
                    self.unreliable_bytes[i] -= m.bytes.len() + UNRELIABLE_OVERHEAD;
                }
                keep
            });
            self.stats.unreliable_dropped += (before - queue.len()) as u64;
        }
    }

    /// How long after a repair its redundant copy may still leave: [`REDUNDANT_COPY_WINDOW_MS`],
    /// or one tick when ticks are longer (a host polls once per tick, ADR-8400).
    fn repair_copy_window_ms(&self) -> u64 {
        REDUNDANT_COPY_WINDOW_MS.max(self.unreliable_lifetime_ms)
    }

    /// A repair made after a loss verdict still owes its redundant copy, and no packet of an
    /// earlier poll carried it: the copy leaves in a packet of its own if nothing else leaves
    /// (ADR-8400). At a long round trip with heavy loss and nothing else to send (a game that
    /// went quiet), each lost repair costs a round trip and a half.
    fn repair_copy_owed(&self, now_ms: u64) -> bool {
        self.recent_repairs.iter().any(|&(key, at, verdict)| {
            verdict
                && at < now_ms
                && self
                    .fragment_index(key)
                    .and_then(|i| self.reliable_out.get(i))
                    .is_some_and(|f| f.in_flight && !f.acked && !f.copied)
        })
    }

    /// Writes as many leading messages of one unreliable queue as fit `body` and returns how
    /// many leading entries the packet takes. A message no packet can carry any more (the
    /// transport budget shrank below it) is part of that count: it must not hide the smaller
    /// messages after it. Nothing leaves the queue until the transport accepted the packet.
    fn plan_unreliable(
        queue: &VecDeque<OutUnreliable>,
        body: &mut WireWriter,
        records: &mut usize,
        budget: usize,
    ) -> usize {
        let mut taken = 0;
        for msg in queue {
            let size = UNRELIABLE_OVERHEAD + msg.bytes.len();
            if size > budget {
                taken += 1;
                continue;
            }
            if *records >= MAX_RECORDS_PER_PACKET || body.len() + size > budget {
                break;
            }
            body.u8(RECORD_UNRELIABLE);
            body.u16(msg.bytes.len() as u16);
            body.raw(&msg.bytes);
            taken += 1;
            *records += 1;
        }
        taken
    }

    /// Removes the `taken` leading messages a transmitted packet carried (or could never carry).
    fn commit_unreliable(&mut self, essential: bool, taken: usize, budget: usize) {
        let class = usize::from(essential);
        for _ in 0..taken {
            let queue = if essential {
                &mut self.essential_out
            } else {
                &mut self.unreliable_out
            };
            let Some(msg) = queue.pop_front() else {
                break;
            };
            let size = UNRELIABLE_OVERHEAD + msg.bytes.len();
            self.unreliable_bytes[class] = self.unreliable_bytes[class].saturating_sub(size);
            if size > budget {
                self.stats.unreliable_dropped += 1;
            } else {
                self.packed_unreliable.push(msg.id);
            }
        }
    }

    fn push_fragment(&mut self, stream: usize, bytes: Vec<u8>, first: bool, more: bool) {
        let st = &mut self.send_streams[stream];
        st.queued_end += bytes.len() as u64;
        let (id, end_offset) = (st.next_id, st.queued_end);
        st.next_id = st.next_id.wrapping_add(1);
        let key = self.next_key;
        self.next_key += 1;
        self.reliable_out.push_back(OutFragment {
            key,
            stream: stream as u8,
            id,
            end_offset,
            bytes,
            first,
            more,
            last_sent_ms: None,
            last_seq: 0,
            acked: false,
            lost: false,
            in_flight: false,
            timeouts: 0,
            backoff_epoch: self.ack_epoch,
            first_sent_ms: None,
            copied: false,
            losses: 0,
            small_cut: false,
        });
    }

    /// The index of the queued fragment `key`.
    fn fragment_index(&self, key: u64) -> Option<usize> {
        self.reliable_out.binary_search_by_key(&key, |f| f.key).ok()
    }

    /// Drops acknowledged fragments (the front at once, the rest in batches) and moves each
    /// stream's floor to its oldest fragment not yet acknowledged.
    fn retire_fragments(&mut self) {
        while self.reliable_out.front().is_some_and(|f| f.acked) {
            self.reliable_out.pop_front();
            self.acked_interior = self.acked_interior.saturating_sub(1);
        }
        if self.acked_interior > 0 && self.acked_interior.saturating_mul(4) >= self.reliable_out.len() {
            self.reliable_out.retain(|f| !f.acked);
            self.acked_interior = 0;
        }
        self.recompute_floors();
    }

    fn recompute_floors(&mut self) {
        for st in &mut self.send_streams {
            st.floor_id = st.next_id;
            st.floor_offset = st.queued_end;
        }
        let mut seen = 0u32;
        let all = if self.send_streams.len() >= 32 {
            u32::MAX
        } else {
            (1u32 << self.send_streams.len()) - 1
        };
        for f in &self.reliable_out {
            if seen == all {
                break;
            }
            let bit = 1u32 << f.stream;
            if f.acked || seen & bit != 0 {
                continue;
            }
            seen |= bit;
            let st = &mut self.send_streams[usize::from(f.stream)];
            st.floor_id = f.id;
            st.floor_offset = f.end_offset - f.bytes.len() as u64;
        }
    }

    /// The in-flight bytes background streams may fill (spec 120 § 5.1-5.3): the path's
    /// bandwidth-delay product (the largest delivery rate of the last [`RATE_WINDOW_MS`] times
    /// the minimum round trip, the estimate of BBR, Cardwell et al. 2016), so the bottleneck
    /// queue a gameplay packet waits behind stays near empty. While no queue has been seen it
    /// grows by a quarter and a packet per round trip, after that by a quarter packet per round
    /// trip; with a queue it holds half a packet under the product until the queue drains (a
    /// delay-based scavenger, the idea of RFC 6817).
    fn background_window(&mut self) -> usize {
        let pkt = self.config.max_packet_bytes;
        let min_rtt = self.min_rtt.front().map(|(_, m)| *m);
        let rate = self.delivery_rates.front().map(|(_, r)| *r);
        let window = match (min_rtt, rate) {
            (Some(min_rtt), Some(rate)) => {
                let bdp = (rate * min_rtt) as usize;
                let queue = self.latest_rtt - min_rtt;
                if queue > BACKGROUND_QUEUE_MS.max(min_rtt / 16.0) {
                    // A queue: drain it.
                    self.background_steady = true;
                    bdp.saturating_sub(pkt / 2)
                } else if self.background_steady {
                    bdp + pkt / 4
                } else {
                    bdp + bdp / 4 + pkt
                }
            }
            _ => RECOVERY_INITIAL_PACKETS * pkt,
        };
        window.clamp(2 * pkt, self.window.bytes.max(2 * pkt))
    }

    /// Adds a delivery-rate sample (bytes per millisecond) to the windowed maximum.
    fn note_delivery_rate(&mut self, now_ms: u64, rate: f64) {
        while self
            .delivery_rates
            .front()
            .is_some_and(|(at, _)| now_ms.saturating_sub(*at) > RATE_WINDOW_MS)
        {
            self.delivery_rates.pop_front();
        }
        while self.delivery_rates.back().is_some_and(|(_, r)| *r <= rate) {
            self.delivery_rates.pop_back();
        }
        self.delivery_rates.push_back((now_ms, rate));
    }

    /// Flow-control credit due now: each stream whose receive window moved a quarter past the
    /// credit last sent (RFC 9000 § 4.2 sends `MAX_STREAM_DATA` as data is consumed), and each
    /// whose newest credit is still unacknowledged a probe timeout after it left (§ 13.3 resends
    /// lost flow-control frames; acknowledgment-only traffic would never reveal the loss).
    fn due_credits(&self, now_ms: u64) -> Vec<(u8, u64)> {
        let retry_after = self.resend_timeout_ms();
        self.recv_streams
            .iter()
            .zip(&self.stream_table)
            .enumerate()
            .filter_map(|(i, (st, spec))| {
                let window = spec.window_bytes as u64;
                let max = st.consumed + window;
                let grow = max >= st.advertised + window / 4;
                let retry = st.advertised > st.acked_advert
                    && now_ms.saturating_sub(st.advert_sent_ms) >= retry_after;
                (grow || retry).then_some((i as u8, max))
            })
            .collect()
    }

    /// The probe timeout before backoff, in milliseconds (RFC 9002 § 6.2.1, spec 120 § 4.1 item 5):
    /// `smoothed_rtt + max(4 * rttvar, 1 ms) + max_ack_delay`, clamped to the configured
    /// `resend_min_ms..=resend_max_ms` and never under 9/8 of the measured round trip; the
    /// configured minimum before the first sample.
    pub fn resend_timeout_ms(&self) -> u64 {
        let Some(srtt) = self.srtt else {
            return self.config.resend_min_ms;
        };
        let pto = (srtt + (4.0 * self.rttvar).max(1.0)).ceil() as u64 + MAX_ACK_DELAY_MS;
        let floor = self
            .round_trip_ms()
            .map_or(0, |rtt| (rtt * 9.0 / 8.0).ceil() as u64);
        pto.clamp(
            self.config.resend_min_ms,
            self.config.resend_max_ms.max(self.config.resend_min_ms),
        )
        .max(floor)
    }

    /// When the probe timeout expires: the probe timeout doubled for each consecutive expiry, up
    /// to `LinkConfig::resend_backoff_max_ms`, after the latest ack-eliciting packet. `None` while
    /// no reliable data waits for an acknowledgment.
    pub fn probe_deadline_ms(&self) -> Option<u64> {
        if !self.reliable_out.iter().any(|f| f.in_flight && !f.acked) {
            return None;
        }
        let base = self.resend_timeout_ms();
        let cap = self.config.resend_backoff_max_ms.max(base);
        let wait = base
            .saturating_mul(1u64 << self.pto_count.min(MAX_BACKOFF_SHIFT))
            .min(cap);
        Some(self.last_ack_eliciting_ms?.saturating_add(wait))
    }

    /// The round trip the path is known to take: the larger of the smoothed and the latest
    /// sample (RFC 9002 § 6.1.2), `None` before the first sample.
    fn round_trip_ms(&self) -> Option<f64> {
        self.srtt.map(|srtt| srtt.max(self.latest_rtt))
    }

    /// How long a packet may stay unacknowledged after a later one was acknowledged before it is
    /// lost: 9/8 of the round trip (the time threshold of RFC 9002 § 6.1.2), at least 1 ms, plus
    /// the reordering this path has shown.
    fn loss_delay_ms(&self, rto: u64) -> u64 {
        self.round_trip_ms()
            .map_or(rto, |rtt| (rtt * 9.0 / 8.0).ceil() as u64)
            .max(1)
            .saturating_add(self.reorder_window_ms)
    }

    /// Silence that means an outage rather than a hitch: the moment a fragment sent when the
    /// silence began times out for the second time in a row.
    fn outage_after_ms(&self, rto: u64) -> u64 {
        let cap = self.config.resend_backoff_max_ms.max(rto);
        rto.saturating_add(rto.max(250).saturating_mul(2).min(cap))
    }

    fn window_min(&self) -> usize {
        RECOVERY_INITIAL_PACKETS
            .min(self.config.max_packets_per_poll)
            .saturating_mul(self.config.max_packet_bytes)
    }

    fn window_max(&self) -> usize {
        RELIABLE_WINDOW.saturating_mul(self.config.max_packet_bytes)
    }

    /// Packets carrying reliable data one poll may send while recovering; `None` when the
    /// connection is not recovering (ADR-7950).
    pub fn recovery_budget(&self) -> Option<usize> {
        self.recovery.as_ref().map(|r| r.budget)
    }

    /// Whether `frag` should leave in a poll at `now_ms`, and why. With `probe` set (the probe
    /// timeout expired) a fragment in flight that has been probed no more than `probed` times
    /// since the last acknowledgment is due as a probe.
    fn fragment_due(&self, frag: &OutFragment, now_ms: u64, probe: Option<u8>) -> Option<Due> {
        if frag.acked {
            return None;
        }
        let Some(sent) = frag.last_sent_ms else {
            return Some(Due::New);
        };
        let waited = now_ms.saturating_sub(sent);
        if frag.lost {
            // At most once per round trip, whatever the peer's acknowledgments claim.
            let spacing = self.round_trip_ms().map_or(0, |rtt| rtt.ceil() as u64);
            return (waited >= spacing + Self::repair_stagger_ms(frag, spacing)).then_some(Due::Lost);
        }
        let probed = if frag.backoff_epoch == self.ack_epoch {
            frag.timeouts
        } else {
            0
        };
        probe
            .is_some_and(|least| frag.in_flight && probed <= least)
            .then_some(Due::Timer)
    }

    /// How much later than one round trip a repair of `frag` leaves (ADR-10088): nothing for the
    /// first repair; from the second loss in a row, a share of the round trip `spacing`, in
    /// [`REPEAT_REPAIR_STAGGER_SLOTS`] steps of `spacing` / [`REPEAT_REPAIR_STAGGER_DIVISOR`],
    /// chosen by the fragment's queue position and its loss count.
    ///
    /// A repair that leaves exactly one round trip after the transmission it replaces arrives at
    /// the bottleneck in the same phase of whatever is periodic there. Behind a drop-tail queue
    /// kept full by a constant-rate flow that phase decides whether a full packet fits (the
    /// traffic phase effect), so a repair lost at one phase is lost again at the same one, every
    /// round trip. The step moves each later attempt to another phase, and neighbors in the queue
    /// (the fragments of one burst, lost together) to different ones. No randomness: the same
    /// connection history gives the same schedule.
    fn repair_stagger_ms(frag: &OutFragment, spacing: u64) -> u64 {
        if frag.losses < 2 {
            return 0;
        }
        let slot = frag
            .key
            .wrapping_add(REPEAT_REPAIR_STAGGER_STRIDE * u64::from(frag.losses))
            % REPEAT_REPAIR_STAGGER_SLOTS;
        spacing.saturating_mul(slot) / REPEAT_REPAIR_STAGGER_DIVISOR
    }

    /// Packets one poll may fill with loss repairs (ADR-7995, ADR-10125):
    /// [`MAX_RESEND_PACKETS_PER_POLL`] over the ticks in a quarter of the round trip, at least one.
    ///
    /// Datagrams that leave one poll reach the bottleneck back to back. A queue that is full takes
    /// the first while it has room and drops the ones behind it, so the repairs of one loss
    /// verdict, sent as one burst, are lost together, and each then waits a round trip for its
    /// next turn. The fate of a repair is not known before a round trip has passed anyway:
    /// spread over a quarter of it, every repair meets the queue at an instant of its own, and
    /// the last one leaves a quarter of a round trip later than it would have. An unmeasured
    /// path and a round trip of a few ticks keep the whole burst.
    pub fn repair_packets_per_poll(&self) -> usize {
        self.round_trip_ms()
            .map_or(MAX_RESEND_PACKETS_PER_POLL, |rtt| self.repair_packets_at(rtt))
    }

    /// [`Self::repair_packets_per_poll`] for a round trip of `rtt` milliseconds.
    fn repair_packets_at(&self, rtt: f64) -> usize {
        let tick = self.unreliable_lifetime_ms.max(1);
        let spread_ticks = (rtt.ceil() as u64) / tick / REPAIR_SPREAD_ROUND_TRIP_DIVISOR;
        (MAX_RESEND_PACKETS_PER_POLL as u64)
            .checked_div(spread_ticks)
            .map_or(MAX_RESEND_PACKETS_PER_POLL, |n| (n as usize).max(1))
    }

    /// The loss repair that would open this poll, if one would: the first foreground candidate,
    /// when it is a repair that the pacer, the repair budget and the window let leave now
    /// (ADR-10125). Returns its record size and why it is due.
    fn lead_repair(
        &self,
        candidates: &[(usize, Due)],
        paced: bool,
        probing: bool,
        resend_room: bool,
    ) -> Option<(usize, Due)> {
        let &(index, due) = candidates.first()?;
        let size = self.reliable_out[index].wire_bytes();
        let leaves = match due {
            Due::New => false,
            Due::Lost => {
                paced
                    && resend_room
                    && !(self.in_flight_bytes > 0 && self.in_flight_bytes + size > self.window.bytes)
            }
            Due::Timer => probing,
        };
        leaves.then_some((size, due))
    }

    /// Whether the poll's first packet leaves its credit and essential records to the second so
    /// that a loss repair leads the poll (ADR-10125). `lead` is [`Self::lead_repair`], `used` the
    /// bytes those records took of the packet's `budget`.
    ///
    /// A full fragment fills a packet. With any record in front of it, it becomes the poll's
    /// second packet, behind one that the essential record and the tick's state fill: at a full
    /// queue the first packet takes the room and the repair behind it is dropped, poll after
    /// poll. The repair is the one packet of the poll its receiver cannot do without (its stall
    /// clock runs), and the records it displaces are sent again every tick; they ride the next
    /// packet of the same poll. Only a repair that would leave in this packet counts: one held by
    /// the pacer, the repair budget or the window displaces nothing.
    fn repair_leads(lead: Option<(usize, Due)>, used: usize, budget: usize) -> bool {
        lead.is_some_and(|(size, _)| size <= budget && used + size > budget)
    }

    /// The longest ordinary unreliable data waits for spread repairs at a time (ADR-10125):
    /// the round trip over [`REPAIR_QUIET_ROUND_TRIP_DIVISOR`].
    fn repair_quiet_ms(&self) -> u64 {
        self.round_trip_ms()
            .map_or(0, |rtt| rtt.ceil() as u64 / REPAIR_QUIET_ROUND_TRIP_DIVISOR)
    }

    /// Reliable candidates of one poll in send order (spec 120 § 5.3, ADR-8407), and where the
    /// background (bulk) part starts. Strict priority by class: the control stream (repairs and
    /// new data), then repairs of the other foreground streams by stream priority, then their new
    /// data by stream priority with weighted round robin by fragment among the streams of one
    /// priority ([`StreamSpec::weight`]); the poll puts other unreliable data next, and the
    /// background streams last (repairs before new data within a priority). Queue order, which
    /// is id order within a stream, breaks every tie.
    fn send_order(&self, now_ms: u64, probe: Option<u8>) -> (Vec<(usize, Due)>, usize) {
        if self.reliable_out.is_empty() {
            return (Vec::new(), 0);
        }
        // At most `MAX_STREAMS` streams are ever declared (`Connection::new` truncates the table).
        let mut new_seen = [0u32; MAX_STREAMS];
        let mut keyed: Vec<(SendKey, Due)> = self
            .reliable_out
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let due = self.fragment_due(f, now_ms, probe)?;
                let s = usize::from(f.stream);
                let spec = &self.stream_table[s];
                let repair = due != Due::New;
                let key = if f.stream == StreamId::CONTROL.0 {
                    (0, 0, 0, 0, i)
                } else if spec.priority >= BACKGROUND_PRIORITY {
                    (3, spec.priority, u32::from(!repair), 0, i)
                } else if repair {
                    (1, spec.priority, 0, 0, i)
                } else {
                    let round = new_seen[s] / u32::from(spec.weight.max(1));
                    new_seen[s] += 1;
                    (2, spec.priority, round, f.stream, i)
                };
                Some((key, due))
            })
            .collect();
        keyed.sort_unstable_by_key(|(key, _)| *key);
        let bulk_start = keyed.partition_point(|((class, ..), _)| *class < 3);
        (
            keyed.into_iter().map(|((.., i), due)| (i, due)).collect(),
            bulk_start,
        )
    }

    /// A packet did not leave (spec 120 § 4.3): nothing of it is committed and its packet number
    /// is not spent. Too large shrinks the packet size; a socket drop is counted, and the poll
    /// ends because the socket is full.
    fn refused(&mut self, refused: crate::socket::TxRefused, header_bytes: usize) {
        match refused {
            crate::socket::TxRefused::TooLarge(max) => self.limit_packet_bytes(max, header_bytes),
            crate::socket::TxRefused::SocketDrop => self.stats.socket_drops += 1,
        }
    }

    /// Starts outage recovery (ADR-7950, ADR-7995): the window collapses and reliable packets are
    /// paced until the path proves itself again.
    fn enter_recovery(&mut self, restored_ms: Option<u64>) {
        if self.recovery.is_some() {
            return;
        }
        let backlog_end = self
            .reliable_out
            .iter()
            .find(|f| f.last_sent_ms.is_none())
            .map_or(self.next_key, |f| f.key);
        let earlier_rate = self.data_pps[0].max(self.data_pps[1]);
        self.recovery = Some(Recovery {
            budget: RECOVERY_INITIAL_PACKETS.min(self.config.max_packets_per_poll),
            round_start_seq: self.local_seq,
            restored_ms,
            rate_cap: RECOVERY_PACKETS_PER_SECOND.max(earlier_rate / 2),
            prior_window: self.window.bytes,
            lost_data: false,
            backlog_end,
        });
        self.window.bytes = self.window_min();
        self.window.limited_rounds.clear();
        // Probes already sent this second count toward the first second's cap.
        self.paced_ms.clone_from(&self.timer_ms);
        self.stats.recoveries += 1;
    }

    /// Ends recovery when the budget has regrown and a second has passed since the path answered,
    /// or as soon as everything sent before the outage is acknowledged. In the second case, with
    /// nothing declared lost, the peer had only stalled: the earlier window comes back.
    fn update_recovery(&mut self, now_ms: u64) {
        let Some(r) = self.recovery.as_ref() else {
            return;
        };
        let regrown = r.budget >= self.config.max_packets_per_poll
            && r.restored_ms.is_some_and(|t| now_ms.saturating_sub(t) >= 1_000);
        let backlog_done = r.restored_ms.is_some()
            && self
                .reliable_out
                .iter()
                .find(|f| !f.acked)
                .is_none_or(|f| f.key >= r.backlog_end);
        if regrown || backlog_done {
            if backlog_done && !r.lost_data {
                self.window.bytes = self.window.bytes.max(r.prior_window);
            }
            self.recovery = None;
        }
    }

    /// The endpoint supplies its authenticated-silence state. Idle probes back off from 250 ms
    /// to 4 s with deterministic bounded jitter; application data can still make progress.
    pub(crate) fn set_interrupted(&mut self, interrupted: bool, now_ms: u64) {
        if interrupted {
            self.interrupted_probe
                .get_or_insert((now_ms.saturating_add(250), 0));
        } else {
            self.interrupted_probe = None;
        }
    }

    /// The low 16 bits of the largest packet number received from the peer (input feedback's
    /// upstream loss, ADR-8077, counts packet numbers modulo 2^16).
    pub(crate) fn remote_seq(&self) -> Option<u16> {
        self.recv.largest().map(|l| l as u16)
    }

    /// True when this side has proof, as of `now_ms`, that the peer hears it: the packet just
    /// received acknowledged something new, or nothing waits for an acknowledgment. An endpoint
    /// that marked the peer interrupted restores it only then, so a peer that still sends but no
    /// longer hears (a one-way outage) is not restored by each of its probes (ADR-7995).
    /// Raw round-trip observations so far and the newest one, milliseconds (ADR-8182).
    pub(crate) fn rtt_samples(&self) -> (u64, f64) {
        self.rtt_observed
    }

    pub(crate) fn peer_hears_us(&self, now_ms: u64) -> bool {
        self.last_ack_ms == Some(now_ms) || self.sent.iter().all(|p| p.acked)
    }

    /// Builds the packets due now. Each starts with `header`. Returns an empty list when there is
    /// nothing to send and no acknowledgement or keepalive is due.
    pub fn poll(&mut self, now_ms: u64, header: &[u8]) -> Vec<Vec<u8>> {
        let mut packets = Vec::new();
        self.poll_transmit(now_ms, header, |packet| {
            packets.push(packet);
            Ok(())
        });
        packets
    }

    /// Restrict future packets to the socket's payload budget. Only never-transmitted tail
    /// fragments may be split and renumbered: a published reliable ID must keep its bytes.
    ///
    /// A budget below the 256 bytes this protocol needs is not latched: the connection stalls
    /// (counted in `LinkStats::transport_stalls`) until the transport reports a usable budget
    /// again, so a transport that reports no budget for a moment does not cut the packet size
    /// for good (ADR-7995).
    pub(crate) fn limit_packet_bytes(&mut self, max: usize, header_bytes: usize) {
        self.transport_stalled = max < 256 || max <= header_bytes + PACKET_HEADER_BYTES + RELIABLE_OVERHEAD;
        if self.transport_stalled {
            return;
        }
        // Probes never exceed what the transport takes (ADR-8300).
        self.pmtu.set_ceiling(max);
        if max >= self.config.max_packet_bytes {
            return;
        }
        self.config.max_packet_bytes = max;
        self.pmtu.set_base(max);
        let fragment_bytes = self.fragment_bytes(header_bytes);
        if !self
            .reliable_out
            .iter()
            .any(|f| f.last_sent_ms.is_none() && f.bytes.len() > fragment_bytes)
        {
            return;
        }
        // A stream sends in id order, so its never-sent fragments are its newest ids: each stream
        // rewinds to its first unsent fragment and queues them again, cut to the new size.
        let (unsent, kept): (Vec<OutFragment>, Vec<OutFragment>) = std::mem::take(&mut self.reliable_out)
            .into_iter()
            .partition(|f| f.last_sent_ms.is_none());
        self.reliable_out = kept.into();
        for f in unsent.iter().rev() {
            let st = &mut self.send_streams[usize::from(f.stream)];
            st.next_id = f.id;
            st.queued_end = f.end_offset - f.bytes.len() as u64;
        }
        for fragment in unsent {
            let s = usize::from(fragment.stream);
            if fragment.bytes.len() <= fragment_bytes {
                self.push_fragment(s, fragment.bytes, fragment.first, fragment.more);
            } else {
                let count = fragment.bytes.len().div_ceil(fragment_bytes);
                for (index, chunk) in fragment.bytes.chunks(fragment_bytes).enumerate() {
                    self.push_fragment(
                        s,
                        chunk.to_vec(),
                        fragment.first && index == 0,
                        index + 1 < count || fragment.more,
                    );
                }
            }
        }
        self.recompute_floors();
    }

    /// Counts a poll that could send nothing because the transport cannot carry this protocol.
    fn note_transport_stall(&mut self) {
        self.stats.transport_stalls += 1;
        if !self.transport_stall_logged {
            self.transport_stall_logged = true;
            tracing::warn!(
                target: "regia_net",
                max_packet_bytes = self.config.max_packet_bytes,
                "the transport's datagram budget is below the 256 bytes the connection needs; \
                 nothing can be sent until it grows"
            );
        }
    }

    /// The acknowledgment frame this side would send now: the newest [`MAX_ACK_RANGES`] ranges
    /// of received packet numbers the peer is not yet known to have heard about, the time the
    /// largest one has waited, and receive timestamps when the peer asked for them. `None` before
    /// the first packet and when every received number has been confirmed (RFC 9000 § 13.2.4,
    /// ADR-8423).
    fn ack_frame(&self, now_ms: u64) -> Option<AckFrame> {
        let largest = self.ack_ranges.largest()?;
        let mut ranges: Vec<(u64, u64)> = self.ack_ranges.newest(MAX_ACK_RANGES).collect();
        Self::clip_first_range(&mut ranges);
        let timestamps = if self.peer_wants_timestamps {
            self.pending_timestamps
                .iter()
                .rev()
                .map(|(packet, at)| AckTimestamp {
                    packet: *packet,
                    after_largest_us: (*at as i64 - self.largest_recv_ms as i64).saturating_mul(1_000),
                })
                .collect()
        } else {
            Vec::new()
        };
        // The delay is known for the newest arrival only. A frame whose largest is an older
        // number (the newer ones were confirmed, a late one filled a gap) reports none and goes
        // flagged as older ranges, so the peer takes no round-trip sample from it.
        // So is the arrival stamp (wire 6, ADR-10050): this side's clock when the newest number
        // arrived, cut to 16 bits; the peer unwraps it and needs no origin.
        let (delay_us, arrival_ms) = if self.frame_is_current() {
            (
                now_ms.saturating_sub(self.largest_recv_ms).saturating_mul(1_000),
                self.largest_recv_ms as u16,
            )
        } else {
            (0, 0)
        };
        Some(AckFrame {
            largest,
            delay_us,
            arrival_ms,
            ranges,
            timestamps,
        })
    }

    /// True when the frame [`Self::ack_frame`] builds names the largest number received, so its
    /// ack delay and the peer's round-trip sample from it are meaningful.
    fn frame_is_current(&self) -> bool {
        self.ack_ranges.largest() == self.recv.largest()
    }

    /// The peer acknowledged a packet whose acknowledgment frame named `named`: it has heard of
    /// every number the frame named, so later frames leave them out (RFC 9000 § 13.2.4). Numbers
    /// that arrived after the frame was built, inside its span, stay named; when the record of
    /// such arrivals is incomplete, nothing is removed (a later confirmation trims instead).
    fn frame_confirmed(&mut self, named: FrameNamed) {
        if named.arrivals < self.late_forgotten {
            return;
        }
        self.ack_ranges.remove(named.lo, named.hi);
        for (index, packet) in &self.late_arrivals {
            if *index >= named.arrivals && (named.lo..=named.hi).contains(packet) {
                self.ack_ranges.insert(*packet);
            }
        }
    }

    /// What the frame [`Self::ack_frame`] would build now names, when `ranges` of it are written.
    fn frame_named(&self, frame: &AckFrame, ranges: usize) -> Option<FrameNamed> {
        let (lo, _) = ranges.checked_sub(1).and_then(|i| frame.ranges.get(i))?;
        Some(FrameNamed {
            lo: *lo,
            hi: frame.largest,
            arrivals: self.arrivals,
        })
    }

    /// Records a fresh arrival for the acknowledgment frames.
    fn note_arrival(&mut self, seq: u64, previous_largest: Option<u64>) {
        let index = self.arrivals;
        self.arrivals += 1;
        self.ack_ranges.insert(seq);
        if previous_largest.is_some_and(|l| seq < l) {
            if self.late_arrivals.len() >= LATE_ARRIVALS_KEPT
                && let Some((dropped, _)) = self.late_arrivals.pop_front()
            {
                self.late_forgotten = dropped + 1;
            }
            self.late_arrivals.push_back((index, seq));
        }
    }

    /// Keeps the first range's length varint within four bytes (acknowledging less is safe).
    fn clip_first_range(ranges: &mut [(u64, u64)]) {
        if let Some(first) = ranges.first_mut() {
            first.0 = first.0.max(first.1.saturating_sub((1 << 30) - 1));
        }
    }

    /// Frames for received ranges older than the newest frame names and not reported yet, oldest
    /// first, at most [`MAX_OLDER_ACK_FRAMES`]: the newest frame only names the newest
    /// [`MAX_ACK_RANGES`] ranges, and a delivered packet nobody names would read as lost.
    fn older_ack_frames(&self) -> Vec<AckFrame> {
        let mut frames = Vec::new();
        let Some(newest_lo) = self.ack_ranges.newest(MAX_ACK_RANGES).last().map(|(lo, _)| lo) else {
            return frames;
        };
        let mut bound = newest_lo;
        while frames.len() < MAX_OLDER_ACK_FRAMES {
            let Some(top) = self.unreported.largest_below(bound) else {
                break;
            };
            let mut ranges: Vec<(u64, u64)> = self.ack_ranges.at_or_below(top, MAX_ACK_RANGES).collect();
            Self::clip_first_range(&mut ranges);
            let Some(lo) = ranges.last().map(|(lo, _)| *lo) else {
                break;
            };
            frames.push(AckFrame {
                largest: top,
                delay_us: 0,
                arrival_ms: 0,
                ranges,
                timestamps: Vec::new(),
            });
            bound = lo;
        }
        frames.reverse();
        frames
    }

    /// True when this side's next poll sends a datagram whatever else is queued (ADR-10200): an
    /// acknowledgment is due, or essential or unreliable data waits. A message that only rides
    /// along (the client's clock stamp) is queued then, so it costs no datagram of its own.
    pub(crate) fn sends_anyway(&self, now_ms: u64) -> bool {
        self.ack_due(now_ms) || !self.essential_out.is_empty() || !self.unreliable_out.is_empty()
    }

    /// True when the acknowledgment this side owes must leave in this poll: a second
    /// ack-eliciting packet, an out-of-order arrival, or [`MAX_ACK_DELAY_MS`] passed since the
    /// first unacknowledged ack-eliciting packet (RFC 9000 § 13.2.1-13.2.2).
    fn ack_due(&self, now_ms: u64) -> bool {
        self.ack_pending
            && !self.ack_ranges.is_empty()
            && (self.ack_urgent || self.ack_deadline_ms.is_some_and(|d| now_ms >= d))
    }

    /// The probe count a probe timeout takes fragments at: the least probed fragment in flight
    /// since the last acknowledgment.
    fn least_probed(&self) -> u8 {
        self.reliable_out
            .iter()
            .filter(|f| f.in_flight && !f.acked)
            .map(|f| {
                if f.backoff_epoch == self.ack_epoch {
                    f.timeouts
                } else {
                    0
                }
            })
            .min()
            .unwrap_or(0)
    }

    /// Commit a packet only after the transport accepted it. A size refusal carries the new
    /// budget and leaves packet number, acknowledgment state, timers and queued messages ready
    /// for the next poll. Encryption record counters are outside this transaction and must never
    /// be rewound.
    pub(crate) fn poll_transmit(
        &mut self,
        now_ms: u64,
        header: &[u8],
        mut transmit: impl FnMut(Vec<u8>) -> Result<(), crate::socket::TxRefused>,
    ) {
        let _span = tracing::trace_span!("regia_net::connection_poll").entered();
        self.header_len = header.len();
        let rto = self.resend_timeout_ms();
        if self.detect_losses(now_ms, rto).is_some()
            && let Some(r) = self.recovery.as_mut()
        {
            r.lost_data = true;
        }
        self.retire();
        self.roll_rate_window(now_ms);
        if self.transport_stalled
            || self.config.max_packet_bytes < 256
            || self.config.max_packet_bytes < header.len() + PACKET_HEADER_BYTES
        {
            self.note_transport_stall();
            return;
        }
        self.expire_mtu_probe(now_ms, rto);
        let budget = self
            .packet_bytes()
            .saturating_sub(header.len() + PACKET_HEADER_BYTES);
        // A full fragment's record: a half fragment (ADR-10189) rides behind other records only
        // where this would have fit (it does with a probed path MTU above the base).
        let full_record = RELIABLE_OVERHEAD + self.fragment_bytes(header.len());
        self.expire_unreliable(now_ms);
        let repair_window = self.repair_copy_window_ms();
        self.recent_repairs
            .retain(|(_, at, _)| now_ms.saturating_sub(*at) <= repair_window);
        self.update_recovery(now_ms);
        if self.recovery.is_none() {
            self.paced_ms.clear();
        }
        // The pacer (spec 120 § 4.2 item 7): 1.25x the target, bursts of one tick (at least two
        // packets) after the first; the essential class has a bucket of its own at the floor.
        let rate_on = self.config.rate.enabled;
        if rate_on {
            let tick = self.unreliable_lifetime_ms.max(1);
            let packet = self.config.max_packet_bytes;
            let rate = f64::from(self.pacing_rate_bytes_per_second().unwrap_or(u32::MAX));
            let depth = self.pacer.depth(rate, tick, 1, packet);
            self.pacer.refill(now_ms, rate, depth);
            let budget_depth = self.budget.depth(rate, tick, 2, packet);
            let unused = self.budget.refill_overflow(now_ms, rate, budget_depth);
            if self.rate_cap.is_some() {
                // A shared uplink's fair share owns the rate: a joiner's first burst (the initial
                // window) would come on top of an aggregate the other peers already fill
                // (ADR-8534), and a share left unused is handed to the other peers, so it earns
                // no excess allowance (ADR-8859).
                self.pacer.limit(depth);
                self.budget.limit(budget_depth);
                self.message_credit = 0.0;
            } else if self.shared_uplink {
                // The peers split one uplink even while no share limits them: what this peer
                // leaves unused is the aggregate's, not an allowance of its own (ADR-10063).
                self.message_credit = 0.0;
            } else {
                // What the budget bucket could not hold is allowance the sender left unused: the
                // excess a message's burst may draw on (ADR-8859).
                let cap = (crate::cc::PACING_BURST_PACKETS * packet) as f64;
                self.message_credit = (self.message_credit + unused).min(cap);
            }
            let floor = f64::from(self.config.rate.floor_bytes_per_second);
            let depth = self.essential_pacer.depth(floor, tick, 1, packet);
            self.essential_pacer.refill(now_ms, floor, depth);
        }
        let mut held = false;
        for recent in [&mut self.paced_ms, &mut self.timer_ms] {
            while recent.front().is_some_and(|t| now_ms.saturating_sub(*t) >= 1_000) {
                recent.pop_front();
            }
        }
        let keepalive_due = self.interrupted_probe.map_or_else(
            || {
                self.last_send_ms
                    .is_none_or(|last| now_ms.saturating_sub(last) >= self.config.keepalive_ms)
            },
            |(due, _)| now_ms >= due,
        );
        // Probe timeout (RFC 9002 § 6.2): one or two packets of the least probed, oldest
        // fragments in flight, never the whole window. Two consecutive expiries with a measured
        // path mean an outage (RFC 9002 § 7.6).
        let mut probe = None;
        let mut probe_packets_left = 0;
        if self
            .probe_deadline_ms()
            .is_some_and(|deadline| now_ms >= deadline)
        {
            self.pto_count = self.pto_count.saturating_add(1);
            self.stats.probe_timeouts += 1;
            probe = Some(self.least_probed());
            let in_flight_packets = self.in_flight_bytes.div_ceil(budget.max(1));
            probe_packets_left = MAX_PROBE_PACKETS.min(in_flight_packets.max(1));
            if self.pto_count >= 2 && self.recovery.is_none() && self.srtt.is_some() {
                self.enter_recovery(None);
            }
        }
        // Reliable data has waited an outage's worth of time (the moment a second consecutive
        // timeout would expire) with nothing acknowledged since it left, on a measured path: the
        // keepalives that probe the path go unanswered too. Pace the backlog when it returns
        // (ADR-7950, ADR-7995; RFC 9002 § 7.6).
        if self.recovery.is_none()
            && self.srtt.is_some()
            && let Some(sent) = self
                .reliable_out
                .iter()
                .filter(|f| f.in_flight && !f.acked)
                .filter_map(|f| f.last_sent_ms)
                .min()
            && self.last_ack_ms.is_none_or(|t| t <= sent)
            && now_ms.saturating_sub(sent) >= self.outage_after_ms(rto)
        {
            self.enter_recovery(None);
        }
        let ack_due = self.ack_due(now_ms);
        if ack_due {
            for frame in self.older_ack_frames() {
                let pn_len = crate::ack::packet_number_len(self.local_seq, self.largest_acked);
                let mut out = WireWriter::with_capacity(header.len() + 1 + pn_len + 64);
                out.raw(header);
                out.u8(match pn_len {
                    1 => 0,
                    2 => 1,
                    _ => 2,
                } | FLAG_ACK
                    | FLAG_ACK_OLDER);
                crate::ack::write_packet_number(&mut out, self.local_seq, pn_len);
                let (frame_bytes, named) = frame.encode_counted(&mut out, false, budget);
                let body_len = out.len() - header.len();
                if let Err(refused) = transmit(out.finish()) {
                    self.refused(refused, header.len());
                    return;
                }
                if let Some((lo, _)) = named.checked_sub(1).and_then(|i| frame.ranges.get(i)) {
                    self.unreported.remove(*lo, frame.largest);
                }
                let seq = self.local_seq;
                let span = self.frame_named(&frame, named);
                self.commit_packet(now_ms, body_len, Vec::new(), false, false, false);
                self.note_frame_sent(seq, span, frame_bytes);
            }
        }
        let mut packet_count = 0;
        let (order, bulk_start) = self.send_order(now_ms, probe);
        let mut reliable_cursor = 0;
        let mut bulk_cursor = bulk_start;
        // The message caps (spec 120 § 4.5 A2): the last second's starts per stream.
        for st in &mut self.send_streams {
            while st
                .started_ms
                .front()
                .is_some_and(|t| now_ms.saturating_sub(*t) >= 1_000)
            {
                st.started_ms.pop_front();
            }
        }
        // Per-stream scratch lives on the stack (ADR-10081): at most `MAX_STREAMS` streams.
        let mut cap_held = [false; MAX_STREAMS];
        if bulk_start == order.len() {
            self.background_progress_ms = now_ms;
        }
        let background_starved = bulk_start < order.len()
            && now_ms.saturating_sub(self.background_progress_ms) >= BACKGROUND_STARVE_MS;
        // Flow control (RFC 9000 § 4): per stream the peer's credit, per connection the bytes
        // the peer may have to store ahead of a gap (`reassembly_cap_bytes` on its side).
        let mut planned_end = [0u64; MAX_STREAMS];
        let mut ahead_total = 0u64;
        for (end, st) in planned_end.iter_mut().zip(&self.send_streams) {
            *end = st.sent_end.max(st.floor_offset);
            ahead_total += *end - st.floor_offset;
        }
        let ahead_cap = self.reassembly_cap_bytes() as u64;
        let background_window = self.background_window();
        let (mut flow_blocked, mut background_held) = (false, false);
        let mut credits = self.due_credits(now_ms);
        // Pacing budgets of this poll, in packets (ADR-7950, ADR-7995).
        let paced = self.paced_ms.len();
        let mut reliable_packets_left = self
            .recovery
            .as_ref()
            .map_or(usize::MAX, |r| r.budget.min(r.rate_cap.saturating_sub(paced)));
        // Repairs are spread over a quarter of the round trip, and polls less than half a tick
        // apart share one budget: the second would reach the bottleneck on the heels of the
        // first (ADR-10125).
        let repair_budget = self.repair_packets_per_poll();
        let tick = self.unreliable_lifetime_ms.max(1);
        while self
            .repair_ms
            .front()
            .is_some_and(|t| now_ms.saturating_sub(*t) >= (tick / 2).max(1))
        {
            self.repair_ms.pop_front();
        }
        let mut resend_packets_left = repair_budget.saturating_sub(self.repair_ms.len());
        // One repair packet per poll (a long round trip, a standing queue): the repair wants the
        // room at the bottleneck that the connection's own state packets would take a tick
        // earlier. While such repairs are due, ordinary unreliable data waits; a repair whose
        // previous tick carried state waits one poll, which is then the quiet one.
        let (mut quiet, mut hold_repairs) = (false, false);
        let mut window_closed = false;
        // The foreground message that started in this poll and is not finished: its stream, the
        // id of its next fragment and the packets it may still take (ADR-8859).
        let mut message_burst: Option<(u8, u16, usize)> = None;
        // One body buffer for every packet of every poll (ADR-10081): its bytes are copied into the
        // packet that leaves, so the buffer itself never does.
        let mut body = WireWriter::reusing(std::mem::take(&mut self.body_scratch));
        body.reserve(budget);
        while packet_count < self.config.max_packets_per_poll {
            body.clear();
            let mut plan: Vec<(usize, Due)> = Vec::new();
            let mut records = 0;
            let mut planned_bytes = 0usize;
            let (mut has_resend, mut has_probe) = (false, false);
            // The pacer decides for this packet; the essential class may also use its floor.
            let paced = !rate_on || (self.pacer.ready() && self.budget.ready());
            let essential_ok = paced || self.essential_pacer.ready();
            // A message's burst is the message's (ADR-8859): the pacer let a foreground reliable
            // message start in this poll, the buckets are spent, and the sender has left allowance
            // unused. The rest of that message leaves now, up to `PACING_BURST_PACKETS` packets
            // from its start, charged to the buckets (they go into debt, so the rate over time is
            // the target's) and to the excess allowance. Before, the allowance was whatever the
            // bucket had left, and a client's own input bundles had drained it: the rest of a
            // 4 KiB command trickled out at 1.25x the target.
            let finishing = message_burst
                .filter(|_| !paced && self.message_credit > 0.0)
                .map(|(stream, id, _)| (stream, id));
            // The fragment the burst takes next (they leave in id order).
            let mut burst_next = finishing;
            // Held: the target let no more through and data waits (or leaves on the floor alone).
            // The essential class counts: a sender whose inputs outrun its target is not
            // app-limited, whatever the floor then lets out (ADR-8592).
            if !paced
                && !held
                && (!self.unreliable_out.is_empty()
                    || !self.essential_out.is_empty()
                    || self
                        .reliable_out
                        .iter()
                        .any(|f| f.last_sent_ms.is_none() || f.lost))
            {
                held = true;
            }
            // Flow-control credit first: it is small and it unblocks the peer's streams.
            let mut packet_credits = std::mem::take(&mut credits);
            for (stream, max) in &packet_credits {
                body.u8(RECORD_CREDIT);
                body.u8(*stream);
                crate::ack::write_varint(&mut body, *max);
                records += 1;
            }
            let mut credit_len = body.len();
            // Essential unreliable data next: a reliable backlog never silences inputs, tick sync
            // or state acknowledgments (spec 120 § 5.3).
            let mut essential_taken = if essential_ok {
                Self::plan_unreliable(&self.essential_out, &mut body, &mut records, budget)
            } else {
                0
            };
            let probing = probe.filter(|_| probe_packets_left > 0);
            // Except in front of a loss repair that these records would push into the poll's
            // second packet: the repair leads the poll and they ride the next packet (ADR-10125).
            let candidates = &order[reliable_cursor..bulk_start];
            let lead = if packet_count == 0 {
                self.lead_repair(candidates, paced, probing.is_some(), resend_packets_left > 0)
            } else {
                None
            };
            // A spread repair is due, whether or not a poll moments ago used this tick's budget:
            // the state waits in every poll of the tick.
            let spread_due = packet_count == 0
                && repair_budget == 1
                && self
                    .lead_repair(candidates, paced, false, true)
                    .is_some_and(|(_, due)| due == Due::Lost)
                // A half fragment's repair leaves alone in half a packet, which finds room with
                // the state in the packet behind it: nothing waits for it (ADR-10189). Unless
                // the pacing allowance does not carry it and that packet: then it cannot lead,
                // the state would take the allowance tick after tick, and the repair needs the
                // wait to get its turn at all.
                && !candidates.first().is_some_and(|&(index, _)| {
                    let frag = &self.reliable_out[index];
                    let packet = header.len() + PACKET_HEADER_BYTES + frag.wire_bytes();
                    frag.small_cut
                        && (!rate_on || (self.pacer.covers(packet + 1) && self.budget.covers(packet + 1)))
                });
            if spread_due {
                let since_state = self
                    .ordinary_sent_ms
                    .map_or(u64::MAX, |t| now_ms.saturating_sub(t));
                if since_state < tick + tick / 2 {
                    // State left a tick ago: this poll is the quiet one, the repair leads the next.
                    (quiet, hold_repairs) = (true, true);
                } else {
                    // The state has waited its share: it rides behind the repair this once.
                    quiet = since_state == u64::MAX || since_state < self.repair_quiet_ms().max(2 * tick);
                }
            }
            // (A poll of one packet keeps it for the essential records, spec 120 § 5.3. So does a
            // poll whose pacing allowance the repair would use up: on a link held to about a
            // packet per tick, a shared uplink's fair share, the tick's state keeps the first
            // place and the repair the room behind it, as before.)
            // A half fragment (ADR-10189) takes a full fragment's place in a poll: it shares a
            // packet with other half fragments only, so a repair of one leads whenever anything
            // is in front of it, and outside a spread it takes its pair along.
            let lead_small = lead.is_some()
                && candidates
                    .first()
                    .is_some_and(|&(index, _)| self.reliable_out[index].small_cut);
            let lead_paid = !rate_on
                || lead.is_some_and(|(size, _)| {
                    let size = if lead_small && repair_budget > 1 {
                        2 * size
                    } else {
                        size
                    };
                    let packet = header.len() + PACKET_HEADER_BYTES + size;
                    self.pacer.covers(packet + 1) && self.budget.covers(packet + 1)
                });
            // (Nor does a repair lead a poll whose recovery budget lets no reliable packet leave:
            // the records it displaced would have no packet to ride.)
            let leads = !hold_repairs
                && lead_paid
                && self.config.max_packets_per_poll >= 2
                && reliable_packets_left > 0;
            if leads
                && !body.is_empty()
                && ((lead_small && body.len() + full_record > budget)
                    || Self::repair_leads(lead, body.len(), budget))
            {
                body = WireWriter::with_capacity(budget);
                records = 0;
                credits = std::mem::take(&mut packet_credits);
                credit_len = 0;
                essential_taken = 0;
            }
            let essential_len = body.len() - credit_len;
            // A spread repair's packet stays small (ADR-10189): behind a queue that counts bytes
            // and is kept full, a packet of half the size finds room far more often than a full
            // one, and a repair lost there waits a whole round trip for its next turn. The half
            // fragment that leads such a poll leaves alone: its pair, the tick's records and new
            // data ride the packets behind it.
            let small_repair = leads && repair_budget == 1 && lead.is_some_and(|(_, due)| due == Due::Lost);
            let small_room = budget / SMALL_FRAGMENT_DIVISOR;
            let mut small_planned = false;
            // The length of the body up to the end of its run of half fragments.
            let mut halves_len = 0;
            // Message starts this packet plans per stream (the cap counts them before they leave).
            let mut packet_starts = [0u32; MAX_STREAMS];
            let mut ordinary_taken = 0;
            let mut copies: Vec<usize> = Vec::new();
            // Strict priority (spec 120 § 5.3): the main phase walks the control stream, then
            // repairs, then new data by priority; then "other unreliable" and redundant copies;
            // then the bulk phase walks background streams.
            for bulk_phase in [false, true] {
                if bulk_phase {
                    // Redundant copies of small reliable fragments first sent moments ago (newest
                    // first, stopping at the first one past the window: first transmissions go out in
                    // id order).
                    if !small_planned
                        && (!plan.is_empty()
                            || essential_taken > 0
                            || (paced && !self.unreliable_out.is_empty())
                            || self.repair_copy_owed(now_ms))
                    {
                        // Repairs first: a fragment already lost once is on a lossy path, and a
                        // repair lost again costs a whole round trip more (ADR-8400).
                        for &(key, at, _) in &self.recent_repairs {
                            if copies.len() >= MAX_COPIES_PER_PACKET || records >= MAX_RECORDS_PER_PACKET {
                                break;
                            }
                            let Some(index) = self.fragment_index(key) else {
                                continue;
                            };
                            let Some(frag) = self.reliable_out.get(index) else {
                                continue;
                            };
                            // Not in the same poll: back-to-back datagrams are lost together.
                            if at == now_ms
                                || frag.copied
                                || !frag.in_flight
                                || frag.acked
                                || body.len() + frag.wire_bytes() > budget
                                || plan.iter().any(|(i, _)| *i == index)
                            {
                                continue;
                            }
                            write_fragment(&mut body, frag);
                            records += 1;
                            copies.push(index);
                        }
                        for (index, frag) in self.reliable_out.iter().enumerate().rev() {
                            let Some(first) = frag.first_sent_ms else {
                                continue;
                            };
                            if now_ms.saturating_sub(first) > REDUNDANT_COPY_WINDOW_MS
                                || copies.len() >= MAX_COPIES_PER_PACKET
                                || records >= MAX_RECORDS_PER_PACKET
                            {
                                break;
                            }
                            // Not in the same poll: back-to-back datagrams are lost together.
                            if first == now_ms
                                || frag.copied
                                || !frag.in_flight
                                || frag.acked
                                || frag.bytes.len() > REDUNDANT_COPY_MAX_BYTES
                                || body.len() + frag.wire_bytes() > budget
                                || plan.iter().any(|(i, _)| *i == index)
                                || copies.contains(&index)
                            {
                                continue;
                            }
                            write_fragment(&mut body, frag);
                            records += 1;
                            copies.push(index);
                        }
                    }
                    // Other unreliable (state frames, voice) before bulk, unless bulk has starved:
                    // then the poll's first packet carries bulk first.
                    if paced && !quiet && !small_planned && !(background_starved && packet_count == 0) {
                        ordinary_taken =
                            Self::plan_unreliable(&self.unreliable_out, &mut body, &mut records, budget);
                    }
                }
                let (cursor, end) = if bulk_phase {
                    (&mut bulk_cursor, order.len())
                } else {
                    (&mut reliable_cursor, bulk_start)
                };
                while reliable_packets_left > 0
                    && *cursor < end
                    && records < MAX_RECORDS_PER_PACKET
                    && (paced || probing.is_some() || (finishing.is_some() && !bulk_phase))
                {
                    let (index, due) = order[*cursor];
                    let frag = &self.reliable_out[index];
                    if due == Due::Timer && probing.is_none() {
                        *cursor += 1;
                        continue;
                    }
                    let s = usize::from(frag.stream);
                    let mut grow = 0;
                    if due == Due::New {
                        // The class's message cap (spec 120 § 4.5 A2): a message that would pass it
                        // waits, and so does the rest of its stream's new data this poll.
                        let cap = if self.config.message_caps {
                            self.stream_table[s].msgs_per_s
                        } else {
                            0
                        };
                        if cap_held[s]
                            || (cap > 0
                                && frag.first
                                && self.send_streams[s].started_ms.len() as u64 + u64::from(packet_starts[s])
                                    >= u64::from(cap))
                        {
                            if !cap_held[s] {
                                cap_held[s] = true;
                                self.send_streams[s].msgs_capped += 1;
                            }
                            *cursor += 1;
                            continue;
                        }
                        let st = &self.send_streams[s];
                        // Reframing unsent work can grow a stream's queue; never publish ids outside
                        // the peer's receive window, even past the admission cap.
                        if usize::from(frag.id.wrapping_sub(st.floor_id)) >= RELIABLE_WINDOW {
                            *cursor += 1;
                            continue;
                        }
                        grow = frag.end_offset.saturating_sub(planned_end[s]);
                        if frag.end_offset > st.max_data || ahead_total + grow > ahead_cap {
                            flow_blocked = true;
                            *cursor += 1;
                            continue;
                        }
                        if self.stream_table[s].priority >= BACKGROUND_PRIORITY
                            && self.in_flight_bytes + planned_bytes + frag.wire_bytes() > background_window
                        {
                            background_held = true;
                            *cursor += 1;
                            continue;
                        }
                    }
                    let resend_room = has_resend || resend_packets_left > 0;
                    // A burst packet conforms only if the excess allowance holds it whole (RFC
                    // 2697 § 3: a packet of B bytes needs Te >= B), counted as it is charged: a
                    // few bytes of credit no longer let a whole packet out (ADR-10063).
                    let burst_covered = || {
                        let packet = PACKET_HEADER_BYTES + body.len() + frag.wire_bytes();
                        self.message_credit >= packet as f64
                    };
                    // New data and loss repairs wait for the pacer; a probe does not (RFC 9002 § 7.7).
                    let allowed = match due {
                        // A message's first fragment always waits for the pacer.
                        Due::New => {
                            !window_closed
                                && (paced
                                    || (burst_next == Some((frag.stream, frag.id))
                                        && !frag.first
                                        && burst_covered()))
                        }
                        Due::Lost => !window_closed && resend_room && paced && !hold_repairs,
                        Due::Timer => resend_room,
                    };
                    if !allowed {
                        *cursor += 1;
                        continue;
                    }
                    let size = frag.wire_bytes();
                    // Background data never borrows from the pacing buckets (ADR-8658): it leaves
                    // only when both hold its whole packet. A bulk packet that put them in debt held
                    // the next poll's foreground data (a command's acknowledgment) for the refill,
                    // about one packet's time at the target: 32 ms p95 on a 256 kbit/s downlink.
                    // A starving background stream (`BACKGROUND_STARVE_MS`) still borrows for the
                    // poll's first packet.
                    if rate_on
                        && due != Due::Timer
                        && self.stream_table[s].priority >= BACKGROUND_PRIORITY
                        && !(background_starved && packet_count == 0)
                    {
                        let packet = header.len() + PACKET_HEADER_BYTES + body.len() + size;
                        if !(self.pacer.covers(packet) && self.budget.covers(packet)) {
                            // The pacer holds data back: the sender is not app-limited.
                            held = true;
                            *cursor += 1;
                            continue;
                        }
                    }
                    // A probe is not held by the window (RFC 9002 § 6.2.4); new data and loss
                    // repairs are, except that an empty path always takes one.
                    if due != Due::Timer {
                        let in_flight = self.in_flight_bytes + planned_bytes;
                        if in_flight > 0 && in_flight + size > self.window.bytes {
                            window_closed = true;
                            // Data waits on the in-flight window: the sender is not app-limited.
                            held = true;
                            self.window.round_limited = true;
                            *cursor += 1;
                            continue;
                        }
                    }
                    if small_planned && body.len() + size > small_room {
                        break;
                    }
                    // A half fragment shares a packet with half fragments only: in front of it,
                    // the tick's records end the packet where they would in front of a full
                    // fragment, and the state keeps the place it had (ADR-10189).
                    let halves_run =
                        body.is_empty() || body.len() == halves_len || body.len() + full_record <= budget;
                    if frag.small_cut && !halves_run {
                        break;
                    }
                    if body.len() + size > budget {
                        if body.is_empty() {
                            *cursor += 1;
                            continue;
                        }
                        break;
                    }
                    write_fragment(&mut body, frag);
                    if due == Due::New && burst_next == Some((frag.stream, frag.id)) {
                        burst_next = Some((frag.stream, frag.id.wrapping_add(1)));
                    }
                    if frag.small_cut && halves_run {
                        halves_len = body.len();
                    }
                    small_planned |= small_repair && frag.small_cut && due == Due::Lost;
                    if !frag.in_flight {
                        planned_bytes += size;
                    }
                    if due == Due::New {
                        ahead_total += grow;
                        planned_end[s] = planned_end[s].max(frag.end_offset);
                    }
                    if due == Due::New && frag.first {
                        packet_starts[s] += 1;
                    }
                    has_resend |= due != Due::New;
                    has_probe |= due == Due::Timer;
                    plan.push((index, due));
                    *cursor += 1;
                    records += 1;
                }
            }
            if paced && !quiet && !small_planned && background_starved && packet_count == 0 {
                ordinary_taken = Self::plan_unreliable(&self.unreliable_out, &mut body, &mut records, budget);
            }
            let mut ping = false;
            if body.is_empty() {
                // Nothing to carry. A packet still leaves for a keepalive or a probe with no data
                // (an ack-eliciting ping), or for an acknowledgment that is due and that no packet
                // of this poll has carried yet.
                // An acknowledgment that is due anyway keeps the path alive as well as a ping
                // would; adding one would only make the peer acknowledge it back.
                let ack_now = ack_due && self.ack_pending;
                let wants_ping = packet_count == 0 && ((keepalive_due && !ack_now) || probing.is_some());
                if wants_ping {
                    ping = true;
                    body.u8(RECORD_PING);
                } else if !(ack_due && self.ack_pending) {
                    break;
                }
            }
            let ack_eliciting = !body.is_empty();
            let pn_len = crate::ack::packet_number_len(self.local_seq, self.largest_acked);
            // Allocated only when an acknowledgment frame is written.
            let mut frame = WireWriter::new();
            let mut carried_timestamps = false;
            // The oldest number the frame names (a tight room may trim its oldest ranges).
            let mut named_from = None;
            let mut span = None;
            let mut frame_older = false;
            if let Some(ack) = self.ack_frame(now_ms) {
                let room = self
                    .packet_bytes()
                    .saturating_sub(header.len() + 1 + pn_len + body.len());
                if ack.min_encoded_len() <= room {
                    frame.reserve(64);
                    frame_older = !self.frame_is_current();
                    // Receive timestamps are relative to the newest arrival: a frame whose largest
                    // is an older number leaves them for the next current frame (ADR-8423).
                    carried_timestamps = self.peer_wants_timestamps && !frame_older;
                    let (_, named) = ack.encode_counted(&mut frame, carried_timestamps, room);
                    named_from = named
                        .checked_sub(1)
                        .and_then(|i| ack.ranges.get(i))
                        .map(|(lo, _)| *lo);
                    span = self.frame_named(&ack, named);
                }
            }
            if frame.is_empty() && !ack_eliciting {
                // An acknowledgment-only packet that cannot hold its acknowledgment.
                break;
            }
            let mut flags = match pn_len {
                1 => 0,
                2 => 1,
                _ => 2,
            };
            if !frame.is_empty() {
                flags |= FLAG_ACK;
            }
            if frame_older {
                flags |= FLAG_ACK_OLDER;
            }
            if carried_timestamps {
                flags |= FLAG_ACK_TIMESTAMPS;
            }
            if self.config.receive_timestamps {
                flags |= FLAG_WANT_TIMESTAMPS;
            }
            let mut out = WireWriter::with_capacity(header.len() + 1 + pn_len + frame.len() + body.len());
            out.raw(header);
            out.u8(flags);
            crate::ack::write_packet_number(&mut out, self.local_seq, pn_len);
            out.raw(frame.as_slice());
            out.raw(body.as_slice());
            let body_len = out.len() - header.len();
            if let Err(refused) = transmit(out.finish()) {
                self.body_scratch = body.finish();
                self.refused(refused, header.len());
                return;
            }
            self.commit_unreliable(true, essential_taken, budget);
            self.commit_unreliable(false, ordinary_taken, budget);
            // The newest new fragment of a foreground stream in this packet, and whether its
            // message started here.
            let foreground = plan.iter().rev().find_map(|&(index, due)| {
                let frag = &self.reliable_out[index];
                (due == Due::New
                    && self.stream_table[usize::from(frag.stream)].priority < BACKGROUND_PRIORITY)
                    .then_some((frag.stream, frag.id))
            });
            let bursting = finishing.is_some() && foreground.is_some();
            message_burst = foreground.and_then(|(stream, id)| {
                let started = plan.iter().any(|&(index, due)| {
                    let frag = &self.reliable_out[index];
                    due == Due::New && frag.stream == stream && frag.first
                });
                let left = if started {
                    crate::cc::PACING_BURST_PACKETS - 1
                } else {
                    message_burst
                        .filter(|(open, _, _)| *open == stream)
                        .map_or(0, |(_, _, left)| left.saturating_sub(1))
                };
                // The next fragment of the stream continues this message and has not left yet.
                let next = id.wrapping_add(1);
                let continues = self
                    .reliable_out
                    .iter()
                    .any(|f| f.stream == stream && f.id == next && !f.first && f.last_sent_ms.is_none());
                (left > 0 && continues).then_some((stream, next, left))
            });
            if ordinary_taken > 0 {
                self.ordinary_sent_ms = Some(now_ms);
            }
            if rate_on {
                if paced || bursting {
                    self.pacer.spend(body_len);
                    self.budget.spend(body_len);
                    if bursting {
                        self.message_credit -= body_len as f64;
                    }
                } else if ack_eliciting {
                    self.essential_pacer.spend(body_len);
                }
            }
            self.round.essential_bytes += essential_len as u64;
            let seq = self.local_seq;
            let epoch = self.ack_epoch;
            let mut ids = Vec::with_capacity(plan.len());
            for (index, due) in plan {
                let frag = &mut self.reliable_out[index];
                if self.stream_table[usize::from(frag.stream)].priority >= BACKGROUND_PRIORITY {
                    self.background_progress_ms = now_ms;
                }
                if due != Due::New {
                    self.stats.reliable_resends += 1;
                    if frag.bytes.len() <= REDUNDANT_COPY_MAX_BYTES {
                        // The repair takes a redundant copy of its own (ADR-8400).
                        frag.copied = false;
                        self.recent_repairs
                            .push_back((frag.key, now_ms, due == Due::Lost));
                    }
                }
                if due == Due::Timer {
                    // Probes of this fragment since the last acknowledgment, from zero after one.
                    if frag.backoff_epoch == epoch {
                        frag.timeouts = frag.timeouts.saturating_add(1);
                    } else {
                        frag.timeouts = 1;
                        frag.backoff_epoch = epoch;
                    }
                }
                if frag.first && frag.first_sent_ms.is_none() {
                    self.send_streams[usize::from(frag.stream)]
                        .started_ms
                        .push_back(now_ms);
                }
                frag.last_sent_ms = Some(now_ms);
                frag.first_sent_ms.get_or_insert(now_ms);
                frag.last_seq = seq;
                frag.lost = false;
                if !frag.in_flight {
                    frag.in_flight = true;
                    self.in_flight_bytes += frag.wire_bytes();
                }
                let st = &mut self.send_streams[usize::from(frag.stream)];
                st.sent_end = st.sent_end.max(frag.end_offset);
                ids.push(frag.key);
            }
            // Copies ride packets that leave anyway: they spend no pacing budget.
            let planned = !ids.is_empty();
            for index in copies {
                // The copy is now the latest transmission: a verdict on the first one alone no
                // longer requeues it, and an acknowledgment of either packet delivers it.
                let frag = &mut self.reliable_out[index];
                frag.copied = true;
                frag.last_sent_ms = Some(now_ms);
                frag.last_seq = seq;
                ids.push(frag.key);
                self.stats.redundant_copies += 1;
            }
            let data = !ids.is_empty();
            if planned {
                reliable_packets_left = reliable_packets_left.saturating_sub(1);
                if self.recovery.is_some() {
                    self.paced_ms.push_back(now_ms);
                }
            }
            if has_resend {
                resend_packets_left = resend_packets_left.saturating_sub(1);
                self.repair_ms.push_back(now_ms);
            }
            if has_probe || (ping && probing.is_some()) {
                probe_packets_left = probe_packets_left.saturating_sub(1);
                self.timer_ms.push_back(now_ms);
            }
            if !frame.is_empty() {
                self.ack_sent(carried_timestamps);
            }
            if let Some(lo) = named_from {
                self.unreported.remove(lo, u64::MAX);
            }
            for (stream, max) in &packet_credits {
                if let Some(st) = self.recv_streams.get_mut(usize::from(*stream)) {
                    st.advertised = st.advertised.max(*max);
                    st.advert_sent_ms = now_ms;
                }
                self.stats.credits_sent += 1;
            }
            self.commit_packet(now_ms, body_len, ids, data, has_resend, ack_eliciting);
            self.note_frame_sent(seq, span, frame.len());
            if let Some(sent) = self.sent.back_mut() {
                sent.credits = packet_credits;
            }
            packet_count += 1;
            if !ack_eliciting {
                break;
            }
        }
        self.body_scratch = body.finish();
        self.stats.stream_blocked += u64::from(flow_blocked);
        self.stats.background_held += u64::from(background_held);
        if held {
            self.stats.paced_holds += 1;
            self.round.held += 1;
        }
        self.send_mtu_probe(now_ms, header, rto, &mut transmit);
    }

    /// Sends the path MTU probe that is due, if any (spec 120 § 4.3, RFC 8899 § 4.1): a ping
    /// padded to the probe size, with no acknowledgment frame and no data, so its loss costs
    /// nothing. Never while the connection paces an outage's backlog or probes an interruption.
    fn send_mtu_probe(
        &mut self,
        now_ms: u64,
        header: &[u8],
        rto: u64,
        transmit: &mut impl FnMut(Vec<u8>) -> Result<(), crate::socket::TxRefused>,
    ) {
        if !self.pmtu.enabled() || self.recovery.is_some() || self.interrupted_probe.is_some() {
            return;
        }
        let Some(request) = self.pmtu.probe_due(now_ms, self.srtt.is_some()) else {
            return;
        };
        let pn_len = crate::ack::packet_number_len(self.local_seq, self.largest_acked);
        if request.size < header.len() + 2 + pn_len {
            return;
        }
        let mut out = WireWriter::with_capacity(request.size);
        out.raw(header);
        let mut flags = match pn_len {
            1 => 0,
            2 => 1,
            _ => 2,
        };
        if self.config.receive_timestamps {
            flags |= FLAG_WANT_TIMESTAMPS;
        }
        out.u8(flags);
        crate::ack::write_packet_number(&mut out, self.local_seq, pn_len);
        out.u8(RECORD_PING);
        out.u8(RECORD_PADDING);
        let pad = request.size.saturating_sub(out.len());
        out.raw(&vec![0u8; pad]);
        let body_len = out.len() - header.len();
        if let Err(refused) = transmit(out.finish()) {
            // Too large for the transport: the size is refused. A socket drop (ADR-8407) says
            // nothing about the size: the probe stays due and goes out on a later poll.
            if matches!(refused, crate::socket::TxRefused::TooLarge(_)) {
                self.pmtu.probe_refused(request, now_ms);
            }
            self.refused(refused, header.len());
            return;
        }
        let seq = self.local_seq;
        // A probe does not move the probe timeout of reliable data.
        let last_ack_eliciting = self.last_ack_eliciting_ms;
        self.commit_packet(now_ms, body_len, Vec::new(), false, false, true);
        self.last_ack_eliciting_ms = last_ack_eliciting;
        if let Some(packet) = self.sent.back_mut().filter(|p| p.seq == seq) {
            packet.mtu_probe = true;
        }
        self.stats.mtu_probes_sent += 1;
        self.pmtu.probe_sent(request, seq, now_ms.saturating_add(rto));
    }

    /// The outstanding path MTU probe's deadline passed: lost when a packet sent after it was
    /// acknowledged, otherwise (an outage) inconclusive and sent again after a probe timeout.
    fn expire_mtu_probe(&mut self, now_ms: u64, rto: u64) {
        let Some(seq) = self.pmtu.expired(now_ms) else {
            return;
        };
        let front = self.sent.front().map_or(0, |p| p.seq);
        if let Some(packet) = seq
            .checked_sub(front)
            .and_then(|i| self.sent.get_mut(usize::try_from(i).ok()?))
            .filter(|p| p.seq == seq)
        {
            packet.lost = true;
        }
        if self.largest_acked.is_some_and(|l| l > seq) {
            self.stats.mtu_probes_lost += 1;
            self.lost_mtu_probe(seq, now_ms);
        } else {
            self.pmtu.probe_inconclusive(seq, now_ms.saturating_add(rto));
        }
    }

    fn lost_mtu_probe(&mut self, seq: u64, now_ms: u64) {
        let before = self.pmtu.counters().black_holes;
        self.pmtu.probe_lost(seq, now_ms);
        if self.pmtu.counters().black_holes > before {
            self.stats.mtu_black_holes += 1;
        }
    }

    /// Packet `seq` just committed carried an acknowledgment frame of `bytes` that named `span`.
    fn note_frame_sent(&mut self, seq: u64, span: Option<FrameNamed>, bytes: usize) {
        self.stats.ack_frame_bytes += bytes as u64;
        if let Some(packet) = self.sent.back_mut().filter(|p| p.seq == seq) {
            packet.ack_named = span;
        }
    }

    /// An acknowledgment frame left: nothing is owed until the next arrival.
    fn ack_sent(&mut self, carried_timestamps: bool) {
        self.ack_pending = false;
        self.ack_urgent = false;
        self.ack_eliciting_unacked = 0;
        self.ack_deadline_ms = None;
        if carried_timestamps {
            self.pending_timestamps.clear();
        }
    }

    fn commit_packet(
        &mut self,
        now_ms: u64,
        body_len: usize,
        reliable_ids: Vec<u64>,
        data: bool,
        resent: bool,
        ack_eliciting: bool,
    ) {
        let seq = self.local_seq;
        self.local_seq = self.local_seq.saturating_add(1).min(crate::ack::VARINT_MAX);
        if let Some((due, step)) = self.interrupted_probe.as_mut() {
            *step = step.saturating_add(1).min(4);
            let base = 250_u64 << *step;
            // Number-derived jitter is deterministic for replay; never above the 4 s cap.
            let jitter = (seq & 0xFFFF).wrapping_mul(0x9e37_79b9) % (base / 8 + 1);
            *due = now_ms.saturating_add(base.saturating_add(jitter).min(4_000));
        }
        if self.sent.len() >= SENT_WINDOW {
            // Too old to be tracked. Without a verdict it is neither delivered nor lost.
            self.sent.pop_front();
        }
        self.sent.push_back(SentPacket {
            seq,
            sent_ms: now_ms,
            ack_eliciting,
            reliable_ids,
            credits: Vec::new(),
            acked: false,
            lost: false,
            loss_sample: 0,
            delivered_at_send: self.window.delivered,
            data,
            resent,
            size: u16::try_from(body_len + self.header_len).unwrap_or(u16::MAX),
            mtu_probe: false,
            path_probe: false,
            ack_named: None,
            bytes: body_len,
        });
        if ack_eliciting {
            self.wire_share.sent(now_ms, body_len, self.header_len);
            self.unanswered_since_ms.get_or_insert(now_ms);
        }
        self.round.sent_bytes += body_len as u64;
        if !ack_eliciting {
            self.round.essential_bytes += body_len as u64;
        }
        if ack_eliciting {
            self.last_ack_eliciting_ms = Some(now_ms);
        } else {
            self.stats.ack_only_sent += 1;
        }
        self.last_send_ms = Some(now_ms);
        self.stats.packets_sent += 1;
        self.stats.bytes_sent += body_len as u64;
        self.window_sent += body_len as u64;
    }

    /// Builds one ping packet with the next packet number, for a path probe (RFC 9000 § 8.2
    /// without a wire change, ADR-8299): the endpoint sends it only on a candidate path. A ping is
    /// ack-eliciting and a lone ping is acknowledged at once (transport v2, ADR-8184), so a packet
    /// from that path whose acknowledgment frame names its number ([`packet_acknowledges`]) proves
    /// the far end received it there. Returns `(packet number, packet)`.
    ///
    /// The probe never counts as lost and its acknowledgment is neither a round-trip, loss,
    /// reordering nor path MTU sample (a probe of another path says nothing about the validated
    /// one); it leaves the keepalive and probe-timeout timers of the validated path as they were.
    pub fn path_probe(&mut self, now_ms: u64, header: &[u8]) -> (u64, Vec<u8>) {
        let pn_len = crate::ack::packet_number_len(self.local_seq, self.largest_acked);
        let mut out = WireWriter::with_capacity(header.len() + 2 + pn_len);
        out.raw(header);
        let mut flags = match pn_len {
            1 => 0,
            2 => 1,
            _ => 2,
        };
        if self.config.receive_timestamps {
            flags |= FLAG_WANT_TIMESTAMPS;
        }
        out.u8(flags);
        crate::ack::write_packet_number(&mut out, self.local_seq, pn_len);
        out.u8(RECORD_PING);
        let body_len = out.len() - header.len();
        let seq = self.local_seq;
        let (last_ack_eliciting, last_send) = (self.last_ack_eliciting_ms, self.last_send_ms);
        self.commit_packet(now_ms, body_len, Vec::new(), false, false, true);
        self.last_ack_eliciting_ms = last_ack_eliciting;
        self.last_send_ms = last_send;
        if let Some(packet) = self.sent.back_mut().filter(|p| p.seq == seq) {
            // `lost` already set: neither loss detection nor a fall out of the window counts it.
            packet.lost = true;
            packet.path_probe = true;
        }
        (seq, out.finish())
    }

    /// Processes one packet body (the endpoint header already stripped). Malformed input is an
    /// error and changes nothing that was not already parsed; it never panics. A protocol
    /// violation fails the connection for good ([`Connection::failure`]). Reassembly has no
    /// shared budget here; a server passes one to [`Connection::receive_within`].
    pub fn receive(&mut self, body: &[u8], now_ms: u64) -> Result<(), WireError> {
        let _span = tracing::trace_span!("regia_net::connection_receive").entered();
        self.receive_within(body, now_ms, &mut ReassemblyBudget::unlimited())
    }

    /// [`Connection::receive`], storing out-of-order fragments only within `budget` (see
    /// [`ReassemblyBudget`]). The budget's `used` moves by this connection's change.
    pub fn receive_within(
        &mut self,
        body: &[u8],
        now_ms: u64,
        budget: &mut ReassemblyBudget,
    ) -> Result<(), WireError> {
        self.heard_ms = Some(self.heard_ms.map_or(now_ms, |at| at.max(now_ms)));
        let before = self.buffered_bytes;
        let result = self.receive_inner(body, now_ms, budget, true);
        budget.apply(before, self.buffered_bytes);
        result
    }

    /// [`Connection::receive_within`] for a packet nothing proves is the peer's (a plain-link
    /// packet from an address the peer has not validated, ADR-8606): a violation found before the
    /// packet changes any state (an over-long fragment, an undeclared stream, a stream or the
    /// reassembly over its cap) is an error, but it does not fail the connection, so such a
    /// packet cannot end the peer's link (ADR-8666). A violation found while the records are
    /// applied (a reliable message over its cap, a full inbox) still fails it. Test-only since
    /// ADR-10077 (KI-NT-018): the endpoints hold such a packet until its address validates and
    /// never hand it to a connection; the tests keep pinning the non-latching receive.
    #[cfg(test)]
    pub(crate) fn receive_unvalidated_within(
        &mut self,
        body: &[u8],
        now_ms: u64,
        budget: &mut ReassemblyBudget,
    ) -> Result<(), WireError> {
        let before = self.buffered_bytes;
        let result = self.receive_inner(body, now_ms, budget, false);
        budget.apply(before, self.buffered_bytes);
        result
    }

    /// `latch`: a violation found before any state changes fails the connection for good.
    fn receive_inner(
        &mut self,
        body: &[u8],
        now_ms: u64,
        budget: &ReassemblyBudget,
        latch: bool,
    ) -> Result<(), WireError> {
        let _span = tracing::trace_span!("regia_net::connection_receive").entered();
        if let Some(failure) = self.failure {
            return Err(failure_error(failure));
        }
        // Validate everything before touching any state so a corrupt packet is all-or-nothing.
        let (flags, pn_len, truncated, ack, records) = match parse_packet(body) {
            Ok(parsed) => parsed,
            Err(e) => {
                if latch {
                    // An authenticated packet is the peer's traffic even when it does not parse
                    // (spec 120 § 5.2): its gap counts toward a stall like a duplicate's, and
                    // nothing else of it is applied. An unvalidated one must not count, or anyone
                    // on the path could run an honest peer's stall clock (ADR-8717, ADR-8764).
                    let interval = self.stall_interval(now_ms);
                    self.unparseable_in_stall = true;
                    self.check_stall(None, interval, now_ms)?;
                }
                return Err(e);
            }
        };
        let seq = crate::ack::decode_packet_number(self.recv.largest(), truncated, pn_len);
        let max_fragment = self.max_fragment_bytes();
        if let Some(len) = records.iter().find_map(|record| match record {
            Record::Reliable { bytes, .. } if bytes.len() > max_fragment => Some(bytes.len()),
            _ => None,
        }) {
            if latch {
                self.fail(LinkFailure::FragmentTooLong);
            }
            return Err(WireError::TooLong {
                len,
                max: max_fragment,
            });
        }
        let declared = self.stream_table.len();
        if records.iter().any(|record| match record {
            Record::Reliable { stream, .. } | Record::Credit { stream, .. } => {
                usize::from(*stream) >= declared
            }
            _ => false,
        }) {
            if latch {
                self.fail(LinkFailure::UnknownStream);
            }
            return Err(failure_error(LinkFailure::UnknownStream));
        }
        // Not stored yet: the receive clock moves only after the cap checks below (ADR-8734).
        let interval = self.stall_interval_at(now_ms);
        let (growth, over_stream) = self.stored_growth(&records);
        if let Some((len, max)) = over_stream {
            if latch {
                self.fail(LinkFailure::StreamOverCap);
            }
            return Err(WireError::TooLong { len, max });
        }
        if growth > 0 {
            let cap = self.reassembly_cap_bytes();
            if self.buffered_bytes + growth > cap {
                let len = self.buffered_bytes + growth;
                if latch {
                    self.fail(LinkFailure::ReassemblyOverCap);
                }
                return Err(WireError::TooLong { len, max: cap });
            }
        }
        // The first state this packet changes, after every check that refuses it whole: an
        // unvalidated packet over a cap leaves the link as it was (ADR-8734).
        self.last_rx_ms = Some(now_ms);
        if growth > 0 && budget.used().saturating_add(growth) > budget.limit() {
            // As if lost: nothing is acknowledged, so an honest sender resends it. The time
            // still counts toward a stall, or a peer could keep its gap by flooding refusals.
            self.stats.reassembly_refused += 1;
            return self.check_stall(None, interval, now_ms);
        }
        let mut expected_before = [0u16; MAX_STREAMS];
        for (slot, st) in expected_before.iter_mut().zip(&self.recv_streams) {
            *slot = st.next_expected;
        }
        self.roll_rate_window(now_ms);
        self.stats.bytes_received += body.len() as u64;
        self.window_recv += body.len() as u64;
        let previous_largest = self.recv.largest();
        let stale = match self.track_remote(seq, now_ms) {
            Arrival::Duplicate => {
                self.stats.duplicates += 1;
                return self.check_stall(None, interval, now_ms);
            }
            Arrival::Rejected => {
                self.stats.out_of_window += 1;
                return self.check_stall(None, interval, now_ms);
            }
            Arrival::Fresh => false,
            Arrival::Stale => true,
        };
        if let Some(ack) = ack {
            self.process_acks(&ack, flags & FLAG_ACK_OLDER == 0, now_ms);
        }
        self.stats.packets_received += 1;
        self.peer_wants_timestamps = flags & FLAG_WANT_TIMESTAMPS != 0;
        if self.peer_wants_timestamps {
            if self.pending_timestamps.len() >= MAX_PENDING_TIMESTAMPS {
                self.pending_timestamps.pop_front();
            }
            self.pending_timestamps.push_back((seq, now_ms));
        }
        self.ack_pending = true;
        if !records.is_empty() {
            // Ack-eliciting (RFC 9000 § 13.2): every second one, out-of-order ones and ones
            // after a gap are acknowledged at once, the rest within the ack delay.
            self.ack_eliciting_unacked = self.ack_eliciting_unacked.saturating_add(1);
            self.ack_deadline_ms
                .get_or_insert(now_ms.saturating_add(MAX_ACK_DELAY_MS));
            let reordered = previous_largest.is_some_and(|l| seq < l || seq > l.saturating_add(1));
            // A ping alone (a keepalive or a probe) has nothing to share a packet with: delaying
            // its acknowledgment would only stretch the round trip the peer measures.
            let ping_only = records.iter().all(|rec| matches!(rec, Record::Ping));
            if self.ack_eliciting_unacked >= ACK_ELICITING_THRESHOLD || reordered || ping_only {
                self.ack_urgent = true;
            }
        }
        if let Some((due, step)) = self.interrupted_probe.as_mut() {
            // The peer is there: answer now instead of waiting out the probe backoff, so both
            // ends can confirm the path within a round trip (ADR-7995).
            *due = now_ms;
            *step = 0;
        }
        for record in records {
            match record {
                Record::Unreliable(bytes) => {
                    if stale {
                        continue;
                    }
                    if self.received.len() >= MAX_RECEIVED_MESSAGES {
                        self.stats.inbound_dropped += 1;
                        continue;
                    }
                    self.received.push_back((Channel::Unreliable, bytes.to_vec()));
                }
                Record::Reliable {
                    stream,
                    id,
                    first,
                    more,
                    bytes,
                } => self.accept_reliable(usize::from(stream), id, first, more, bytes)?,
                Record::Credit { stream, max } => {
                    if let Some(st) = self.send_streams.get_mut(usize::from(stream)) {
                        st.max_data = st.max_data.max(max);
                    }
                }
                Record::Ping => {}
            }
        }
        self.retire();
        self.check_stall(Some(&expected_before), interval, now_ms)
    }

    /// Net change of `buffered_bytes` if `records` were accepted: what they would store ahead of
    /// a gap, less what they would let drain, summed over the streams; and the first stream whose
    /// own stored bytes would pass its window, as (bytes, window). Allocation-free past one small
    /// sort.
    fn stored_growth(&self, records: &[Record<'_>]) -> (usize, Option<(usize, usize)>) {
        let mut fresh: Vec<(u8, usize, usize)> = records
            .iter()
            .filter_map(|record| match record {
                Record::Reliable {
                    stream, id, bytes, ..
                } => {
                    let st = self.recv_streams.get(usize::from(*stream))?;
                    let ahead = usize::from(id.wrapping_sub(st.next_expected));
                    (ahead < RELIABLE_WINDOW && !st.stored.contains_key(id)).then_some((
                        *stream,
                        ahead,
                        bytes.len(),
                    ))
                }
                Record::Unreliable(_) | Record::Ping | Record::Credit { .. } => None,
            })
            .collect();
        if fresh.is_empty() {
            return (0, None);
        }
        // The first copy of an id in a packet is the one stored (`or_insert_with`).
        fresh.sort_by_key(|&(stream, ahead, _)| (stream, ahead));
        fresh.dedup_by_key(|&mut (stream, ahead, _)| (stream, ahead));
        let mut total = 0usize;
        let mut over = None;
        for group in fresh.chunk_by(|a, b| a.0 == b.0) {
            let s = usize::from(group[0].0);
            let st = &self.recv_streams[s];
            // How far the in-order drain would run: through every offset present in the packet or
            // already stored.
            let mut drain = 0usize;
            let mut freed = 0usize;
            let mut next = group.iter().peekable();
            while drain < RELIABLE_WINDOW {
                if next.peek().is_some_and(|&&(_, ahead, _)| ahead == drain) {
                    next.next();
                } else if let Some(frag) = st.stored.get(&st.next_expected.wrapping_add(drain as u16)) {
                    freed += frag.bytes.len();
                } else {
                    break;
                }
                drain += 1;
            }
            let stored: usize = next.map(|&(_, _, len)| len).sum();
            let growth = stored.saturating_sub(freed);
            let window = self.stream_table[s].window_bytes;
            if over.is_none() && st.buffered + growth > window {
                over = Some((st.buffered + growth, window));
            }
            total += growth;
        }
        (total, over)
    }

    /// The peer traffic one packet arriving at `now_ms` counts toward a stall: the time since the
    /// previous packet, at most one RTO as it stood before this packet's acknowledgments (the
    /// first sample after an outage can carry the outage itself). Moves no state.
    fn stall_interval_at(&self, now_ms: u64) -> u64 {
        self.last_rx_ms
            .map_or(0, |last| now_ms.saturating_sub(last))
            .min(self.resend_timeout_ms())
    }

    /// [`Self::stall_interval_at`], and the packet's arrival stored (an authenticated packet that
    /// does not parse, ADR-8764).
    fn stall_interval(&mut self, now_ms: u64) -> u64 {
        let interval = self.stall_interval_at(now_ms);
        self.last_rx_ms = Some(now_ms);
        interval
    }

    /// An authenticated record of the peer that is not a payload (a `Join` resend, or an `Accept`
    /// or `Deny` only a server sends) arrived at `now_ms` (ADR-8883). It is the peer's traffic
    /// (spec 120 § 5.2), so its gap counts toward a stalled reassembly the way an authenticated
    /// packet that does not parse does (ADR-8764): capped at one RTO, the loss stretch withheld
    /// (the sender chooses what it sends; an honest one sends none of these while a fragment of
    /// its own waits), and nothing of it applied. `Err` once the stall passes its deadline: the
    /// connection has failed with [`LinkFailure::ReassemblyStalled`].
    pub(crate) fn note_authenticated_record(&mut self, now_ms: u64) -> Result<(), WireError> {
        if let Some(failure) = self.failure {
            return Err(failure_error(failure));
        }
        let interval = self.stall_interval(now_ms);
        self.unparseable_in_stall = true;
        self.check_stall(None, interval, now_ms)
    }

    /// Counts `interval` of peer traffic toward a reassembly stall on every stream that waits on
    /// a missing fragment and made no progress since `before` (each stream's next expected id
    /// when the packet arrived; `None`: no stream progressed), and fails the connection when one
    /// passes [`Connection::reassembly_stall_ms`]. Progress, or nothing waiting, resets a
    /// stream. The caller caps one gap between packets at the RTO as it stood before the packet
    /// (ADR-8521), so an outage counts one RTO, not the outage, even when the first round-trip
    /// sample after it carries the outage.
    fn check_stall(
        &mut self,
        before: Option<&[u16; MAX_STREAMS]>,
        interval: u64,
        now_ms: u64,
    ) -> Result<(), WireError> {
        let mut longest = 0;
        let mut stalling = false;
        for (i, st) in self.recv_streams.iter_mut().enumerate() {
            let waiting = !st.stored.is_empty() || !st.assembling.is_empty();
            let progressed = before.is_some_and(|b| b[i] != st.next_expected);
            if progressed || !waiting {
                st.stall_ms = 0;
                continue;
            }
            stalling = true;
            st.stall_ms = st.stall_ms.saturating_add(interval);
            longest = longest.max(st.stall_ms);
        }
        if !stalling {
            // No stream waits: the next stall starts with the measured-loss stretch again.
            self.unparseable_in_stall = false;
        }
        // The loss-stretched deadline is never below the base one: only measure loss past it.
        self.stall_checked_ms = now_ms;
        let base = self.reassembly_stall_base_at(now_ms);
        let deadline = self.reassembly_stall_deadline_at(now_ms);
        if longest >= base && longest >= deadline {
            self.fail(LinkFailure::ReassemblyStalled);
            return Err(failure_error(LinkFailure::ReassemblyStalled));
        }
        Ok(())
    }

    /// Stores one reliable fragment of stream `s` and delivers what it completes: in order on an
    /// ordered stream; on an unordered stream also any whole message past a gap.
    fn accept_reliable(
        &mut self,
        s: usize,
        id: u16,
        first: bool,
        more: bool,
        bytes: &[u8],
    ) -> Result<(), WireError> {
        let ordered = self.stream_table[s].ordered;
        let st = &mut self.recv_streams[s];
        let ahead = usize::from(id.wrapping_sub(st.next_expected));
        if ahead >= RELIABLE_WINDOW {
            // Already delivered (behind the window) or impossibly far ahead: ignore.
            return Ok(());
        }
        let std::collections::hash_map::Entry::Vacant(slot) = st.stored.entry(id) else {
            return Ok(());
        };
        slot.insert(InFragment {
            bytes: bytes.to_vec(),
            len: bytes.len(),
            first,
            more,
            delivered: false,
        });
        st.buffered += bytes.len();
        self.buffered_bytes += bytes.len();
        if !ordered && ahead > 0 {
            self.deliver_past_gap(s, id)?;
        }
        let max = self.config.max_reliable_message_bytes;
        loop {
            let st = &mut self.recv_streams[s];
            let Some(frag) = st.stored.remove(&st.next_expected) else {
                break;
            };
            st.next_expected = st.next_expected.wrapping_add(1);
            st.consumed += frag.len as u64;
            if frag.delivered {
                // Delivered past the gap already; a partial message before it was malformed.
                st.assembling.clear();
                continue;
            }
            st.buffered = st.buffered.saturating_sub(frag.len);
            self.buffered_bytes = self.buffered_bytes.saturating_sub(frag.len);
            if st.assembling.len() + frag.len > max {
                let len = st.assembling.len() + frag.len;
                // The stream cannot be resynchronized: every later message is suspect.
                self.fail(LinkFailure::MessageTooLong);
                return Err(WireError::TooLong { len, max });
            }
            st.assembling.extend_from_slice(&frag.bytes);
            if !frag.more {
                let message = std::mem::take(&mut st.assembling);
                self.deliver(s, message)?;
            }
        }
        Ok(())
    }

    /// On an unordered stream: delivers the message fragment `id` belongs to when all of its
    /// fragments are stored, though an earlier one is still missing. Its fragments stay as
    /// markers (bytes freed) until the gap before them fills.
    fn deliver_past_gap(&mut self, s: usize, id: u16) -> Result<(), WireError> {
        let st = &self.recv_streams[s];
        let mut start = id;
        loop {
            let Some(f) = st.stored.get(&start) else {
                return Ok(());
            };
            if f.delivered {
                return Ok(());
            }
            if f.first {
                break;
            }
            start = start.wrapping_sub(1);
            if usize::from(start.wrapping_sub(st.next_expected)) >= RELIABLE_WINDOW {
                return Ok(());
            }
        }
        let max = self.config.max_reliable_message_bytes;
        let (mut end, mut total) = (start, 0usize);
        loop {
            let Some(f) = st.stored.get(&end) else {
                return Ok(());
            };
            if f.delivered || (end != start && f.first) {
                return Ok(());
            }
            total += f.len;
            if total > max {
                self.fail(LinkFailure::MessageTooLong);
                return Err(WireError::TooLong { len: total, max });
            }
            if !f.more {
                break;
            }
            end = end.wrapping_add(1);
            if usize::from(end.wrapping_sub(st.next_expected)) >= RELIABLE_WINDOW {
                return Ok(());
            }
        }
        let st = &mut self.recv_streams[s];
        let mut message = Vec::with_capacity(total);
        let mut at = start;
        loop {
            if let Some(f) = st.stored.get_mut(&at) {
                message.append(&mut f.bytes);
                f.bytes = Vec::new();
                f.delivered = true;
                st.buffered = st.buffered.saturating_sub(f.len);
                self.buffered_bytes = self.buffered_bytes.saturating_sub(f.len);
            }
            if at == end {
                break;
            }
            at = at.wrapping_add(1);
        }
        self.deliver(s, message)
    }

    /// Queues a whole message of stream `s` for [`Connection::next_message`].
    fn deliver(&mut self, s: usize, message: Vec<u8>) -> Result<(), WireError> {
        if self.received.len() >= MAX_RECEIVED_MESSAGES {
            self.fail(LinkFailure::InboxFull);
            return Err(WireError::TooLong {
                len: self.received.len() + 1,
                max: MAX_RECEIVED_MESSAGES,
            });
        }
        // Held back by nothing: another stream waits on a gap, or (unordered) this one does.
        if self
            .recv_streams
            .iter()
            .enumerate()
            .any(|(i, st)| !st.stored.is_empty() && (i != s || !self.stream_table[s].ordered))
        {
            self.stats.delivered_past_gap += 1;
        }
        self.received
            .push_back((Channel::of_stream(StreamId(s as u8)), message));
        Ok(())
    }

    fn track_remote(&mut self, seq: u64, now_ms: u64) -> Arrival {
        let Some(largest) = self.recv.largest() else {
            self.recv.insert(seq);
            self.unreported.insert(seq);
            self.note_arrival(seq, None);
            self.first_remote_seq = Some(seq);
            self.largest_recv_ms = now_ms;
            return Arrival::Fresh;
        };
        if seq > largest {
            if seq - largest > MAX_FORWARD_JUMP {
                self.far_ahead_streak += 1;
                if self.far_ahead_streak < RESYNC_AFTER {
                    return Arrival::Rejected;
                }
            }
            self.far_ahead_streak = 0;
            self.recv.insert(seq);
            self.unreported.insert(seq);
            self.note_arrival(seq, Some(largest));
            self.largest_recv_ms = now_ms;
            return Arrival::Fresh;
        }
        self.far_ahead_streak = 0;
        if self.recv.insert(seq) == Inserted::Duplicate {
            return Arrival::Duplicate;
        }
        self.unreported.insert(seq);
        self.note_arrival(seq, Some(largest));
        // Acknowledged like any other, but its unreliable records are too old to deliver.
        if largest - seq > 32 {
            Arrival::Stale
        } else {
            Arrival::Fresh
        }
    }

    fn process_acks(&mut self, frame: &AckFrame, sample_rtt: bool, now_ms: u64) {
        if frame.largest >= self.local_seq {
            // Names a packet this side never sent: a confused or hostile peer. Ignore the frame.
            return;
        }
        let Some(front) = self.sent.front().map(|p| p.seq) else {
            return;
        };
        // Receive timestamps first: the packets they name may retire below.
        if !frame.timestamps.is_empty() {
            self.timestamp_reports += 1;
            for ts in &frame.timestamps {
                let Some(packet) = ts
                    .packet
                    .checked_sub(front)
                    .and_then(|i| self.sent.get(usize::try_from(i).ok()?))
                    .filter(|p| p.seq == ts.packet)
                else {
                    continue;
                };
                if self.receive_timestamps.len() >= MAX_RECEIVE_TIMESTAMPS {
                    self.receive_timestamps.pop_front();
                }
                self.receive_timestamps.push_back(ReceiveTimestamp {
                    packet: ts.packet,
                    sent_ms: packet.sent_ms,
                    received_us: ts.after_largest_us,
                    report: self.timestamp_reports,
                });
            }
        }
        let mut outcome = AckOutcome::default();
        let mut sample = None;
        // Only numbers still tracked can be acknowledged: a range the size of the number space
        // costs no more than the sent-packet window.
        let last_tracked = front + self.sent.len().saturating_sub(1) as u64;
        for (lo, hi) in &frame.ranges {
            let end = (*hi).min(last_tracked);
            let mut seq = (*lo).max(front);
            while seq <= end {
                if let Some(found) = self.ack_one(seq, front, now_ms, &mut outcome)
                    && seq == frame.largest
                    && sample_rtt
                {
                    sample = Some(found);
                }
                seq += 1;
            }
        }
        // A round-trip sample comes only from the largest acknowledged packet, when this frame
        // is the first to acknowledge it, it was ack-eliciting and never declared lost (RFC 9002
        // § 5.1). The peer's ack delay comes off, up to `MAX_ACK_DELAY_MS`, unless that would go
        // under the minimum round trip (§ 5.3).
        if let Some((latest, sent_ms)) = sample {
            let delay = (frame.delay_us as f64 / 1_000.0).min(MAX_ACK_DELAY_MS as f64);
            self.note_min_rtt(latest, now_ms);
            let min = self.min_rtt.front().map_or(latest, |(_, m)| *m);
            let adjusted = if latest - delay >= min {
                latest - delay
            } else {
                latest
            };
            self.observe_rtt(adjusted, sent_ms, now_ms);
            if self.round.min_rtt <= 0.0 || adjusted < self.round.min_rtt {
                self.round.min_rtt = adjusted;
            }
            self.round.delay_samples += 1;
            // The same packet's one-way delay (ADR-10050): the peer's clock at its arrival minus
            // this side's when it left. No ack delay to take off, and nothing of the way back.
            let one_way = self.one_way.sample(now_ms, sent_ms, frame.arrival_ms);
            if self.round.min_one_way <= 0.0 || one_way < self.round.min_one_way {
                self.round.min_one_way = one_way;
            }
        }
        if !frame.timestamps.is_empty() && self.config.rate.enabled {
            // The peer's arrival times, anchored where its largest packet's arrival is estimated
            // on this clock (now minus the ack delay; the return path's variation is noise).
            let anchor = now_ms as f64 - frame.delay_us as f64 / 1_000.0;
            let report = self.timestamp_reports;
            let mut points: Vec<(f64, f64)> = self
                .receive_timestamps
                .iter()
                .filter(|t| t.report == report)
                .map(|t| (t.sent_ms as f64, anchor + t.received_us as f64 / 1_000.0))
                .collect();
            points.sort_by(|a, b| a.0.total_cmp(&b.0));
            for (sent, arrival) in points {
                self.trendline.add(sent, arrival);
            }
        }
        if outcome.newly_acked == 0 {
            self.end_rate_round(now_ms);
            return;
        }
        // Any acknowledgment of a new packet resets the probe backoff (RFC 9002 § 6.2.1).
        self.pto_count = 0;
        self.unanswered_since_ms = None;
        self.ack_epoch = self.ack_epoch.wrapping_add(1);
        self.last_ack_ms = Some(now_ms);
        self.retire_fragments();
        self.note_delivery(&outcome);
        let rto = self.resend_timeout_ms();
        if let Some(lost_sent_ms) = self.detect_losses(now_ms, rto) {
            // Reliable data was lost. When the acknowledged packet left an outage's worth of time
            // after the lost one, with nothing in between acknowledged, the path was down: pace
            // the backlog (the persistent-congestion idea of RFC 9002 § 7.6). A long first round
            // trip is not an outage: both packets left at the start of it.
            let span = self.largest_acked_sent_ms.saturating_sub(lost_sent_ms);
            if self.recovery.is_none() && span >= self.outage_after_ms(rto) {
                self.enter_recovery(Some(now_ms));
            }
            if let Some(r) = self.recovery.as_mut() {
                r.lost_data = true;
            }
        }
        if let Some(r) = self.recovery.as_mut() {
            r.restored_ms.get_or_insert(now_ms);
        }
        self.end_rate_round(now_ms);
    }

    /// Ends the rate controller's feedback round once it lasted `max(srtt, 50 ms)` (spec 120
    /// § 4.2 item 1) and feeds it to the controller.
    fn end_rate_round(&mut self, now_ms: u64) {
        if !self.config.rate.enabled {
            return;
        }
        let start = *self.round.start_ms.get_or_insert(now_ms);
        let srtt = self.srtt.unwrap_or(0.0);
        let length = (srtt.ceil() as u64).max(crate::cc::MIN_ROUND_MS);
        let interval = now_ms.saturating_sub(start);
        if interval < length {
            return;
        }
        let r = std::mem::take(&mut self.round);
        let sample = crate::cc::RoundSample {
            interval_ms: interval,
            acked_bytes: r.acked_bytes,
            sent_bytes: r.sent_bytes,
            acked_packets: r.acked_packets,
            lost_packets: r.lost_packets,
            round_min_rtt_ms: r.min_rtt,
            srtt_ms: srtt,
            min_rtt_ms: self.min_rtt.front().map_or(0.0, |(_, m)| *m),
            delay_samples: r.delay_samples,
            round_min_one_way_ms: r.min_one_way,
            min_one_way_ms: self.one_way.min_ms(),
            gradient: self.config.receive_timestamps.then(|| self.trendline.signal()),
            held_bytes: r.held,
            capped: self
                .rate_cap
                .is_some_and(|c| c < f64::from(self.rate.target_bytes_per_second())),
            essential_bytes: r.essential_bytes,
        };
        self.rate.on_round(now_ms, &sample);
        let essential = r.essential_bytes as f64 * 1_000.0 / interval.max(1) as f64;
        self.essential_rate = 0.75 * self.essential_rate + 0.25 * essential;
        self.round.start_ms = Some(now_ms);
        tracing::trace!(
            target: "regia_net",
            target_bps = self.rate.target_bytes_per_second(),
            state = self.rate.state().as_str(),
            queue_ms = self.rate.queue_delay_ms(),
            delivery_bps = self.rate.delivery_bytes_per_second(),
            "rate round"
        );
    }

    /// The rate the pacer spreads this side's traffic at, bytes per second: the controller's
    /// target, or the shared uplink's fair share when that is lower. `None` with the controller
    /// off.
    pub fn pacing_rate_bytes_per_second(&self) -> Option<u32> {
        self.config.rate.enabled.then(|| {
            let t = f64::from(self.rate.target_bytes_per_second());
            self.rate_cap.map_or(t, |c| c.min(t)).max(1.0) as u32
        })
    }

    /// Bytes per second the game's own traffic may use now (spec 120 § 4.2 item 9): the pacing
    /// rate minus what the essential class uses. `None` with the controller off.
    pub fn send_allowance_bytes_per_second(&self) -> Option<u32> {
        let rate = f64::from(self.pacing_rate_bytes_per_second()?);
        Some((rate - self.essential_rate).max(0.0) as u32)
    }

    /// The rate controller (its target, state and signals).
    pub fn rate_controller(&self) -> &crate::cc::RateController {
        &self.rate
    }

    /// Caps the pacing rate at a shared uplink's fair share (spec 120 § 4.2 item 8); `None`
    /// lifts the cap. Like the controller's target, the cap is a rate of packet bodies: a share
    /// stated on the wire is divided by [`Self::wire_bytes_per_body_byte`] first (ADR-10097).
    pub fn set_rate_cap(&mut self, bytes_per_second: Option<f64>) {
        self.rate_cap = bytes_per_second;
    }

    /// What this side's packets cost on the wire for each body byte lately
    /// ([`crate::cc::WireShare::ratio`], ADR-10097): the controller's target, its delivery rate
    /// and [`LinkStats::rate_target_bps`] count packet bodies, and times this they are rates on
    /// the wire (endpoint header, IP and UDP headers included).
    pub fn wire_bytes_per_body_byte(&self) -> f64 {
        self.wire_share.ratio()
    }

    /// How long this side has been sending ack-eliciting packets without a new one acknowledged
    /// while the peer's own packets still arrive, milliseconds (ADR-10097): 0 while
    /// acknowledgments come, with nothing sent, or when nothing was heard from the peer in the
    /// latest [`crate::cc::UPLINK_STARVED_MS`] (a link that is down both ways is an outage, not
    /// this). A server reads its own uplink's drops from it: a full drop-tail queue loses the
    /// packets of the peers served last, all of them, while those peers go on talking.
    pub fn unanswered_ms(&self, now_ms: u64) -> u64 {
        self.unanswered_at(now_ms)
    }

    /// How long [`Self::unanswered_ms`] must run before a server counts this peer as starved by
    /// its uplink, milliseconds (ADR-10200): [`crate::cc::UPLINK_STARVED_RTTS`] of the path's
    /// minimum round trip plus the acknowledgment delay, within
    /// [`crate::cc::UPLINK_STARVED_MIN_MS`]..=[`crate::cc::UPLINK_STARVED_MS`]; the most before a
    /// round trip is known.
    pub fn starved_after_ms(&self) -> u64 {
        self.min_rtt
            .front()
            .map_or(crate::cc::UPLINK_STARVED_MS, |(_, min)| {
                let wait = (crate::cc::UPLINK_STARVED_RTTS * (min + MAX_ACK_DELAY_MS as f64)).ceil();
                (wait as u64).clamp(crate::cc::UPLINK_STARVED_MIN_MS, crate::cc::UPLINK_STARVED_MS)
            })
    }

    fn unanswered_at(&self, now_ms: u64) -> u64 {
        let heard = self
            .heard_ms
            .is_some_and(|at| now_ms.saturating_sub(at) < crate::cc::UPLINK_STARVED_MS);
        match self.unanswered_since_ms {
            Some(since) if heard => now_ms.saturating_sub(since),
            _ => 0,
        }
    }

    /// Tells the rate controller that this connection's sender splits a shared uplink among its
    /// peers (spec 120 § 4.2 item 8), whether or not a share is in force now
    /// (`RateController::set_shared_uplink`, ADR-8859).
    pub fn set_shared_uplink(&mut self, shared: bool) {
        self.rate.set_shared_uplink(shared);
        self.shared_uplink = shared;
        if shared {
            self.message_credit = 0.0;
        }
    }

    /// Changes the project rate the controller never exceeds (0 = none).
    pub fn set_max_rate(&mut self, bytes_per_second: u32) {
        self.config.rate.max_bytes_per_second = bytes_per_second;
        self.rate.set_max_bytes_per_second(bytes_per_second);
    }

    /// When the pacer lets the next packet that is waiting leave (spec 120 § 4.2 item 7): `None`
    /// when nothing waits for it or the controller is off; the endpoint polls again then.
    pub fn next_send_at_ms(&self, now_ms: u64) -> Option<u64> {
        let rate = f64::from(self.pacing_rate_bytes_per_second()?);
        let waiting = !self.unreliable_out.is_empty()
            || self
                .reliable_out
                .iter()
                .any(|f| f.last_sent_ms.is_none() || f.lost);
        waiting.then(|| {
            self.pacer
                .next_send_at_ms(now_ms, rate)
                .max(self.budget.next_send_at_ms(now_ms, rate))
        })
    }

    /// Adds a round-trip sample to the 10 s minimum window (a monotonic queue).
    fn note_min_rtt(&mut self, sample: f64, now_ms: u64) {
        while self
            .min_rtt
            .front()
            .is_some_and(|(at, _)| now_ms.saturating_sub(*at) > MIN_RTT_WINDOW_MS)
        {
            self.min_rtt.pop_front();
        }
        while self.min_rtt.back().is_some_and(|(_, m)| *m >= sample) {
            self.min_rtt.pop_back();
        }
        self.min_rtt.push_back((now_ms, sample));
    }

    /// Acknowledges `seq`. Returns the round-trip sample and the send time when the packet is
    /// newly acknowledged, ack-eliciting, was never declared lost and carried no resent fragment.
    fn ack_one(&mut self, seq: u64, front: u64, now_ms: u64, outcome: &mut AckOutcome) -> Option<(f64, u64)> {
        let offset = usize::try_from(seq.checked_sub(front)?).ok()?;
        let packet = self.sent.get_mut(offset)?;
        if packet.seq != seq || packet.acked {
            return None;
        }
        packet.acked = true;
        let late = packet.lost;
        let loss_sample = packet.loss_sample;
        let sent_ms = packet.sent_ms;
        let sample = now_ms.saturating_sub(sent_ms) as f64;
        let data = packet.data;
        let resent = packet.resent;
        let eliciting = packet.ack_eliciting;
        let delivered_at_send = packet.delivered_at_send;
        let (size, mtu_probe, path_probe) = (usize::from(packet.size), packet.mtu_probe, packet.path_probe);
        let ids = std::mem::take(&mut packet.reliable_ids);
        let named = packet.ack_named.take();
        let bytes = packet.bytes;
        let credits = std::mem::take(&mut packet.credits);
        outcome.newly_acked += 1;
        if let Some(named) = named {
            self.frame_confirmed(named);
        }
        if mtu_probe {
            // A probe carries no data and was never a loss sample: only the search hears of it
            // (a late acknowledgment of one already counted lost changes nothing).
            if !late {
                self.pmtu.probe_acked(seq, now_ms);
            }
        } else if self.pmtu.enabled() && !path_probe {
            self.pmtu.packet_acked(size, now_ms);
        }
        // An MTU probe or a path probe is no sample of the rate controller's round: its loss was
        // never counted there (a path probe is created lost), so its acknowledgment takes nothing
        // back either (ADR-8487).
        if !mtu_probe && !path_probe {
            self.round.acked_bytes += bytes as u64;
            self.round.acked_packets += 1;
            if late {
                self.round.lost_packets = self.round.lost_packets.saturating_sub(1);
            }
        }
        // A path probe left on another path: it neither moves the largest acknowledged number
        // (which would make the validated path's packets before it look lost) nor ends a round.
        for (stream, max) in credits {
            if let Some(st) = self.recv_streams.get_mut(usize::from(stream)) {
                st.acked_advert = st.acked_advert.max(max);
            }
        }
        if !path_probe && self.largest_acked.is_none_or(|l| seq > l) {
            self.largest_acked = Some(seq);
            self.largest_acked_sent_ms = sent_ms;
        }
        if !path_probe && seq >= self.window.round_end_seq {
            outcome.round_done = true;
        }
        // While recovering, a round trip at the current budget doubles it (RFC 5681 § 3.1).
        if let Some(rec) = self.recovery.as_mut()
            && !path_probe
            && seq >= rec.round_start_seq
        {
            rec.budget = rec.budget.saturating_mul(2).min(self.config.max_packets_per_poll);
            rec.round_start_seq = self.local_seq;
        }
        self.stats.packets_acked += 1;
        if mtu_probe || path_probe {
            // Neither a loss sample nor a reorder signal.
        } else if late {
            // Reordering, not loss: take the verdict back (spec 120 § 4.1 item 7, DO27 rule A7)
            // and widen the thresholds to what this path just showed (RFC 9002 § 6.1.1, RFC 8985
            // § 6.2).
            if let Some(largest) = self.largest_acked
                && largest > seq
            {
                self.reorder_threshold = self
                    .reorder_threshold
                    .max((largest - seq).saturating_add(1))
                    .min(MAX_REORDER_THRESHOLD);
            }
            // The loss delay that would have waited for this acknowledgment.
            let base = self
                .loss_delay_ms(self.resend_timeout_ms())
                .saturating_sub(self.reorder_window_ms);
            let needed = now_ms.saturating_sub(sent_ms).saturating_sub(base);
            self.reorder_window_ms = self
                .reorder_window_ms
                .max(needed.saturating_add(1))
                .min(MAX_REORDER_WINDOW_MS);
            self.stats.packets_lost = self.stats.packets_lost.saturating_sub(1);
            self.stats.spurious_losses += 1;
            let first = self.loss_pushed - self.loss_window.len() as u64;
            if let Some(slot) = loss_sample
                .checked_sub(first)
                .and_then(|i| self.loss_window.get_mut(i as usize))
            {
                *slot = true;
            }
        } else {
            self.push_loss_sample(true);
        }
        if data {
            self.window_data_acked += 1;
            outcome.delivery_base = Some(
                outcome
                    .delivery_base
                    .map_or(delivered_at_send, |base| base.min(delivered_at_send)),
            );
        }
        for key in ids {
            if let Some(index) = self.fragment_index(key)
                && let Some(frag) = self.reliable_out.get_mut(index)
                && !frag.acked
            {
                frag.acked = true;
                frag.lost = false;
                self.acked_interior += 1;
                let size = frag.wire_bytes();
                if frag.in_flight {
                    frag.in_flight = false;
                    self.in_flight_bytes = self.in_flight_bytes.saturating_sub(size);
                }
                self.window.delivered += size as u64;
            }
        }
        if data && now_ms > sent_ms {
            // Delivery rate over this packet's round trip (the sampling of BBR's estimator).
            let delivered = self.window.delivered.saturating_sub(delivered_at_send);
            self.note_delivery_rate(now_ms, delivered as f64 / (now_ms - sent_ms) as f64);
        }
        (!late && !resent && eliciting).then_some((sample, sent_ms))
    }

    /// Moves the in-flight window after an acknowledgment (ADR-7995). It grows at once to twice
    /// what one round trip delivered; it shrinks only when [`WINDOW_ROUNDS`] round trips in a row
    /// were held by the window and every one delivered less than half of it.
    fn note_delivery(&mut self, outcome: &AckOutcome) {
        let max = self.window_max();
        if let Some(base) = outcome.delivery_base {
            let sample = usize::try_from(self.window.delivered - base).unwrap_or(usize::MAX);
            self.window.round_best = self.window.round_best.max(sample);
            let grown = sample.saturating_mul(2).min(max);
            if grown > self.window.bytes {
                self.window.bytes = grown;
            }
        }
        if !outcome.round_done {
            return;
        }
        if self.window.round_limited {
            if self.window.limited_rounds.len() >= WINDOW_ROUNDS {
                self.window.limited_rounds.pop_front();
            }
            self.window.limited_rounds.push_back(self.window.round_best);
            if self.window.limited_rounds.len() >= WINDOW_ROUNDS {
                let best = self.window.limited_rounds.iter().copied().max().unwrap_or(0);
                let target = best.saturating_mul(2).clamp(self.window_min(), max);
                if target < self.window.bytes {
                    self.window.bytes = target;
                }
            }
        } else {
            self.window.limited_rounds.clear();
        }
        self.window.round_end_seq = self.local_seq;
        self.window.round_best = 0;
        self.window.round_limited = false;
    }

    /// Feeds one round-trip observation to the estimator. A sample far above the estimate (past
    /// the loss delay) may be the peer's delay, not the path's: a receiver that stalled reports
    /// old packets when it wakes. Such a sample raises the latest round trip at once (timers and
    /// loss detection back off) but is smoothed in only after a packet sent later confirms it, so
    /// a real step in path delay is learned within one round trip and a stall never is.
    fn observe_rtt(&mut self, sample: f64, sent_ms: u64, now_ms: u64) {
        self.rtt_observed = (self.rtt_observed.0 + 1, sample);
        let Some(srtt) = self.srtt else {
            self.rtt_sample(sample);
            return;
        };
        if self.rtt_probe_after_ms.is_some_and(|after| sent_ms < after) {
            // Sent before the doubtful observation: it cannot confirm or refute it.
            self.latest_rtt = self.latest_rtt.max(sample);
            return;
        }
        if sample > srtt.max(self.latest_rtt) * 9.0 / 8.0 {
            self.latest_rtt = sample;
            self.rtt_probe_after_ms = Some(now_ms);
        } else {
            self.rtt_probe_after_ms = None;
            self.rtt_sample(sample);
        }
    }

    fn rtt_sample(&mut self, r: f64) {
        // RFC 6298 § 2.2-2.3 with alpha = 1/8, beta = 1/4.
        self.latest_rtt = r;
        match self.srtt {
            None => {
                self.srtt = Some(r);
                self.rttvar = r / 2.0;
                self.jitter = 0.0;
            }
            Some(srtt) => {
                self.rttvar = 0.75 * self.rttvar + 0.25 * (srtt - r).abs();
                self.jitter = 0.75 * self.jitter + 0.25 * (srtt - r).abs();
                self.srtt = Some(0.875 * srtt + 0.125 * r);
            }
        }
    }

    /// Records one resolved packet and returns the sample's position.
    fn push_loss_sample(&mut self, delivered: bool) -> u64 {
        if self.loss_window.len() >= LOSS_SAMPLES {
            self.loss_window.pop_front();
        }
        self.loss_window.push_back(delivered);
        self.loss_pushed += 1;
        self.loss_pushed - 1
    }

    /// Declares lost every unacknowledged packet older than the newest acknowledged one that is
    /// [`LOSS_PACKET_THRESHOLD`] or more behind it (more on a path that has shown reordering), or
    /// that has been out longer than the loss delay (RFC 9002 § 6.1). Fragments whose latest transmission such a packet carried become
    /// due again. Returns when the oldest packet that cost a fragment its latest transmission
    /// left, `None` when no reliable data was lost.
    ///
    /// The scan starts at `loss_scan_from`, not at the oldest tracked record (ADR-10081): records
    /// stay tracked for late acknowledgments long after their verdict (`LATE_ACK_WINDOW`), and a
    /// verdict is final for this scan, so walking them again on every poll and every
    /// acknowledgment only cost time: about a thousand records per call on any path that ever
    /// lost a packet.
    fn detect_losses(&mut self, now_ms: u64, rto: u64) -> Option<u64> {
        let largest = self.largest_acked?;
        let delay = self.loss_delay_ms(rto);
        let mut newly_lost = Vec::new();
        let mut examined = 0u64;
        // Packet numbers are consecutive along the window (`process_acks` finds a packet by the
        // same offset), so the start is an offset from the front. The search is only a fallback
        // that stays exact should the numbers ever not be consecutive.
        let start = match self.sent.front() {
            None => 0,
            Some(front) => {
                let from = self.loss_scan_from.max(front.seq);
                let offset = usize::try_from(from - front.seq).unwrap_or(usize::MAX);
                if self.sent.get(offset).is_some_and(|p| p.seq == from) {
                    offset
                } else {
                    self.sent.partition_point(|p| p.seq < from)
                }
            }
        };
        // The oldest packet this scan leaves without a verdict, or where the scan ended.
        let mut open_from = None;
        let mut scanned_to = self.loss_scan_from;
        for (index, packet) in self.sent.range(start..).enumerate() {
            let index = start + index;
            if packet.seq >= largest {
                break;
            }
            examined += 1;
            scanned_to = packet.seq.saturating_add(1);
            if packet.acked || packet.lost {
                continue;
            }
            // An MTU probe's own deadline decides (`expire_mtu_probe`), unless the packet
            // threshold already says it is gone.
            if largest - packet.seq >= self.reorder_threshold
                || (!packet.mtu_probe && now_ms.saturating_sub(packet.sent_ms) >= delay)
            {
                newly_lost.push(index);
            } else if open_from.is_none() {
                open_from = Some(packet.seq);
            }
        }
        self.work.loss_scan_records += examined;
        // Every index in `newly_lost` gets its verdict below.
        self.loss_scan_from = self.loss_scan_from.max(open_from.unwrap_or(scanned_to));
        let mut lost_data: Option<u64> = None;
        // A lost MTU probe is no congestion signal (RFC 8899 § 3): the rate controller's round
        // counts data packets only (ADR-8487).
        let lost_data_packets = newly_lost
            .iter()
            .filter(|&&index| self.sent.get(index).is_some_and(|p| !p.mtu_probe))
            .count();
        self.round.lost_packets = self
            .round
            .lost_packets
            .saturating_add(u32::try_from(lost_data_packets).unwrap_or(u32::MAX));
        for index in newly_lost {
            let Some((probe, seq, size)) = self
                .sent
                .get(index)
                .map(|p| (p.mtu_probe, p.seq, usize::from(p.size)))
            else {
                continue;
            };
            if probe {
                if let Some(packet) = self.sent.get_mut(index) {
                    packet.lost = true;
                }
                if self.pmtu.is_outstanding(seq) {
                    self.stats.mtu_probes_lost += 1;
                    self.lost_mtu_probe(seq, now_ms);
                }
                continue;
            }
            if self.pmtu.enabled() {
                let between = self
                    .pmtu
                    .loss_run_first()
                    .is_some_and(|first| self.small_acked_between(first, seq));
                if self.pmtu.packet_lost(size, seq, between, now_ms) {
                    self.stats.mtu_black_holes += 1;
                }
            }
            let sample = self.push_loss_sample(false);
            self.stats.packets_lost += 1;
            let Some(packet) = self.sent.get_mut(index) else {
                continue;
            };
            packet.lost = true;
            packet.loss_sample = sample;
            let (seq, sent_ms) = (packet.seq, packet.sent_ms);
            // Lost credit is sent again (RFC 9000 § 13.3) unless newer credit already left.
            for (stream, max) in &packet.credits {
                if let Some(st) = self.recv_streams.get_mut(usize::from(*stream))
                    && st.acked_advert < *max
                    && st.advertised == *max
                {
                    st.advertised = st.acked_advert;
                }
            }
            let keys = packet.reliable_ids.clone();
            for key in keys {
                if let Some(at) = self.fragment_index(key)
                    && let Some(frag) = self.reliable_out.get_mut(at)
                    && !frag.acked
                    && !frag.lost
                    && frag.last_seq == seq
                {
                    frag.lost = true;
                    frag.losses = frag.losses.saturating_add(1);
                    if frag.in_flight {
                        frag.in_flight = false;
                        self.in_flight_bytes = self.in_flight_bytes.saturating_sub(frag.wire_bytes());
                    }
                    lost_data = Some(lost_data.map_or(sent_ms, |t| t.min(sent_ms)));
                }
            }
        }
        lost_data
    }

    /// True when a data packet of at most the base size numbered strictly between `first` and
    /// `last` was acknowledged (path MTU black-hole detection, ADR-8300). Bounded by the
    /// sent-packet window.
    fn small_acked_between(&self, first: u64, last: u64) -> bool {
        let Some(front) = self.sent.front().map(|p| p.seq) else {
            return false;
        };
        let base = self.config.max_packet_bytes;
        let from = usize::try_from(first.saturating_add(1).saturating_sub(front)).unwrap_or(usize::MAX);
        self.sent
            .iter()
            .skip(from)
            .take_while(|p| p.seq < last)
            .any(|p| p.acked && !p.mtu_probe && !p.path_probe && usize::from(p.size) <= base)
    }

    /// Drops sent-packet records that are resolved for good: acknowledged, or declared lost and
    /// so far behind the newest acknowledgment that no later one can name them.
    fn retire(&mut self) {
        while let Some(front) = self.sent.front() {
            let past_late_acks = front.lost
                && self
                    .largest_acked
                    .is_some_and(|a| a > front.seq && a - front.seq > LATE_ACK_WINDOW);
            if !(front.acked || past_late_acks) {
                break;
            }
            self.sent.pop_front();
        }
    }

    fn roll_rate_window(&mut self, now_ms: u64) {
        let start = *self.rate_window_start.get_or_insert(now_ms);
        let elapsed = now_ms.saturating_sub(start);
        if elapsed >= 1_000 {
            let secs = elapsed as f64 / 1000.0;
            self.stats.send_kbps = self.window_sent as f64 * 8.0 / 1000.0 / secs;
            self.stats.recv_kbps = self.window_recv as f64 * 8.0 / 1000.0 / secs;
            self.data_pps = [self.data_pps[1], (self.window_data_acked as f64 / secs) as usize];
            self.window_sent = 0;
            self.window_recv = 0;
            self.window_data_acked = 0;
            self.rate_window_start = Some(now_ms);
        }
    }

    /// Takes the next delivered message.
    pub fn next_message(&mut self) -> Option<(Channel, Vec<u8>)> {
        self.received.pop_front()
    }

    /// Records examined so far (ADR-10081).
    #[cfg(test)]
    pub(crate) fn work(&self) -> WorkCounters {
        self.work
    }

    /// Counters and estimates.
    pub fn stats(&self) -> LinkStats {
        let mut stats = self.stats;
        stats.rtt_ms = self.srtt.unwrap_or(0.0);
        stats.jitter_ms = self.jitter;
        stats.loss_fraction = if self.loss_window.is_empty() {
            0.0
        } else {
            self.loss_window.iter().filter(|d| !**d).count() as f32 / self.loss_window.len() as f32
        };
        stats.min_rtt_ms = self.min_rtt.front().map_or(0.0, |(_, m)| *m);
        stats.reliable_in_flight = self.reliable_out.len() - self.acked_interior.min(self.reliable_out.len());
        stats.recovering = self.recovery.is_some();
        stats.path_mtu = u32::try_from(self.path_mtu()).unwrap_or(u32::MAX);
        stats.mtu_search = self.pmtu.state();
        stats.queue_delay_ms = self.rate.queue_delay_ms();
        stats.send_allowance_bps = self.send_allowance_bytes_per_second().unwrap_or(u32::MAX);
        stats.rate_target_bps = self.pacing_rate_bytes_per_second().unwrap_or(u32::MAX);
        stats.rate_state = self.rate.state();
        stats.rate_cuts = self.rate.cuts();
        stats.rate_breaker = self.rate.breaker_tripped();
        stats.reliable_buffered_bytes = self.buffered_bytes;
        // As the latest stall check enforced it: before the first round-trip sample the bound of
        // the oldest unanswered packet's wait is part of the deadline (ADR-8658, ADR-8859).
        stats.reassembly_stall_deadline_ms = self.reassembly_stall_deadline_at(self.stall_checked_ms);
        stats.failure = self.failure;
        stats.reliable_assembling_bytes = self.recv_streams.iter().map(|st| st.assembling.len()).sum();
        stats.reassembly_stall_ms = self.recv_streams.iter().map(|st| st.stall_ms).max().unwrap_or(0);
        stats
    }

    /// Takes the receive timestamps the peer reported for this side's packets, oldest first (spec
    /// 120 § 4.1 item 3; empty unless `LinkConfig::receive_timestamps` is set and the peer
    /// supports them). At most [`MAX_RECEIVE_TIMESTAMPS`] wait; older ones are dropped.
    pub fn take_receive_timestamps(&mut self) -> Vec<ReceiveTimestamp> {
        self.receive_timestamps.drain(..).collect()
    }

    /// The number the next packet will carry (62-bit, counts up from 0, never reused).
    pub fn next_packet_number(&self) -> u64 {
        self.local_seq
    }

    /// True when reliable fragments still wait for acknowledgement.
    pub fn has_unacked_reliable(&self) -> bool {
        self.reliable_out.iter().any(|f| !f.acked)
    }
}

/// The error every receive returns once the connection failed.
fn failure_error(failure: LinkFailure) -> WireError {
    match failure {
        LinkFailure::MessageTooLong => {
            WireError::Invalid("connection failed on an oversized reliable message")
        }
        LinkFailure::InboxFull => WireError::Invalid("connection failed on a full reliable inbox"),
        LinkFailure::FragmentTooLong => {
            WireError::Invalid("connection failed on an oversized reliable fragment")
        }
        LinkFailure::ReassemblyOverCap => WireError::Invalid("connection failed on its reassembly cap"),
        LinkFailure::ReassemblyStalled => WireError::Invalid("reassembly stalled"),
        LinkFailure::StreamOverCap => WireError::Invalid("connection failed on a stream's window"),
        LinkFailure::UnknownStream => WireError::Invalid("connection failed on an undeclared stream"),
    }
}

enum Arrival {
    Fresh,
    Duplicate,
    Stale,
    Rejected,
}

enum Record<'a> {
    Ping,
    Unreliable(&'a [u8]),
    Reliable {
        stream: u8,
        id: u16,
        first: bool,
        more: bool,
        bytes: &'a [u8],
    },
    Credit {
        stream: u8,
        max: u64,
    },
}

/// Writes one reliable fragment record (stream 2 in its short form).
fn write_fragment(body: &mut WireWriter, frag: &OutFragment) {
    let tag = if frag.stream == StreamId::GAME_RPC.0 {
        RECORD_RELIABLE
    } else {
        RECORD_STREAM | frag.stream | if frag.first { RECORD_FIRST } else { 0 }
    };
    body.u8(if frag.more { tag | RECORD_MORE } else { tag });
    body.u16(frag.id);
    body.u16(frag.bytes.len() as u16);
    body.raw(&frag.bytes);
}

/// Reliable records in a packet body, for tests (0 for a malformed body).
#[cfg(test)]
fn test_reliable_records(body: &[u8]) -> usize {
    let mut r = WireReader::new(body);
    let Ok(flags) = r.u8() else {
        return 0;
    };
    let pn_len = [1, 2, 4, 4][usize::from(flags & FLAG_PN_LEN)];
    if r.take(pn_len).is_err() {
        return 0;
    }
    if flags & FLAG_ACK != 0 && AckFrame::decode(&mut r, flags & FLAG_ACK_TIMESTAMPS != 0).is_err() {
        return 0;
    }
    parse_records(&mut r).map_or(0, |records| {
        records
            .iter()
            .filter(|rec| matches!(rec, Record::Reliable { .. }))
            .count()
    })
}

/// The (truncated) packet number of a packet body, for tests that check numbering.
#[cfg(test)]
fn test_packet_number(body: &[u8]) -> u64 {
    match body.first().map(|f| f & FLAG_PN_LEN) {
        Some(0) => u64::from(body[1]),
        Some(1) => u64::from(u16::from_le_bytes([body[1], body[2]])),
        _ => u64::from(u32::from_le_bytes([body[1], body[2], body[3], body[4]])),
    }
}

/// A packet body parsed but not applied: flags, packet-number length, truncated packet number,
/// the ack frame if any, and the records.
type ParsedPacket<'a> = (u8, usize, u64, Option<AckFrame>, Vec<Record<'a>>);

/// Parses a packet body without touching any connection state (`Connection::receive_inner`
/// applies it only when the whole body parses).
fn parse_packet(body: &[u8]) -> Result<ParsedPacket<'_>, WireError> {
    let mut r = WireReader::new(body);
    let flags = r.u8()?;
    if flags & !FLAG_KNOWN != 0 {
        return Err(WireError::Invalid("unknown packet flags"));
    }
    let pn_len = match flags & FLAG_PN_LEN {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => return Err(WireError::Invalid("bad packet number length")),
    };
    let truncated = match pn_len {
        1 => u64::from(r.u8()?),
        2 => u64::from(r.u16()?),
        _ => u64::from(r.u32()?),
    };
    let ack = if flags & FLAG_ACK != 0 {
        Some(AckFrame::decode(&mut r, flags & FLAG_ACK_TIMESTAMPS != 0)?)
    } else {
        None
    };
    let records = parse_records(&mut r)?;
    Ok((flags, pn_len, truncated, ack, records))
}

fn parse_records<'a>(r: &mut WireReader<'a>) -> Result<Vec<Record<'a>>, WireError> {
    let mut out = Vec::new();
    while !r.is_empty() {
        if out.len() >= MAX_RECORDS_PER_PACKET {
            return Err(WireError::Invalid("too many records in one packet"));
        }
        let tag = r.u8()?;
        match tag {
            RECORD_PING => out.push(Record::Ping),
            RECORD_PADDING => {
                // Padding runs to the end of the packet and is all zeros (ADR-8300).
                let pad = r.take(r.remaining())?;
                if pad.iter().any(|b| *b != 0) {
                    return Err(WireError::Invalid("nonzero padding"));
                }
                break;
            }
            RECORD_UNRELIABLE => {
                let len = usize::from(r.u16()?);
                out.push(Record::Unreliable(r.take(len)?));
            }
            RECORD_CREDIT => {
                let stream = r.u8()?;
                let max = crate::ack::read_varint(r)?;
                out.push(Record::Credit { stream, max });
            }
            t if t & !RECORD_MORE == RECORD_RELIABLE || t & RECORD_STREAM != 0 => {
                let (stream, first) = if t & RECORD_STREAM != 0 {
                    (t & RECORD_STREAM_ID, t & RECORD_FIRST != 0)
                } else {
                    (StreamId::GAME_RPC.0, false)
                };
                let id = r.u16()?;
                let len = usize::from(r.u16()?);
                out.push(Record::Reliable {
                    stream,
                    id,
                    first,
                    more: t & RECORD_MORE != 0,
                    bytes: r.take(len)?,
                });
            }
            _ => return Err(WireError::Invalid("unknown record tag")),
        }
    }
    Ok(out)
}

/// The default link with the first flight named off (ADR-10156; off by default in v0.7,
/// ADR-10231, and a later release may turn it on): the transport's tests of the initial window
/// (the 64-packet first burst, the acknowledgment trains it makes, the window that follows
/// delivery) build their connections on the blind start they test, whatever the default is.
/// The first flight has its own tests
/// (`uplink_cap_tests::a_first_flight_sends_eight_packets_and_then_the_floor_until_the_path_answers`).
#[cfg(test)]
pub(crate) fn blind_link() -> LinkConfig {
    let mut config = LinkConfig::default();
    config.rate.first_flight = false;
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &[u8] = &[0xAA, 0xBB];

    /// Moves every packet `from` produces into `to`, dropping those `drop` selects (by packet index).
    fn pump(
        from: &mut Connection,
        to: &mut Connection,
        now: u64,
        drop: &mut dyn FnMut(usize) -> bool,
    ) -> usize {
        let packets = from.poll(now, H);
        let n = packets.len();
        for (i, p) in packets.into_iter().enumerate() {
            if !drop(i) {
                to.receive(&p[H.len()..], now).expect("valid packet");
            }
        }
        n
    }

    /// A forged wire-5 body: packet number `pn` sent in four bytes, an acknowledgment frame of
    /// `ack` (largest, ranges) when given, then `records`.
    fn forged(pn: u64, ack: Option<(u64, Vec<(u64, u64)>)>, records: &[u8]) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.u8(2 | if ack.is_some() { FLAG_ACK } else { 0 });
        w.u32(pn as u32);
        if let Some((largest, ranges)) = ack {
            AckFrame {
                largest,
                delay_us: 0,
                arrival_ms: 0,
                ranges,
                timestamps: Vec::new(),
            }
            .encode(&mut w, false, usize::MAX);
        }
        w.raw(records);
        w.finish()
    }

    fn drain(c: &mut Connection) -> Vec<(Channel, Vec<u8>)> {
        std::iter::from_fn(|| c.next_message()).collect()
    }

    #[test]
    fn interruption_probes_back_off_and_restore_the_normal_keepalive() {
        let mut a = Connection::new(LinkConfig::default());
        a.poll(0, &[]);
        a.set_interrupted(true, 0);
        // Six probes fit by 13 s even at the maximum positive jitter on the first rounds.
        let mut times = Vec::new();
        for now in (10..=13_000).step_by(10) {
            if !a.poll(now, &[]).is_empty() {
                times.push(now);
            }
        }
        assert_eq!(times.len(), 6, "{times:?}");
        assert_eq!(times[0], 250);
        for (i, pair) in times.windows(2).enumerate() {
            let base = (500_u64 << i).min(4_000);
            assert!(
                (base..=base.saturating_add(base / 8 + 10).min(4_010)).contains(&(pair[1] - pair[0])),
                "{times:?}"
            );
        }
        a.set_interrupted(false, 13_000);
        assert_eq!(a.poll(13_100, &[]).len(), 1);
    }

    /// From the ack-poll lane (ADR-7949, ADR-7988), on wire 5: a full burst whose packet
    /// numbers cross 2^16 (where the old 16-bit sequence wrapped, and a 2-byte truncation turns
    /// over) is delivered and acknowledged with no loss and no resend.
    #[test]
    fn the_ack_horizon_survives_sequence_wrap() {
        let mut a = Connection::new(blind_link());
        let mut b = Connection::new(blind_link());
        a.local_seq = u64::from(u16::MAX) - 15;
        a.window.round_end_seq = a.local_seq;
        for _ in 0..64 {
            a.send(Channel::GAME_RPC, vec![7; a.fragment_bytes(H.len())], H.len())
                .expect("queued");
        }
        for now in 0..4 {
            pump(&mut a, &mut b, now, &mut |_| false);
            pump(&mut b, &mut a, now, &mut |_| false);
        }
        assert_eq!(drain(&mut b).len(), 64);
        assert!(!a.has_unacked_reliable());
        assert_eq!(a.stats().packets_lost, 0);
        assert_eq!(a.stats().reliable_resends, 0);
    }

    #[test]
    fn seq_greater_wraps() {
        assert!(seq_greater(1, 0));
        assert!(seq_greater(0, 65535));
        assert!(!seq_greater(65535, 0));
        assert!(!seq_greater(5, 5));
    }

    #[test]
    fn reliable_messages_arrive_in_order_under_heavy_loss() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        for i in 0..200u32 {
            a.send(Channel::GAME_RPC, i.to_le_bytes().to_vec(), H.len())
                .expect("queued");
        }
        let mut rng = 12345u64;
        let mut lossy = move |_| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % 10 < 4 // 40 % loss each way
        };
        let mut got = Vec::new();
        for step in 0..400u64 {
            let now = step * 50;
            pump(&mut a, &mut b, now, &mut lossy);
            pump(&mut b, &mut a, now, &mut lossy);
            got.extend(drain(&mut b));
        }
        let expect: Vec<(Channel, Vec<u8>)> = (0..200u32)
            .map(|i| (Channel::GAME_RPC, i.to_le_bytes().to_vec()))
            .collect();
        assert_eq!(got, expect, "every reliable message once, in order");
        assert!(!a.has_unacked_reliable(), "everything acknowledged");
        assert!(a.stats().reliable_resends > 0, "loss forced resends");
        assert!(a.stats().packets_lost > 0, "loss was detected");
    }

    #[test]
    fn large_reliable_message_is_fragmented_and_reassembled() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let big: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        a.send(Channel::GAME_RPC, big.clone(), H.len()).expect("queued");
        a.send(Channel::GAME_RPC, b"after".to_vec(), H.len())
            .expect("queued");
        for step in 0..20u64 {
            pump(&mut a, &mut b, step * 10, &mut |_| false);
            pump(&mut b, &mut a, step * 10, &mut |_| false);
        }
        let got = drain(&mut b);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].1, big);
        assert_eq!(got[1].1, b"after".to_vec());
    }

    #[test]
    fn unreliable_is_not_resent_and_size_is_bounded() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let max = a.max_unreliable_bytes(H.len());
        assert!(matches!(
            a.send(Channel::Unreliable, vec![0; max + 1], H.len()),
            Err(SendError::TooLarge { .. })
        ));
        a.send(Channel::Unreliable, vec![7; max], H.len())
            .expect("fits exactly");
        // First poll's packet is lost.
        pump(&mut a, &mut b, 0, &mut |_| true);
        for step in 1..10u64 {
            pump(&mut a, &mut b, step * 200, &mut |_| false);
        }
        assert!(drain(&mut b).is_empty(), "a lost unreliable message is gone");
    }

    #[test]
    fn duplicate_packets_are_ignored() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        a.send(Channel::Unreliable, b"once".to_vec(), H.len())
            .expect("queued");
        let packets = a.poll(0, H);
        assert_eq!(packets.len(), 1);
        b.receive(&packets[0][H.len()..], 0).expect("valid");
        b.receive(&packets[0][H.len()..], 0).expect("valid");
        assert_eq!(drain(&mut b).len(), 1);
        assert_eq!(b.stats().duplicates, 1);
    }

    #[test]
    fn rtt_converges_to_the_link_delay() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        // a -> b at t, b answers at t + 40, a hears at t + 80: an 80 ms round trip.
        for step in 0..50u64 {
            let t = step * 100;
            for p in a.poll(t, H) {
                b.receive(&p[H.len()..], t + 40).expect("valid");
            }
            for p in b.poll(t + 40, H) {
                a.receive(&p[H.len()..], t + 80).expect("valid");
            }
        }
        let rtt = a.stats().rtt_ms;
        assert!((75.0..=85.0).contains(&rtt), "rtt {rtt}");
        assert!(a.resend_timeout_ms() >= 100);
    }

    /// Spec 120 § 9.4 (ADR-8219): the reported jitter comes from the samples. A steady 200 ms
    /// round trip reports 0 jitter from its first sample on (RFC 6298's `RTTVAR` starts at R / 2,
    /// 100 ms, which put the margin target at its clamp), while the resend timeout keeps RFC
    /// 6298's conservative start.
    #[test]
    fn jitter_is_seeded_from_samples_not_half_the_round_trip() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let mut first = None;
        for step in 0..20u64 {
            let t = step * 300;
            for p in a.poll(t, H) {
                b.receive(&p[H.len()..], t + 100).expect("valid");
            }
            for p in b.poll(t + 100, H) {
                a.receive(&p[H.len()..], t + 200).expect("valid");
            }
            if first.is_none() && a.stats().rtt_ms > 0.0 {
                first = Some((a.stats().jitter_ms, a.resend_timeout_ms()));
            }
        }
        let (jitter, rto) = first.expect("a sample");
        assert_eq!(jitter, 0.0, "first-sample jitter");
        assert!(rto > 200, "RFC 6298 start: SRTT + 4 x R/2 (clamped): {rto}");
        assert_eq!(a.stats().jitter_ms, 0.0, "a steady path has no jitter");
    }

    #[test]
    fn backpressure_refuses_past_the_window() {
        let mut a = Connection::new(LinkConfig {
            max_reliable_in_flight: 4,
            ..LinkConfig::default()
        });
        for _ in 0..4 {
            a.send(Channel::GAME_RPC, vec![1], H.len()).expect("room");
        }
        assert!(matches!(
            a.send(Channel::GAME_RPC, vec![1], H.len()),
            Err(SendError::Backpressure(4))
        ));
    }

    /// Audit H1 (muse2) / M9 (grok): a datagram of tiny records is refused whole, before any
    /// state changes, and the sender never builds one.
    #[test]
    fn a_packet_of_too_many_records_is_refused_and_never_sent() {
        let mut hostile = forged(0, None, &[]);
        for _ in 0..=MAX_RECORDS_PER_PACKET {
            hostile.extend_from_slice(&[RECORD_UNRELIABLE, 0, 0]);
        }
        let mut c = Connection::new(LinkConfig::default());
        assert!(c.receive(&hostile, 0).is_err());
        assert!(c.next_message().is_none());
        assert_eq!(c.stats().packets_received, 0, "all or nothing");

        // Raw transport throughput: more reliable messages per second than a class cap allows
        // (spec 120 § 4.5 A2, ADR-8407), so the caps are off.
        let mut a = Connection::new(LinkConfig {
            message_caps: false,
            ..LinkConfig::default()
        });
        let mut b = Connection::new(LinkConfig::default());
        let count = MAX_RECORDS_PER_PACKET * 3 + 7;
        for _ in 0..count {
            a.send(Channel::Unreliable, Vec::new(), H.len()).expect("fits");
            a.send(Channel::GAME_RPC, Vec::new(), H.len()).expect("fits");
        }
        let packets = a.poll(0, H);
        assert!(packets.len() > 6, "split over packets: {}", packets.len());
        for p in &packets {
            b.receive(&p[H.len()..], 0)
                .expect("a correct peer is never refused");
        }
        let got = drain(&mut b);
        let unreliable = got.iter().filter(|(c, _)| *c == Channel::Unreliable).count();
        assert_eq!(got.len() - unreliable, count, "every reliable record arrived");
        assert_eq!(unreliable, count);
    }

    /// Audit M3 (grok) / M2 (muse2), on wire 5: a packet number is never reused, so an old packet
    /// cannot name a new one; what is left to refuse is an acknowledgment of a packet never sent
    /// (it must not mark queued reliable data delivered) and a replayed packet (a duplicate).
    #[test]
    fn stale_packets_do_not_acknowledge_anything() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        a.send(Channel::GAME_RPC, b"keep".to_vec(), H.len())
            .expect("queued");
        let packets = a.poll(0, H);
        assert_eq!(packets.len(), 1);
        // An acknowledgment of packets 0..=5 when only packet 0 was sent: ignored as a whole.
        a.receive(&forged(100, Some((5, vec![(0, 5)])), &[]), 1)
            .expect("valid");
        assert!(a.has_unacked_reliable(), "the forged ack was ignored");
        assert_eq!(a.stats().packets_acked, 0);
        // A replay of an earlier packet is a duplicate and acknowledges nothing either.
        let genuine = forged(101, Some((0, vec![(0, 0)])), &[]);
        a.receive(&forged(101, None, &[]), 2).expect("valid");
        a.receive(&genuine, 3).expect("valid");
        assert_eq!(a.stats().duplicates, 1);
        assert!(a.has_unacked_reliable(), "the replay was ignored");
        // The same acknowledgment on a fresh packet counts.
        a.receive(&forged(102, Some((0, vec![(0, 0)])), &[]), 4)
            .expect("valid");
        assert!(!a.has_unacked_reliable());
        b.receive(&packets[0][H.len()..], 4).expect("valid");
        assert_eq!(drain(&mut b).len(), 1);
    }

    /// A hostile acknowledgment frame naming a range of 2^40 packets costs no more than the
    /// sent-packet window: only packets still tracked are visited (without the bound this test
    /// would walk a trillion numbers).
    #[test]
    fn a_huge_acknowledged_range_is_bounded_by_the_sent_window() {
        let mut a = Connection::new(LinkConfig::default());
        a.local_seq = 1 << 40;
        a.send(Channel::GAME_RPC, b"x".to_vec(), 0).expect("queued");
        assert_eq!(a.poll(0, &[]).len(), 1);
        let top = (1u64 << 40) + 1;
        a.receive(&forged(7, Some((top - 1, vec![(0, top - 1)])), &[]), 1)
            .expect("valid");
        assert_eq!(a.stats().packets_acked, 1);
        assert!(!a.has_unacked_reliable());
        // A frame whose largest was never sent is ignored whole.
        a.receive(&forged(8, Some((top + 5, vec![(0, top + 5)])), &[]), 2)
            .expect("valid");
        assert_eq!(a.stats().packets_acked, 1);
    }

    /// A 2-byte packet number that arrives after 300 newer ones still decodes to itself (with
    /// RFC 9000's 1-byte minimum it would decode as a newer packet and acknowledge that one).
    #[test]
    fn a_long_delayed_packet_decodes_to_its_own_number() {
        // No probe repeats the message while the first copy is held back.
        let mut a = Connection::new(LinkConfig {
            resend_min_ms: 10_000,
            resend_max_ms: 10_000,
            ..LinkConfig::default()
        });
        let mut b = Connection::new(LinkConfig::default());
        // Larger than a redundant copy takes, so only the held packet carries it.
        a.send(Channel::GAME_RPC, vec![7; 300], 0).expect("queued");
        let late = a.poll(0, &[]);
        for now in 1..=300u64 {
            a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            for p in a.poll(now, &[]) {
                b.receive(&p, now).expect("valid packet");
            }
        }
        while b.next_message().is_some() {}
        b.receive(&late[0], 301).expect("valid");
        assert_eq!(b.next_message(), Some((Channel::GAME_RPC, vec![7; 300])));
        assert!(b.recv.contains(0), "recorded as packet 0");
        assert_eq!(b.recv.largest(), Some(300));
    }

    /// Audit L3 (grok): the receive side cannot be configured into unbounded reassembly.
    #[test]
    fn the_reliable_message_limit_is_clamped() {
        let c = Connection::new(LinkConfig {
            max_reliable_message_bytes: usize::MAX,
            ..LinkConfig::default()
        });
        assert_eq!(c.config().max_reliable_message_bytes, MAX_RELIABLE_MESSAGE_LIMIT);
    }

    /// A consumer that never drains cannot grow the delivered queue past its bound.
    #[test]
    fn the_delivered_queue_is_bounded() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let mut now = 0;
        while (b.received.len()) < MAX_RECEIVED_MESSAGES {
            for _ in 0..64 {
                a.send(Channel::Unreliable, vec![1], H.len()).expect("fits");
            }
            now += 1;
            for p in a.poll(now, H) {
                b.receive(&p[H.len()..], now).expect("valid");
            }
            pump(&mut b, &mut a, now, &mut |_| false);
        }
        a.send(Channel::Unreliable, vec![1], H.len()).expect("fits");
        now += 1;
        for p in a.poll(now, H) {
            b.receive(&p[H.len()..], now).expect("valid");
        }
        assert_eq!(b.received.len(), MAX_RECEIVED_MESSAGES);
        assert_eq!(b.stats().inbound_dropped, 1);
        a.send(Channel::GAME_RPC, vec![2], H.len()).expect("fits");
        now += 1;
        let packets = a.poll(now, H);
        assert!(matches!(
            b.receive(&packets[0][H.len()..], now),
            Err(WireError::TooLong { .. })
        ));
    }

    #[test]
    fn corrupt_bodies_are_errors_not_panics() {
        let mut b = Connection::new(LinkConfig::default());
        assert!(b.receive(&[], 0).is_err());
        assert!(b.receive(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0x55], 0).is_err());
        assert!(
            b.receive(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01, 0xFF, 0xFF], 0)
                .is_err()
        );
        assert_eq!(b.stats().packets_received, 0, "a rejected packet changes nothing");
    }

    #[test]
    fn mutated_packets_never_panic_or_break_the_stream() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        for i in 0..50u32 {
            a.send(
                Channel::GAME_RPC,
                vec![i as u8; (i as usize * 97) % 3000],
                H.len(),
            )
            .expect("queued");
            a.send(Channel::Unreliable, vec![1, 2, 3], H.len())
                .expect("queued");
        }
        let mut rng = 0xC0FF_EE00_u64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let mut victim = Connection::new(LinkConfig::default());
        for step in 0..40u64 {
            for p in a.poll(step * 20, H) {
                let body = &p[H.len()..];
                // Feed the clean packet to `b` and up to four mutations to `victim`.
                b.receive(body, step * 20).expect("valid");
                for _ in 0..4 {
                    let mut m = body.to_vec();
                    match next() % 4 {
                        0 => m.truncate((next() as usize) % (m.len() + 1)),
                        1 => {
                            let i = (next() as usize) % m.len().max(1);
                            if let Some(byte) = m.get_mut(i) {
                                *byte ^= 1 << (next() % 8);
                            }
                        }
                        2 => m.extend((0..(next() % 16)).map(|_| next() as u8)),
                        _ => {
                            let i = (next() as usize) % m.len().max(1);
                            if i + 2 <= m.len() {
                                m[i..i + 2].copy_from_slice(&[0xFF, 0xFF]);
                            }
                        }
                    }
                    let _ = victim.receive(&m, step * 20);
                }
            }
            for p in b.poll(step * 20, H) {
                a.receive(&p[H.len()..], step * 20).expect("valid");
            }
        }
        let reliable = std::iter::from_fn(|| b.next_message())
            .filter(|(c, _)| *c == Channel::GAME_RPC)
            .count();
        assert_eq!(reliable, 50, "the clean stream is unaffected");
    }

    #[test]
    fn mismatched_window_settings_cannot_wedge_the_stream() {
        // The sender asks for a window far above the protocol window; it is clamped, so the
        // receiver (default settings) never drops an id it still has to deliver.
        // Raw transport throughput: more reliable messages per second than a class cap allows
        // (spec 120 § 4.5 A2, ADR-8407), so the caps are off.
        let mut a = Connection::new(LinkConfig {
            max_reliable_in_flight: 40_000,
            message_caps: false,
            ..LinkConfig::default()
        });
        let mut b = Connection::new(LinkConfig::default());
        assert_eq!(a.config().max_reliable_in_flight, RELIABLE_WINDOW);
        let mut sent = 0u32;
        let mut got = 0u32;
        for step in 0..400u64 {
            while sent < 70_000
                && a.send(Channel::GAME_RPC, sent.to_le_bytes().to_vec(), H.len())
                    .is_ok()
            {
                sent += 1;
            }
            pump(&mut a, &mut b, step * 20, &mut |_| false);
            pump(&mut b, &mut a, step * 20, &mut |_| false);
            for (_, m) in drain(&mut b) {
                assert_eq!(m, got.to_le_bytes().to_vec(), "in order");
                got += 1;
            }
        }
        assert_eq!(
            got, 70_000,
            "every message delivered, past the window and past the 16-bit id wrap"
        );
    }

    #[test]
    fn a_round_trip_longer_than_the_loss_timeout_still_delivers() {
        // One-way 1250 ms: every packet is "lost" by the timeout before its ack can come back.
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let mut to_b: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        let mut to_a: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        for i in 0..20u32 {
            a.send(Channel::GAME_RPC, i.to_le_bytes().to_vec(), H.len())
                .expect("queued");
        }
        let mut got = 0;
        for step in 0..1_000u64 {
            let now = step * 20;
            for p in a.poll(now, H) {
                to_b.push_back((now + 1250, p));
            }
            for p in b.poll(now, H) {
                to_a.push_back((now + 1250, p));
            }
            while to_b.front().is_some_and(|(t, _)| *t <= now) {
                if let Some((_, p)) = to_b.pop_front() {
                    b.receive(&p[H.len()..], now).expect("valid");
                }
            }
            while to_a.front().is_some_and(|(t, _)| *t <= now) {
                if let Some((_, p)) = to_a.pop_front() {
                    a.receive(&p[H.len()..], now).expect("valid");
                }
            }
            got += drain(&mut b).len();
        }
        assert_eq!(got, 20);
        assert!(
            !a.has_unacked_reliable(),
            "late acks still acknowledge the fragments"
        );
        assert!(
            (2_400.0..2_700.0).contains(&a.stats().rtt_ms),
            "rtt {}",
            a.stats().rtt_ms
        );
    }

    #[test]
    fn one_far_ahead_packet_does_not_wedge_the_link() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        pump(&mut a, &mut b, 0, &mut |_| false);
        // A forged packet 20000 ahead, with a forged acknowledgment.
        let forged = forged(20_000, Some((0, vec![(0, 0)])), &[]);
        b.receive(&forged, 1).expect("well formed");
        assert_eq!(b.stats().out_of_window, 1);
        for i in 0..10u32 {
            a.send(Channel::GAME_RPC, i.to_le_bytes().to_vec(), H.len())
                .expect("queued");
        }
        for step in 1..20u64 {
            pump(&mut a, &mut b, step * 20, &mut |_| false);
            pump(&mut b, &mut a, step * 20, &mut |_| false);
        }
        assert_eq!(drain(&mut b).len(), 10);
        assert!(!a.has_unacked_reliable());
    }

    #[test]
    fn a_genuine_long_outage_resyncs() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        pump(&mut a, &mut b, 0, &mut |_| false);
        // 2000 packets lost in a row.
        for step in 1..=2_000u64 {
            a.send(Channel::Unreliable, vec![1], H.len()).expect("queued");
            pump(&mut a, &mut b, step, &mut |_| true);
        }
        for step in 0..20u64 {
            a.send(Channel::Unreliable, vec![2], H.len()).expect("queued");
            pump(&mut a, &mut b, 3_000 + step, &mut |_| false);
        }
        assert_eq!(b.stats().out_of_window, u64::from(RESYNC_AFTER - 1));
        assert!(
            !drain(&mut b).is_empty(),
            "the receiver moved to the new position"
        );
    }

    #[test]
    fn oversized_reassembly_fails_the_connection_for_good() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig {
            max_reliable_message_bytes: 2_000,
            ..LinkConfig::default()
        });
        a.send(Channel::GAME_RPC, vec![7; 5_000], H.len())
            .expect("queued");
        let packets = a.poll(0, H);
        let results: Vec<bool> = packets
            .iter()
            .map(|p| b.receive(&p[H.len()..], 0).is_ok())
            .collect();
        assert!(results.contains(&false), "the oversized message is refused");
        a.send(Channel::GAME_RPC, vec![1], H.len()).expect("queued");
        for p in a.poll(500, H) {
            assert!(
                b.receive(&p[H.len()..], 500).is_err(),
                "and the connection stays failed"
            );
        }
    }

    #[test]
    fn hostile_settings_are_clamped() {
        let c = Connection::new(LinkConfig {
            max_packet_bytes: 1 << 20,
            max_packets_per_poll: 0,
            max_reliable_in_flight: 0,
            resend_min_ms: 0,
            ..LinkConfig::default()
        });
        assert_eq!(c.config().max_packet_bytes, crate::socket::MAX_DATAGRAM_BYTES);
        assert_eq!(c.config().max_packets_per_poll, 1);
        assert_eq!(c.config().max_reliable_in_flight, 1);
        let mut c = c;
        assert!(
            matches!(
                c.send(Channel::GAME_RPC, vec![0; 10_000], H.len()),
                Err(SendError::TooLarge { .. })
            ),
            "a message needing more fragments than the window is too large, not backpressure"
        );
    }

    #[test]
    fn idle_connection_sends_keepalives_only_when_due() {
        let mut a = Connection::new(LinkConfig::default());
        assert_eq!(a.poll(0, H).len(), 1, "first poll announces the flow");
        assert!(
            a.poll(50, H).is_empty(),
            "nothing due before the keepalive interval"
        );
        assert_eq!(a.poll(100, H).len(), 1);
    }

    /// A reliable body that already fits in one fragment is that fragment: the send does not
    /// allocate a second copy of the bytes.
    #[test]
    fn one_fragment_reliable_send_keeps_the_body_allocation() {
        let mut c = Connection::new(LinkConfig::default());
        // A measured short path cuts full fragments (ADR-10189); the half cut of an unmeasured
        // path is `the_half_cut_keeps_the_body_allocation`.
        c.srtt = Some(20.0);
        c.latest_rtt = 20.0;
        let mut body = vec![0x5Au8; 64];
        body.shrink_to_fit();
        let ptr = body.as_ptr();
        c.send(Channel::GAME_RPC, body, H.len()).expect("queued");
        assert_eq!(c.reliable_out.len(), 1);
        assert!(!c.reliable_out[0].more);
        assert_eq!(c.reliable_out[0].bytes, vec![0x5A; 64]);
        assert_eq!(
            c.reliable_out[0].bytes.as_ptr(),
            ptr,
            "the fragment is the caller's buffer"
        );

        let frag = c.fragment_bytes(H.len());
        let mut long = vec![0x3Cu8; frag * 2 + 3];
        long.shrink_to_fit();
        let long_ptr = long.as_ptr();
        c.send(Channel::GAME_RPC, long, H.len()).expect("queued");
        assert_eq!(c.reliable_out.len(), 4, "one short fragment plus three long ones");
        let parts: Vec<&[u8]> = c
            .reliable_out
            .iter()
            .skip(1)
            .map(|f| f.bytes.as_slice())
            .collect();
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= frag));
        assert_eq!(parts[0].len(), frag);
        assert_eq!(parts[1].len(), frag);
        assert_eq!(parts[2].len(), 3);
        assert!(c.reliable_out[1].more && c.reliable_out[2].more && !c.reliable_out[3].more);
        assert_eq!(
            c.reliable_out[1].bytes.as_ptr(),
            long_ptr,
            "the first fragment of a long body is the caller's buffer"
        );
        let mut joined = Vec::new();
        joined.extend_from_slice(parts[0]);
        joined.extend_from_slice(parts[1]);
        joined.extend_from_slice(parts[2]);
        assert_eq!(joined, vec![0x3Cu8; frag * 2 + 3]);
    }

    /// The same on a path that is not measured yet, where a long body is cut into half fragments
    /// (ADR-10189): the first half fragment is the caller's buffer, every other one holds only
    /// itself, and the parts join to the body.
    #[test]
    fn the_half_cut_keeps_the_body_allocation() {
        let mut c = Connection::new(LinkConfig::default());
        assert!(c.cuts_small_fragments(), "unmeasured");
        let frag = c.fragment_bytes(H.len());
        let small = c.small_fragment_bytes(H.len());
        let len = frag * 2 + 3;
        let mut long: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        long.shrink_to_fit();
        let body = long.clone();
        let long_ptr = long.as_ptr();
        c.send(Channel::GAME_RPC, long, H.len()).expect("queued");
        let count = len.div_ceil(small);
        assert_eq!(count, 5, "four half fragments and the rest");
        assert_eq!(c.reliable_out.len(), count);
        assert!(c.reliable_out.iter().all(|f| f.small_cut));
        assert!(
            c.reliable_out
                .iter()
                .take(count - 1)
                .all(|f| f.bytes.len() == small && f.more)
        );
        assert_eq!(c.reliable_out[count - 1].bytes.len(), len - (count - 1) * small);
        assert!(!c.reliable_out[count - 1].more);
        assert!(c.reliable_out[0].first && c.reliable_out.iter().skip(1).all(|f| !f.first));
        assert_eq!(
            c.reliable_out[0].bytes.as_ptr(),
            long_ptr,
            "the first fragment of a long body is the caller's buffer"
        );
        let held: usize = c.reliable_out.iter().skip(1).map(|f| f.bytes.capacity()).sum();
        assert!(
            held <= len,
            "every tail fragment holds itself: {held} bytes for {len}"
        );
        let joined: Vec<u8> = c
            .reliable_out
            .iter()
            .flat_map(|f| f.bytes.iter().copied())
            .collect();
        assert_eq!(joined, body);
    }

    /// corr-net: a many-fragment reliable message holds about its own size in memory while it
    /// waits for acks. Repeated `split_off` left every head fragment with the capacity of the
    /// whole remaining tail, so the queued capacity grew with the square of the fragment count
    /// (about 29 MiB for one 256 KiB message at the default packet size).
    #[test]
    fn corr_net_many_fragment_send_holds_linear_memory() {
        let mut c = Connection::new(LinkConfig::default());
        let frag = c.fragment_bytes(H.len());
        let fragments = 200usize;
        let len = frag * fragments - 7;
        let body: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        c.send(Channel::GAME_RPC, body.clone(), H.len()).expect("queued");
        assert_eq!(c.reliable_out.len(), fragments);
        let held: usize = c.reliable_out.iter().map(|f| f.bytes.capacity()).sum();
        // The first fragment keeps the caller's allocation (`len`); every tail holds itself.
        assert!(
            held <= 2 * len + fragments * 16,
            "{held} bytes of capacity queued for a {len}-byte message"
        );
        assert!(c.reliable_out.iter().all(|f| f.bytes.len() <= frag));
        let flags: Vec<bool> = c.reliable_out.iter().map(|f| f.more).collect();
        assert!(flags[..fragments - 1].iter().all(|m| *m) && !flags[fragments - 1]);
        let joined: Vec<u8> = c
            .reliable_out
            .iter()
            .flat_map(|f| f.bytes.iter().copied())
            .collect();
        assert_eq!(joined, body);
    }

    /// A packet body numbered `seq` carrying one reliable fragment (`more` = more follow).
    fn reliable_body(seq: u16, id: u16, more: bool, len: usize) -> Vec<u8> {
        let mut w = WireWriter::new();
        // Wire 5 (ADR-8184): flags (a 2-byte packet number, no ack frame), then the number.
        w.u8(1);
        w.u16(seq);
        w.u8(if more {
            RECORD_RELIABLE | RECORD_MORE
        } else {
            RECORD_RELIABLE
        });
        w.u16(id);
        w.u16(len as u16);
        w.raw(&vec![9; len]);
        w.finish()
    }

    /// ADR-7981: a packet that would store past a shared budget is refused whole (unacked, as if
    /// lost) without failing the connection, while one that fills the gap always gets in and
    /// frees what it lets drain.
    #[test]
    fn a_full_budget_refuses_out_of_order_storage_but_never_the_gap() {
        let mut budget = ReassemblyBudget::new(2_500);
        let mut c = Connection::new(LinkConfig::default());
        for id in 1..=2 {
            c.receive_within(&reliable_body(id, id, true, 1_000), 0, &mut budget)
                .expect("stored");
        }
        assert_eq!((budget.used(), c.reliable_buffered_bytes()), (2_000, 2_000));
        let received = c.stats().packets_received;
        // A third fragment ahead of the gap would pass the budget: refused, nothing changes.
        c.receive_within(&reliable_body(3, 3, true, 1_000), 1, &mut budget)
            .expect("a refusal is not an error");
        assert_eq!(c.stats().reassembly_refused, 1);
        assert_eq!(
            c.stats().packets_received,
            received,
            "as if lost: not counted, not acked"
        );
        assert_eq!(budget.used(), 2_000);
        assert!(c.failure().is_none());
        // The gap (id 0) gets in even with the budget full, and drains everything.
        c.receive_within(&reliable_body(4, 0, true, 1_000), 2, &mut budget)
            .expect("the gap fills");
        assert_eq!((budget.used(), c.reliable_buffered_bytes()), (0, 0));
        assert_eq!(c.stats().reliable_assembling_bytes, 3_000);
        // The resent third fragment now fits; the last one ends the message.
        c.receive_within(&reliable_body(5, 3, true, 1_000), 3, &mut budget)
            .expect("in order");
        c.receive_within(&reliable_body(6, 4, false, 10), 4, &mut budget)
            .expect("the end");
        let (channel, message) = c.next_message().expect("delivered");
        assert_eq!((channel, message.len()), (Channel::GAME_RPC, 4_010));
        budget.release(c.reliable_buffered_bytes());
        assert_eq!(budget.used(), 0);
    }

    /// ADR-8658: before its first round-trip sample, a receiver whose keepalive (sent at 0) is
    /// still unanswered knows the round trip is at least that long: the deadline's RTO is never
    /// below the wait, up to `resend_max_ms`. A gap opened at 0 under keepalive-paced traffic
    /// fails at 4 x `resend_max_ms`, not at the 2 s floor, and no later however long the peer
    /// leaves the keepalive unanswered.
    #[test]
    fn an_unanswered_keepalive_stretches_the_unmeasured_stall_deadline_to_its_cap() {
        let mut c = Connection::new(LinkConfig::default());
        assert_eq!(c.poll(0, &[]).len(), 1, "the first poll sends a keepalive");
        c.receive(&reliable_body(1, 1, true, 100), 0).expect("stored");
        let (mut now, mut seq) = (0, 1);
        let failed = loop {
            now += 50;
            seq += 1;
            let keepalive = {
                let mut w = WireWriter::new();
                w.u8(1);
                w.u16(seq);
                w.u8(RECORD_PING);
                w.finish()
            };
            if c.receive(&keepalive, now).is_err() {
                break now;
            }
            assert!(now < 20_000, "never stalled");
        };
        assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
        assert_eq!(
            failed,
            REASSEMBLY_STALL_RTO_MULTIPLE * c.config().resend_max_ms,
            "4 x the largest RTO the configuration allows"
        );
    }

    /// ADR-8859 (KI-NT-034): `LinkStats::reassembly_stall_deadline_ms` is the deadline the stall
    /// check enforces, the unanswered-packet bound of ADR-8658 included. Same link as above: while
    /// it lives, the counted stall stays under the reported deadline, and it fails at the deadline
    /// last reported. Without the bound in the report the stat said 2 s while the link lived to 4 s.
    #[test]
    fn stats_report_the_stall_deadline_with_the_unanswered_packet_bound() {
        let mut c = Connection::new(LinkConfig::default());
        assert_eq!(c.poll(0, &[]).len(), 1, "the first poll sends a keepalive");
        c.receive(&reliable_body(1, 1, true, 100), 0).expect("stored");
        assert_eq!(
            c.stats().reassembly_stall_deadline_ms,
            REASSEMBLY_STALL_MIN_MS,
            "nothing has waited yet: the 2 s floor"
        );
        let (mut now, mut seq) = (0, 1);
        let mut reported = c.stats().reassembly_stall_deadline_ms;
        let failed = loop {
            now += 50;
            seq += 1;
            let keepalive = {
                let mut w = WireWriter::new();
                w.u8(1);
                w.u16(seq);
                w.u8(RECORD_PING);
                w.finish()
            };
            if c.receive(&keepalive, now).is_err() {
                break now;
            }
            let stats = c.stats();
            assert!(
                stats.reassembly_stall_ms < stats.reassembly_stall_deadline_ms,
                "at {now} ms the link lives with {} ms counted against a reported deadline of {} ms",
                stats.reassembly_stall_ms,
                stats.reassembly_stall_deadline_ms
            );
            reported = stats.reassembly_stall_deadline_ms;
            assert!(now < 20_000, "never stalled");
        };
        assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
        assert_eq!(
            failed, reported,
            "it fails at the deadline the stats last reported"
        );
        assert_eq!(c.stats().reassembly_stall_deadline_ms, failed);
    }

    /// ADR-7981: the stall clock counts the peer's traffic, at most one RTO per gap between packets,
    /// so a peer back from a long outage that fills its gap is not failed for the outage; progress
    /// resets the clock.
    #[test]
    fn an_outage_does_not_use_up_the_stall_deadline() {
        let mut c = Connection::new(LinkConfig::default());
        let deadline = c.reassembly_stall_ms();
        assert_eq!(
            deadline, REASSEMBLY_STALL_MIN_MS,
            "4 x RTO is below 2 s before any RTT sample"
        );
        c.receive(&reliable_body(1, 1, true, 100), 0).expect("stored");
        // Ten seconds of silence, then the gap is filled.
        c.receive(&reliable_body(2, 0, true, 100), 10_000)
            .expect("an outage is not a stall");
        assert!(c.failure().is_none());
        // A message left open, with keepalive-paced traffic: fails once the deadline is spent.
        let mut now = 10_000;
        let mut seq = 3;
        let failed = loop {
            now += 50;
            seq += 1;
            let keepalive = {
                let mut w = WireWriter::new();
                // Wire 5 (ADR-8184): flags (a 2-byte packet number, no ack frame), then the number.
                w.u8(1);
                w.u16(seq);
                w.u8(RECORD_PING);
                w.finish()
            };
            if c.receive(&keepalive, now).is_err() {
                break now;
            }
            assert!(now < 20_000, "never stalled");
        };
        assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
        assert_eq!(
            failed - 10_000,
            deadline,
            "the deadline of traffic after the last progress"
        );
        assert_eq!(c.stats().reliable_assembling_bytes, 0, "freed");
    }

    /// Authenticated bodies that do not parse, one of each early parse error in turn.
    fn unparseable_body(k: u64) -> Vec<u8> {
        match k % 3 {
            0 => Vec::new(),    // empty: no flags byte
            1 => vec![0x80],    // unknown packet flags
            _ => vec![1, 0x2a], // a 2-byte packet number cut short
        }
    }

    /// ADR-8764 (review of v06-freeze-reds-net-fuzz, finding 1): an authenticated packet that does
    /// not parse is still the peer's traffic. Behind a gap, a peer that sends only such packets
    /// fails with `ReassemblyStalled` at the same deadline keepalives would reach, and nothing of
    /// those packets is applied.
    #[test]
    fn an_authenticated_unparseable_drip_counts_toward_the_stall_deadline() {
        let mut c = Connection::new(LinkConfig::default());
        let deadline = c.reassembly_stall_ms();
        c.receive(&reliable_body(1, 1, true, 100), 0).expect("stored");
        let received = c.stats().packets_received;
        let mut now = 0;
        let failed = loop {
            now += 50;
            if c.receive(&unparseable_body(now / 50), now).is_err() && c.failure().is_some() {
                break now;
            }
            assert!(now < 20_000, "never stalled");
        };
        assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
        assert_eq!(
            failed, deadline,
            "the deadline of the peer's traffic since the gap"
        );
        assert_eq!(
            c.stats().packets_received,
            received,
            "nothing of them was applied"
        );
    }

    /// ADR-8764: the measured-loss stretch (ADR-8521) is for an honest sender's lost repairs. A
    /// peer that skips its own packet numbers stretches its deadline, but once its authenticated
    /// packets stop parsing the stall fails at the unstretched deadline.
    #[test]
    fn an_unparseable_packet_withholds_the_loss_stretch() {
        let mut c = Connection::new(LinkConfig::default());
        let base = c.reassembly_stall_ms();
        c.receive(&reliable_body(1, 1, true, 100), 0).expect("stored");
        // Packet number 10 after 1: eight numbers missing, so the stretch is large.
        let ping = {
            let mut w = WireWriter::new();
            w.u8(1);
            w.u16(10);
            w.u8(RECORD_PING);
            w.finish()
        };
        c.receive(&ping, 0).expect("a ping");
        let stretched = c.reassembly_stall_ms();
        assert!(
            stretched > base,
            "the skipped numbers stretch the deadline ({stretched} ms)"
        );
        let mut now = 0;
        let failed = loop {
            now += 50;
            if c.receive(&unparseable_body(now / 50), now).is_err() && c.failure().is_some() {
                break now;
            }
            assert!(now < 20_000, "never stalled");
        };
        assert_eq!(c.failure(), Some(LinkFailure::ReassemblyStalled));
        assert_eq!(failed, base, "the unstretched deadline, not {stretched} ms");
    }

    /// ADR-8764: the same packets from a source nothing validated (ADR-8606) do not run the
    /// stall clock, so a forger on the path cannot fail an honest peer's link with them.
    #[test]
    fn an_unvalidated_unparseable_packet_does_not_run_the_stall_clock() {
        let mut budget = ReassemblyBudget::unlimited();
        let mut c = Connection::new(LinkConfig::default());
        c.receive(&reliable_body(1, 1, true, 100), 0).expect("stored");
        for k in 1..=400u64 {
            assert!(
                c.receive_unvalidated_within(&unparseable_body(k), k * 50, &mut budget)
                    .is_err()
            );
        }
        assert!(c.failure().is_none(), "the link stands");
        assert_eq!(
            c.stats().reassembly_stall_ms,
            0,
            "no traffic of the peer's was counted"
        );
        // The peer's own next packet counts its gap since the last packet it sent, at most one RTO.
        c.receive(&reliable_body(2, 2, true, 100), 20_050)
            .expect("stored");
        assert!(c.failure().is_none());
    }
}

#[cfg(test)]
mod mtu_edge_tests {
    use super::*;

    #[test]
    fn quic_edges_size_refusal_preserves_sequence_and_reliable_work() {
        let mut sender = Connection::new(LinkConfig {
            max_packet_bytes: 1400,
            ..LinkConfig::default()
        });
        let mut receiver = Connection::new(LinkConfig::default());
        let message = vec![0x42; 1300];
        sender.send(Channel::GAME_RPC, message.clone(), 0).expect("queue");
        let mut refused = 0;
        sender.poll_transmit(0, &[], |packet| {
            assert!(packet.len() > 1200);
            refused += 1;
            Err(crate::socket::TxRefused::TooLarge(1200))
        });
        assert_eq!(refused, 1);
        assert_eq!(
            sender.stats().packets_sent,
            0,
            "a refused datagram must not burn a sequence"
        );
        let mut sequences = Vec::new();
        sender.poll_transmit(1, &[], |packet| {
            assert!(packet.len() <= 1200, "retry must shrink to the peer limit");
            sequences.push(test_packet_number(&packet));
            receiver.receive(&packet, 1).expect("receive");
            Ok(())
        });
        assert_eq!(sequences, vec![0, 1]);
        assert_eq!(receiver.next_message(), Some((Channel::GAME_RPC, message)));
    }

    #[test]
    fn quic_edges_refusal_after_one_send_keeps_published_ids_and_delayed_ack_valid() {
        let mut sender = Connection::new(LinkConfig {
            max_packet_bytes: 1400,
            ..LinkConfig::default()
        });
        let mut receiver = Connection::new(LinkConfig::default());
        sender.send(Channel::GAME_RPC, vec![1; 500], 0).expect("first");
        sender.send(Channel::GAME_RPC, vec![2; 1300], 0).expect("second");
        sender.poll_transmit(0, &[], |packet| {
            if packet.len() > 1200 {
                return Err(crate::socket::TxRefused::TooLarge(1200));
            }
            assert_eq!(test_packet_number(&packet), 0);
            receiver.receive(&packet, 0).expect("first packet");
            Ok(())
        });
        assert_eq!(sender.stats().packets_sent, 1);
        assert_eq!(
            sender.reliable_out.iter().map(|f| f.id).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(sender.reliable_out[0].bytes, vec![1; 500]);
        for ack in receiver.poll(1, &[]) {
            sender.receive(&ack, 1).expect("delayed ACK");
        }
        assert_eq!(sender.stats().reliable_in_flight, 2);
        let mut sequences = Vec::new();
        sender.poll_transmit(2, &[], |packet| {
            assert!(packet.len() <= 1200);
            sequences.push(test_packet_number(&packet));
            receiver.receive(&packet, 2).expect("reframed tail");
            Ok(())
        });
        assert_eq!(sequences, [1, 2]);
        assert_eq!(receiver.next_message(), Some((Channel::GAME_RPC, vec![1; 500])));
        assert_eq!(receiver.next_message(), Some((Channel::GAME_RPC, vec![2; 1300])));
        assert_eq!(receiver.next_message(), None);
    }

    #[test]
    fn quic_edges_refusal_retains_unreliable_records_that_fit_smaller_packets() {
        let mut sender = Connection::new(LinkConfig::default());
        let mut receiver = Connection::new(LinkConfig::default());
        for byte in [1, 2] {
            sender
                .send(Channel::Unreliable, vec![byte; 500], 0)
                .expect("queue");
        }
        sender.poll_transmit(0, &[], |_| Err(crate::socket::TxRefused::TooLarge(800)));
        assert_eq!(sender.stats().packets_sent, 0);
        assert_eq!(sender.stats().unreliable_dropped, 0);
        sender.poll_transmit(1, &[], |packet| {
            assert!(packet.len() <= 800);
            receiver.receive(&packet, 1).expect("receive");
            Ok(())
        });
        for byte in [1, 2] {
            assert_eq!(
                receiver.next_message(),
                Some((Channel::Unreliable, vec![byte; 500]))
            );
        }
        assert_eq!(sender.stats().packets_sent, 2);
        assert_eq!(sender.stats().unreliable_dropped, 0);
    }

    #[test]
    fn quic_edges_reframed_queue_respects_the_receive_window() {
        let mut sender = Connection::new(LinkConfig {
            max_packet_bytes: 1400,
            ..LinkConfig::default()
        });
        let mut receiver = Connection::new(LinkConfig::default());
        for _ in 0..RELIABLE_WINDOW {
            sender
                .send(Channel::GAME_RPC, vec![9; 1300], 0)
                .expect("original queue fits");
        }
        sender.limit_packet_bytes(1200, 0);
        assert_eq!(sender.stats().reliable_in_flight, 2 * RELIABLE_WINDOW);
        let mut received = 0;
        for step in 0..1000 {
            let now = step * 100;
            sender.poll_transmit(now, &[], |packet| {
                assert!(packet.len() <= 1200);
                receiver.receive(&packet, now).expect("receive");
                Ok(())
            });
            while let Some((channel, bytes)) = receiver.next_message() {
                assert_eq!(channel, Channel::GAME_RPC);
                assert_eq!(bytes, vec![9; 1300]);
                received += 1;
            }
            for ack in receiver.poll(now, &[]) {
                sender.receive(&ack, now).expect("ACK");
            }
            if sender.stats().reliable_in_flight == 0 {
                break;
            }
        }
        assert_eq!(received, RELIABLE_WINDOW);
        assert_eq!(sender.stats().reliable_in_flight, 0);
    }

    #[test]
    fn quic_edges_shrink_drops_one_oversized_unreliable_message_but_sends_the_next() {
        let mut sender = Connection::new(LinkConfig {
            max_packet_bytes: 1400,
            ..LinkConfig::default()
        });
        let mut receiver = Connection::new(LinkConfig::default());
        sender
            .send(Channel::Unreliable, vec![1; 1300], 0)
            .expect("fits old cap");
        sender
            .send(Channel::Unreliable, vec![2; 100], 0)
            .expect("small message");
        sender.limit_packet_bytes(1200, 0);
        sender.poll_transmit(0, &[], |packet| {
            assert!(packet.len() <= 1200);
            receiver.receive(&packet, 0).expect("receive");
            Ok(())
        });
        assert_eq!(receiver.next_message(), Some((Channel::Unreliable, vec![2; 100])));
        assert_eq!(receiver.next_message(), None);
        assert_eq!(sender.stats().unreliable_dropped, 1);
    }
}

#[cfg(test)]
mod unify_tests {
    //! The unified loss, pacing and recovery policy (ADR-7995) at the level of one connection.
    use super::*;

    fn exchange(from: &mut Connection, to: &mut Connection, now: u64) -> usize {
        let packets = from.poll(now, &[]);
        let n = packets.len();
        for p in packets {
            to.receive(&p, now).expect("valid packet");
        }
        n
    }

    /// A hundred packets between two replies (wire 5): one acknowledgment-only reply names
    /// them all in one range; the sender sees every packet acknowledged and none lost.
    #[test]
    fn an_acknowledgment_train_names_a_burst_oldest_first() {
        let mut a = Connection::new(LinkConfig {
            max_packets_per_poll: 128,
            ..blind_link()
        });
        let mut b = Connection::new(blind_link());
        // One message per packet, within the unreliable queue's byte cap.
        for _ in 0..100 {
            a.send(Channel::Unreliable, vec![1; 600], 0).expect("queued");
        }
        assert_eq!(exchange(&mut a, &mut b, 0), 100);
        let frame = b.ack_frame(1).expect("frame");
        assert_eq!(frame.ranges, vec![(0, 99)]);
        let replies = b.poll(1, &[]);
        assert_eq!(replies.len(), 1, "one reply");
        for p in &replies {
            a.receive(p, 2).expect("ack");
        }
        let stats = a.stats();
        assert_eq!(stats.packets_acked, 100);
        assert_eq!(stats.packets_lost, 0);
        assert!(!b.ack_pending, "everything is reported");
    }

    /// Four polls of 100 packets before one reply, with every tenth packet lost (40 ranges, more
    /// than one frame names): the older ranges leave first in a frame of their own, so the sender
    /// finds no false gap and counts exactly the dropped packets lost.
    #[test]
    fn a_capped_acknowledgment_train_never_skips_a_range() {
        let mut a = Connection::new(LinkConfig {
            max_packets_per_poll: 512,
            ..blind_link()
        });
        let mut b = Connection::new(blind_link());
        let mut dropped = 0;
        for _ in 0..4 {
            for _ in 0..100 {
                a.send(Channel::Unreliable, vec![1; 600], 0).expect("queued");
            }
            for (i, p) in a.poll(0, &[]).into_iter().enumerate() {
                if i % 10 == 9 {
                    dropped += 1;
                    continue;
                }
                b.receive(&p, 0).expect("valid packet");
            }
        }
        let frame = b.ack_frame(1).expect("frame");
        assert_eq!(frame.ranges.len(), MAX_ACK_RANGES, "the newest 32 ranges");
        let replies = b.poll(1, &[]);
        assert_eq!(
            replies.len(),
            2,
            "the 8 older ranges go first, in their own frame"
        );
        for p in &replies {
            a.receive(p, 1).expect("ack");
        }
        // 40 ranges of 9 packets are named; the dropped packets below the largest acknowledged
        // (398) are lost, the last one (399) is not judged yet.
        assert_eq!(a.stats().packets_acked, 360);
        assert_eq!(a.stats().packets_lost, dropped - 1);
        assert_eq!(a.stats().spurious_losses, 0);
    }

    /// Spec 120 § 4.1 items 4 and 7: three later packets acknowledged make a loss; the packet's
    /// own late acknowledgment takes the loss back, cancels the pending resend and clears the
    /// loss sample.
    #[test]
    fn a_late_acknowledgment_undoes_the_loss_and_the_resend() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        // Larger than a redundant copy takes (ADR-8184): only the held packet carries it.
        a.send(Channel::GAME_RPC, vec![9; 300], 0).expect("queued");
        let held = a.poll(0, &[]);
        assert_eq!(held.len(), 1);
        // Three later packets overtake it.
        for now in 1..=3u64 {
            a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            exchange(&mut a, &mut b, now);
        }
        for p in b.poll(4, &[]) {
            a.receive(&p, 5).expect("ack");
        }
        assert_eq!(a.stats().packets_lost, 1, "three later packets were acknowledged");
        assert!(a.reliable_out[0].lost);
        // The held packet arrives after all, before the repair left.
        b.receive(&held[0], 6).expect("late packet");
        for p in b.poll(6, &[]) {
            a.receive(&p, 7).expect("late ack");
        }
        let stats = a.stats();
        assert_eq!(stats.packets_lost, 0, "reordering is not loss");
        assert_eq!(stats.loss_fraction, 0.0);
        assert_eq!(stats.reliable_in_flight, 0);
        a.poll(8, &[]);
        assert_eq!(a.stats().reliable_resends, 0, "nothing was repeated");
    }

    /// ADR-8400: a small reliable fragment whose first transmission and its redundant copy were
    /// both lost is repaired, and the repair rides once more in the next packet that leaves
    /// within the copy window, exactly as the first transmission did. At 20 % loss a repair
    /// alone fails one time in five; with its copy, one in twenty-five.
    #[test]
    fn a_repair_takes_a_redundant_copy_of_its_own() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        a.send(Channel::GAME_RPC, vec![7; 100], 0).expect("queued");
        // The first transmission and its copy (the next packet that leaves) are both lost.
        let first = a.poll(0, &[]);
        assert_eq!(first.len(), 1);
        a.send(Channel::Unreliable, vec![1], 0).expect("queued");
        let copy = a.poll(5, &[]);
        assert_eq!(test_reliable_records(&copy[0]), 1, "the first copy rides along");
        // Three later packets are acknowledged: the copy's packet is lost.
        for now in 6..=8u64 {
            a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            exchange(&mut a, &mut b, now);
        }
        for p in b.poll(9, &[]) {
            a.receive(&p, 10).expect("ack");
        }
        assert!(a.reliable_out[0].lost, "the fragment's latest packet was lost");
        let repair = a.poll(11, &[]);
        assert_eq!(repair.len(), 1);
        assert_eq!(test_reliable_records(&repair[0]), 1, "the repair");
        assert_eq!(a.stats().reliable_resends, 1);
        // The next packet that leaves within the window carries the repair's copy.
        a.send(Channel::Unreliable, vec![1], 0).expect("queued");
        let repair_copy = a.poll(14, &[]);
        assert_eq!(
            test_reliable_records(&repair_copy[0]),
            1,
            "the repair rides once more"
        );
        assert_eq!(a.stats().redundant_copies, 2, "one copy per transmission");
        assert_eq!(a.stats().reliable_resends, 1, "a copy is not a resend");
        // One copy per repair, and none past the window.
        for now in [16u64, 40] {
            a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            let later = a.poll(now, &[]);
            assert_eq!(test_reliable_records(&later[0]), 0, "at {now} ms");
        }
        assert_eq!(a.stats().redundant_copies, 2);
        // The copy delivers the message, once.
        b.receive(&repair_copy[0], 20).expect("copy");
        let mut got = Vec::new();
        while let Some((channel, m)) = b.next_message() {
            if channel == Channel::GAME_RPC {
                got.push(m);
            }
        }
        assert_eq!(got, vec![vec![7; 100]]);
    }

    /// ADR-8400: after a loss verdict, a repair's copy does not wait for traffic that may never
    /// come (a game that went quiet, the end of a session): the next poll sends it alone, once.
    #[test]
    fn a_repair_sends_its_copy_alone_when_nothing_else_leaves() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        a.send(Channel::GAME_RPC, vec![7; 100], 0).expect("queued");
        let _lost = a.poll(0, &[]);
        a.send(Channel::Unreliable, vec![1], 0).expect("queued");
        let _lost_copy = a.poll(5, &[]);
        for now in 6..=8u64 {
            a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            exchange(&mut a, &mut b, now);
        }
        for p in b.poll(9, &[]) {
            a.receive(&p, 10).expect("ack");
        }
        let repair = a.poll(11, &[]);
        assert_eq!(repair.len(), 1);
        assert_eq!(test_reliable_records(&repair[0]), 1, "the repair");
        // Nothing else is queued: the copy leaves on its own at the next poll.
        let copy = a.poll(12, &[]);
        assert_eq!(copy.len(), 1, "one packet for the copy");
        assert_eq!(test_reliable_records(&copy[0]), 1);
        assert_eq!(a.stats().redundant_copies, 2);
        assert_eq!(a.stats().reliable_resends, 1, "a copy is not a resend");
        // Once only.
        assert!(a.poll(13, &[]).is_empty(), "no second copy");
    }

    /// Two packets ahead is reordering, not loss: the packet threshold is three. The time
    /// threshold (9/8 of the round trip) then finds the packet that never arrives.
    #[test]
    fn two_later_acknowledgments_are_not_a_loss_until_the_time_threshold() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        // Larger than a redundant copy takes (ADR-8184): only the held packet carries it.
        a.send(Channel::GAME_RPC, vec![9; 300], 0).expect("queued");
        let _held = a.poll(0, &[]);
        for now in 1..=2u64 {
            a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            exchange(&mut a, &mut b, now);
        }
        // The acknowledgment takes a 100 ms round trip.
        for p in b.poll(2, &[]) {
            a.receive(&p, 102).expect("ack");
        }
        assert!((99.0..=101.0).contains(&a.stats().rtt_ms));
        assert_eq!(a.stats().packets_lost, 0, "two packets ahead is not a loss");
        assert!(!a.reliable_out[0].lost);
        // 9/8 of 100 ms after it left, with a later packet acknowledged, it is lost and repaired.
        assert!(a.poll(110, &[]).iter().all(|p| test_reliable_records(p) == 0));
        assert_eq!(a.stats().packets_lost, 0);
        let repair = a.poll(113, &[]);
        assert_eq!(a.stats().packets_lost, 1);
        assert_eq!(a.stats().reliable_resends, 1);
        assert!(repair.iter().any(|p| test_reliable_records(p) > 0));
    }

    /// The window opens with the first burst, doubles while the path delivers what it lets out,
    /// and is not shrunk by a round trip that needed less.
    #[test]
    fn the_window_follows_delivery_and_ignores_idle_rounds() {
        // Raw transport throughput: more reliable messages per second than a class cap allows
        // (spec 120 § 4.5 A2, ADR-8407), so the caps are off.
        let mut a = Connection::new(LinkConfig {
            message_caps: false,
            ..blind_link()
        });
        let mut b = Connection::new(blind_link());
        let initial = a.window.bytes;
        assert_eq!(initial, 64 * 1200);
        for _ in 0..600 {
            a.send(Channel::GAME_RPC, vec![3; 1100], 0).expect("queued");
        }
        // One poll's packet cap, then the rest of the first window; after that it is closed.
        assert_eq!(exchange(&mut a, &mut b, 0), 64);
        assert_eq!(exchange(&mut a, &mut b, 1), initial / 1105 - 64);
        assert!(a.window.round_limited);
        assert_eq!(exchange(&mut a, &mut b, 2), 0, "the window holds the rest");
        assert_eq!(a.in_flight_bytes, (initial / 1105) * 1105);
        exchange(&mut b, &mut a, 20);
        assert_eq!(a.in_flight_bytes, 0);
        assert!(
            a.window.bytes >= 2 * 64 * 1105,
            "twice what one round trip delivered: {}",
            a.window.bytes
        );
        // An application-limited tail must not shrink it.
        for step in 2..60u64 {
            exchange(&mut a, &mut b, step * 20);
            exchange(&mut b, &mut a, step * 20);
        }
        let grown = a.window.bytes;
        assert!(!a.has_unacked_reliable());
        assert!(
            grown >= 2 * 64 * 1105,
            "the window kept what the path delivered: {grown}"
        );
        for step in 60..120u64 {
            a.send(Channel::GAME_RPC, vec![4; 100], 0).expect("queued");
            exchange(&mut a, &mut b, step * 20);
            exchange(&mut b, &mut a, step * 20);
        }
        assert_eq!(
            a.window.bytes, grown,
            "small sends neither grow nor shrink the window"
        );
        assert_eq!(a.stats().recoveries, 0);
        assert_eq!(a.stats().reliable_resends, 0);
    }

    /// A burst the path delivers in reverse order: most packets arrive more than 32 behind the
    /// newest. The receiver still reports every one, so nothing is resent or counted lost.
    #[test]
    fn a_shuffled_burst_is_reported_in_full_and_never_resent() {
        let mut a = Connection::new(blind_link());
        let mut b = Connection::new(blind_link());
        for id in 0..64u8 {
            a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queued");
        }
        let burst = a.poll(0, &[]);
        assert_eq!(burst.len(), 64);
        for p in burst.iter().rev() {
            b.receive(p, 5).expect("data");
        }
        // The same packet again is a duplicate, wherever it sits in the window.
        b.receive(&burst[0], 5).expect("duplicate");
        assert_eq!(b.stats().duplicates, 1);
        for p in b.poll(5, &[]) {
            a.receive(&p, 10).expect("ack");
        }
        let got: Vec<u8> = std::iter::from_fn(|| b.next_message())
            .map(|(_, m)| m[0])
            .collect();
        assert_eq!(got, (0..64).collect::<Vec<u8>>());
        assert_eq!(a.stats().packets_acked, 64);
        assert_eq!(a.stats().packets_lost, 0);
        assert!(!a.has_unacked_reliable());
        a.poll(200, &[]);
        assert_eq!(a.stats().reliable_resends, 0);
    }

    /// A verdict that turns out wrong widens the packet threshold to the reordering seen, so the
    /// same shuffle is not misread again (RFC 9002 § 6.1.1).
    #[test]
    fn a_spurious_verdict_widens_the_reorder_threshold() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        assert_eq!(a.reorder_threshold, u64::from(LOSS_PACKET_THRESHOLD));
        let mut now = 0u64;
        for round in 0..2 {
            // One packet is overtaken by five later ones.
            a.send(Channel::Unreliable, vec![0], 0).expect("queued");
            let held = a.poll(now, &[]);
            for _ in 0..5 {
                now += 1;
                a.send(Channel::Unreliable, vec![1], 0).expect("queued");
                exchange(&mut a, &mut b, now);
            }
            for p in b.poll(now, &[]) {
                a.receive(&p, now + 1).expect("ack");
            }
            if round == 0 {
                assert_eq!(a.stats().packets_lost, 1, "the first time it reads as a loss");
            } else {
                assert_eq!(a.stats().packets_lost, 0, "the same shuffle is not misread again");
            }
            b.receive(&held[0], now + 1).expect("late packet");
            for p in b.poll(now + 1, &[]) {
                a.receive(&p, now + 2).expect("late ack");
            }
            assert_eq!(a.stats().packets_lost, 0);
            assert_eq!(a.reorder_threshold, 6, "five packets overtook it");
            now += 10;
        }
        assert!(a.reorder_window_ms >= 1);
    }

    /// The one reply that acknowledges a burst is lost (wire 5): the receiver's next packet
    /// carries the same frame, so the burst is neither resent nor counted lost.
    #[test]
    fn a_lost_train_packet_is_covered_by_its_twin() {
        let mut a = Connection::new(blind_link());
        let mut b = Connection::new(blind_link());
        for id in 0..64u8 {
            a.send(Channel::GAME_RPC, vec![id; 1100], 0).expect("queued");
        }
        assert_eq!(exchange(&mut a, &mut b, 0), 64);
        let replies = b.poll(1, &[]);
        assert_eq!(replies.len(), 1, "one reply");
        // It is lost; b's next packet (a state update at 20 ms) carries the acknowledgment again.
        let mut acked_at = None;
        for now in 2..200u64 {
            if now == 20 {
                b.send(Channel::Unreliable, vec![5; 300], 0).expect("state");
            }
            for p in b.poll(now, &[]) {
                a.receive(&p, now).expect("ack");
            }
            if acked_at.is_none() && !a.has_unacked_reliable() {
                acked_at = Some(now);
            }
            a.poll(now, &[]);
        }
        assert_eq!(acked_at, Some(20));
        assert_eq!(a.stats().packets_acked, 64);
        assert_eq!(a.stats().packets_lost, 0);
        assert_eq!(a.stats().reliable_resends, 0);
    }

    /// A receiver that stalls for seconds acknowledges old packets when it wakes. Those samples
    /// are the peer's delay, not the path's: the smoothed round trip does not move, and a packet
    /// sent afterwards restores the latest sample.
    #[test]
    fn a_stalled_peer_does_not_stretch_the_smoothed_round_trip() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        // A 40 ms round trip.
        let mut to_b: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        let mut to_a: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        let mut run = |a: &mut Connection, b: &mut Connection, from: u64, to: u64, b_polls: bool| {
            for now in (from..to).step_by(10) {
                for p in a.poll(now, &[]) {
                    to_b.push_back((now + 20, p));
                }
                while to_b.front().is_some_and(|(t, _)| *t <= now) {
                    if let Some((_, p)) = to_b.pop_front() {
                        b.receive(&p, now).expect("packet");
                    }
                }
                if b_polls {
                    for p in b.poll(now, &[]) {
                        to_a.push_back((now + 20, p));
                    }
                }
                while to_a.front().is_some_and(|(t, _)| *t <= now) {
                    if let Some((_, p)) = to_a.pop_front() {
                        a.receive(&p, now).expect("ack");
                    }
                }
            }
        };
        run(&mut a, &mut b, 0, 1_000, true);
        let before = a.stats().rtt_ms;
        assert!((39.0..=41.0).contains(&before), "rtt {before}");
        run(&mut a, &mut b, 1_000, 6_000, false);
        run(&mut a, &mut b, 6_000, 6_100, true);
        let after = a.stats().rtt_ms;
        assert!(
            (after - before).abs() <= 2.0,
            "smoothed rtt moved from {before} to {after}"
        );
        run(&mut a, &mut b, 6_100, 6_500, true);
        assert!(
            a.latest_rtt <= 45.0,
            "a fresh packet restores the latest sample: {}",
            a.latest_rtt
        );
        assert_eq!(a.stats().packets_lost, 0);
    }

    /// Random loss is not congestion (spec 120 § 4.2 item 3): at 10 % loss in each direction
    /// the window never drops below its first size, the stream never enters recovery and it
    /// keeps most of its rate, delivered once and in order.
    #[test]
    fn random_loss_does_not_shrink_the_window() {
        // Raw transport throughput: more reliable messages per second than a class cap allows
        // (spec 120 § 4.5 A2, ADR-8407), so the caps are off.
        let mut a = Connection::new(LinkConfig {
            message_caps: false,
            ..blind_link()
        });
        let mut b = Connection::new(blind_link());
        let initial = a.window.bytes;
        let mut rng = 0x1234_5678_9ABC_DEF1u64;
        let mut lost = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng.is_multiple_of(10)
        };
        let (mut next, mut got) = (0u32, 0u32);
        let mut smallest = initial;
        for step in 0..600u64 {
            let now = step * 20;
            loop {
                let mut m = vec![0u8; 1000];
                m[..4].copy_from_slice(&next.to_le_bytes());
                if a.send(Channel::GAME_RPC, m, 0).is_err() {
                    break;
                }
                next += 1;
            }
            for p in a.poll(now, &[]) {
                if !lost() {
                    b.receive(&p, now).expect("packet");
                }
            }
            for p in b.poll(now, &[]) {
                if !lost() {
                    a.receive(&p, now).expect("ack");
                }
            }
            while let Some((_, m)) = b.next_message() {
                assert_eq!(u32::from_le_bytes([m[0], m[1], m[2], m[3]]), got, "in order");
                got += 1;
            }
            smallest = smallest.min(a.window.bytes);
        }
        assert!(
            smallest >= initial,
            "the window fell to {smallest} from {initial}"
        );
        assert_eq!(a.stats().recoveries, 0);
        assert!(
            got >= 20_000,
            "the stream keeps most of its rate: {got} of 38,400 at most"
        );
        let stats = a.stats();
        assert!(
            (0.05..0.2).contains(&stats.loss_fraction),
            "about a tenth is counted lost: {}",
            stats.loss_fraction
        );
    }

    /// Forty seeded links with random delay, jitter, loss, duplication, reordering and outages,
    /// carrying reliable messages of mixed sizes both ways. Every message arrives once and in
    /// order, both queues drain, and at every step the in-flight byte count equals what the
    /// queue says is in flight and stays within the window (plus timer probes, which bypass it).
    #[test]
    fn seeded_links_deliver_in_order_and_keep_the_in_flight_accounting_exact() {
        struct Rng(u64);
        impl Rng {
            fn next(&mut self) -> u64 {
                self.0 ^= self.0 << 13;
                self.0 ^= self.0 >> 7;
                self.0 ^= self.0 << 17;
                self.0
            }
            fn below(&mut self, n: u64) -> u64 {
                self.next() % n.max(1)
            }
        }
        fn consistent(c: &Connection) {
            let flagged: usize = c
                .reliable_out
                .iter()
                .filter(|f| f.in_flight)
                .map(OutFragment::wire_bytes)
                .sum();
            assert_eq!(c.in_flight_bytes, flagged, "in-flight bytes match the queue");
            assert!(c.reliable_out.iter().all(|f| !(f.in_flight && f.acked)));
            assert!(c.window.bytes >= c.window_min() && c.window.bytes <= c.window_max());
        }
        let (mut all_sent, mut all_lost, mut all_resent, mut all_recoveries) = (0u64, 0u64, 0u64, 0u64);
        for seed in 1..=40u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let delay = rng.below(300);
            let jitter = rng.below(80);
            let loss = rng.below(30);
            let dup = rng.below(5);
            let reorder = rng.below(30);
            let reorder_max = 1 + rng.below(150);
            let outage_at = 2_000 + rng.below(8_000);
            let outage_len = rng.below(6_000);
            let mut ends = [
                // Raw transport accounting: the class caps (ADR-8407) are off.
                Connection::new(LinkConfig {
                    message_caps: false,
                    ..LinkConfig::default()
                }),
                Connection::new(LinkConfig::default()),
            ];
            // In flight toward end 0 and end 1: (arrival, bytes).
            let mut wires: [Vec<(u64, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
            let mut sent = [0u32; 2];
            let mut got = [0u32; 2];
            let mut now = 0u64;
            while now < 180_000 {
                let sending = now < 15_000;
                let out = now >= outage_at && now < outage_at + outage_len;
                for side in 0..2 {
                    if sending && rng.below(3) == 0 {
                        let len = match rng.below(10) {
                            0 => 2_000 + rng.below(18_000) as usize,
                            1..=3 => 200 + rng.below(1_000) as usize,
                            _ => 4 + rng.below(60) as usize,
                        };
                        let mut m = vec![0u8; len];
                        m[..4].copy_from_slice(&sent[side].to_le_bytes());
                        if ends[side].send(Channel::GAME_RPC, m, 0).is_ok() {
                            sent[side] += 1;
                        }
                    }
                    for p in ends[side].poll(now, &[]) {
                        if out || rng.below(100) < loss {
                            continue;
                        }
                        let mut hold = delay + rng.below(jitter + 1);
                        if rng.below(100) < reorder {
                            hold += 1 + rng.below(reorder_max);
                        }
                        if rng.below(100) < dup {
                            wires[1 - side].push((now + hold, p.clone()));
                        }
                        wires[1 - side].push((now + hold, p));
                    }
                    consistent(&ends[side]);
                }
                for side in 0..2 {
                    let mut due: Vec<(u64, Vec<u8>)> = Vec::new();
                    wires[side].retain(|(at, p)| {
                        let arrived = *at <= now;
                        if arrived {
                            due.push((*at, p.clone()));
                        }
                        !arrived
                    });
                    due.sort_by_key(|(at, _)| *at);
                    for (_, p) in due {
                        ends[side].receive(&p, now).expect("valid packet");
                    }
                    consistent(&ends[side]);
                    while let Some((channel, m)) = ends[side].next_message() {
                        assert_eq!(channel, Channel::GAME_RPC);
                        let n = u32::from_le_bytes([m[0], m[1], m[2], m[3]]);
                        assert_eq!(n, got[side], "seed {seed}: once, in order");
                        got[side] += 1;
                    }
                }
                if !sending
                    && got[0] == sent[1]
                    && got[1] == sent[0]
                    && ends.iter().all(|c| !c.has_unacked_reliable())
                {
                    break;
                }
                now += 10;
            }
            if got[0] != sent[1] {
                for c in &ends {
                    eprintln!("DEBUG seed {seed} stats {:?}", c.stats());
                    eprintln!(
                        "DEBUG front {:?} in_flight {} window {} pto {} deadline {:?} recovery {:?} srtt {:?} latest {} largest_acked {:?} sent {} local {}",
                        c.reliable_out.front().map(|f| (
                            f.id,
                            f.last_sent_ms,
                            f.last_seq,
                            f.acked,
                            f.lost,
                            f.in_flight,
                            f.timeouts
                        )),
                        c.in_flight_bytes,
                        c.window.bytes,
                        c.pto_count,
                        c.probe_deadline_ms(),
                        c.recovery,
                        c.srtt,
                        c.latest_rtt,
                        c.largest_acked,
                        c.sent.len(),
                        c.local_seq
                    );
                    eprintln!(
                        "DEBUG sent front {:?}",
                        c.sent.front().map(|p| (
                            p.seq,
                            p.acked,
                            p.lost,
                            p.ack_eliciting,
                            p.reliable_ids.len()
                        ))
                    );
                }
            }
            assert_eq!(
                (got[0], got[1]),
                (sent[1], sent[0]),
                "seed {seed} (delay {delay} jitter {jitter} loss {loss}% dup {dup}% reorder {reorder}% \
                 outage {outage_len} ms): everything delivered"
            );
            for c in &ends {
                assert!(!c.has_unacked_reliable(), "seed {seed}: both queues drain");
                assert_eq!(c.in_flight_bytes, 0, "seed {seed}");
                let stats = c.stats();
                assert!(stats.packets_lost <= stats.packets_sent);
                all_lost += stats.packets_lost;
                all_resent += stats.reliable_resends;
                all_recoveries += stats.recoveries;
            }
            all_sent += u64::from(sent[0] + sent[1]);
        }
        // The links really were hostile: messages flowed, packets were lost and repaired, and
        // some outages were long enough to count.
        assert!(all_sent > 20_000, "{all_sent} messages");
        assert!(
            all_lost > 1_000 && all_resent > 100,
            "{all_lost} lost, {all_resent} resent"
        );
        assert!(all_recoveries > 0, "{all_recoveries} recoveries");
        println!("PROPERTY sent={all_sent} lost={all_lost} resent={all_resent} recoveries={all_recoveries}");
    }

    /// One saturating run over a lossy, jittery link with no rate limit: both ends keep a backlog
    /// of 1000-byte reliable messages for `ms` milliseconds. Returns, per direction (toward end 0,
    /// toward end 1), the bytes delivered from `from_ms` on and the lowest pacing target its sender
    /// had from then on.
    fn saturating_lossy_run(
        link: (u64, u64, u64, u64, u64, u64),
        seed: u64,
        rate_enabled: bool,
        from_ms: u64,
        ms: u64,
    ) -> ([u64; 2], [u32; 2]) {
        let (delay, jitter, loss, dup, reorder, reorder_max) = link;
        let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
        let mut below = move |n: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % n.max(1)
        };
        let mut config = LinkConfig::default();
        config.rate.enabled = rate_enabled;
        let mut ends = [Connection::new(config.clone()), Connection::new(config)];
        let mut wires: [Vec<(u64, Vec<u8>)>; 2] = [Vec::new(), Vec::new()];
        let (mut delivered, mut lowest) = ([0u64; 2], [u32::MAX; 2]);
        let mut now = 0u64;
        while now < ms {
            for side in 0..2 {
                while ends[side].reliable_out.len() < 32 {
                    if ends[side].send(Channel::GAME_RPC, vec![7u8; 1_000], 0).is_err() {
                        break;
                    }
                }
                for p in ends[side].poll(now, &[]) {
                    if below(100) < loss {
                        continue;
                    }
                    let mut hold = delay + below(jitter + 1);
                    if below(100) < reorder {
                        hold += 1 + below(reorder_max);
                    }
                    if below(100) < dup {
                        wires[1 - side].push((now + hold, p.clone()));
                    }
                    wires[1 - side].push((now + hold, p));
                }
                if now >= from_ms {
                    lowest[1 - side] = lowest[1 - side].min(ends[side].stats().rate_target_bps);
                }
            }
            for side in 0..2 {
                let mut due: Vec<(u64, Vec<u8>)> = Vec::new();
                wires[side].retain(|(at, p)| {
                    let arrived = *at <= now;
                    if arrived {
                        due.push((*at, p.clone()));
                    }
                    !arrived
                });
                due.sort_by_key(|(at, _)| *at);
                for (_, p) in due {
                    ends[side].receive(&p, now).expect("valid packet");
                }
                while let Some((_, m)) = ends[side].next_message() {
                    if now >= from_ms {
                        delivered[side] += m.len() as u64;
                    }
                }
            }
            now += 10;
        }
        (delivered, lowest)
    }

    /// ADR-8490: a lossy, jittery link with no rate limit is not congestion, so the rate
    /// controller must not hold its sender at the minimum. The seven links are the seeded-links
    /// property's links that sat at 4 KiB/s at 59e5b6263 (12-29 % loss, 43-79 ms jitter, ADR-8400):
    /// a saturating sender over each must deliver, from 20 s to 60 s, at least half of what the
    /// same link carries with the controller off (the transport's own window alone).
    #[test]
    fn lossy_jittery_unlimited_links_keep_half_the_uncontrolled_rate() {
        // (delay, jitter, loss %, duplicates %, reordered %, reorder max), drawn by the property's
        // seeds 50, 60, 124, 143, 147, 216 and 335.
        const LINKS: [(u64, u64, u64, u64, u64, u64); 7] = [
            (43, 70, 29, 1, 9, 42),
            (72, 79, 21, 1, 9, 28),
            (149, 59, 27, 3, 27, 40),
            (36, 43, 23, 0, 27, 40),
            (51, 77, 26, 2, 21, 15),
            (142, 71, 26, 0, 13, 102),
            (157, 71, 25, 4, 12, 33),
        ];
        let mut short = Vec::new();
        for (i, &link) in LINKS.iter().enumerate() {
            let seed = 1 + i as u64;
            let (on, lowest) = saturating_lossy_run(link, seed, true, 20_000, 60_000);
            let (off, _) = saturating_lossy_run(link, seed, false, 20_000, 60_000);
            for dir in 0..2 {
                let share = on[dir] as f64 / off[dir].max(1) as f64;
                println!(
                    "link {i} {link:?} toward end {dir}: {} B/s with the controller, {} B/s without \
                     ({:.0} %), lowest target {}",
                    on[dir] / 40,
                    off[dir] / 40,
                    share * 100.0,
                    lowest[dir]
                );
                if share < 0.5 {
                    short.push((i, dir, share));
                }
            }
        }
        assert!(
            short.is_empty(),
            "links under half the uncontrolled rate: {short:?}"
        );
    }

    /// ADR-8658: background data never leaves the pacing buckets in debt. A sender with a bulk
    /// backlog at a 32 kB/s cap, over a path that answers at once, polled every 16 ms: a small
    /// reliable reply queued after any poll leaves in the next one. When a bulk packet could
    /// borrow, the 1.0x budget was in debt by up to a packet after it, and the reply waited for
    /// the refill (37 ms at the cap: two or three polls).
    #[test]
    fn a_reply_queued_behind_a_bulk_backlog_leaves_in_the_next_poll() {
        let mut config = LinkConfig::default();
        config.rate.max_bytes_per_second = 32_000;
        let mut a = Connection::new(config);
        let mut b = Connection::new(LinkConfig::default());
        let bulk = Channel::Stream(StreamId::BULK);
        let mut queued_at: Option<u64> = None;
        let (mut replies, mut worst) = (0, 0);
        let mut now = 0;
        while now < 6_000 {
            // Keep the bulk backlog.
            while a.send(bulk, vec![3u8; 8 * 1024], 0).is_ok() {}
            for p in a.poll(now, &[]) {
                b.receive(&p, now).expect("valid packet");
            }
            while let Some((channel, m)) = b.next_message() {
                if channel == Channel::GAME_RPC {
                    assert_eq!(m, vec![5u8; 100]);
                    let at = queued_at.take().expect("a reply was queued");
                    worst = worst.max(now - at);
                    replies += 1;
                }
            }
            for p in b.poll(now, &[]) {
                a.receive(&p, now).expect("valid packet");
            }
            // After the first second (past the first burst), a reply every 100 ms.
            if now >= 1_000 && queued_at.is_none() && now.is_multiple_of(96) {
                a.send(Channel::GAME_RPC, vec![5u8; 100], 0)
                    .expect("reply queued");
                queued_at = Some(now);
            }
            now += 16;
        }
        assert!(
            a.stats().reliable_in_flight > 0,
            "the bulk backlog was still sending"
        );
        assert!(replies >= 40, "{replies} replies");
        assert_eq!(worst, 16, "every reply left in the poll after it was queued");
    }

    /// Row 22: a transport that reports no usable budget stalls the connection visibly instead of
    /// silently, and the budget is not latched: packets flow again when the transport recovers.
    #[test]
    fn a_transport_below_the_protocol_minimum_is_counted_and_recovers() {
        let mut a = Connection::new(LinkConfig::default());
        a.send(Channel::GAME_RPC, vec![5; 300], 0).expect("queued");
        a.limit_packet_bytes(0, 0);
        assert!(a.poll(0, &[]).is_empty());
        assert!(a.poll(10, &[]).is_empty());
        assert_eq!(a.stats().transport_stalls, 2);
        assert_eq!(
            a.config().max_packet_bytes,
            1200,
            "an unusable budget is not latched"
        );
        a.limit_packet_bytes(2048, 0);
        let packets = a.poll(20, &[]);
        assert_eq!(packets.len(), 1, "sending resumes");
        assert_eq!(a.stats().transport_stalls, 2);
        // A refusal during a poll with a budget below the minimum also stalls instead of
        // shrinking the fragments to nothing.
        let mut b = Connection::new(LinkConfig::default());
        b.send(Channel::GAME_RPC, vec![5; 300], 0).expect("queued");
        b.poll_transmit(0, &[], |_| Err(crate::socket::TxRefused::TooLarge(100)));
        assert_eq!(b.stats().packets_sent, 0);
        assert!(b.poll(10, &[]).is_empty());
        assert_eq!(b.stats().transport_stalls, 1);
    }
}

#[cfg(test)]
mod v2_tests {
    //! Transport v2 rules one at a time (spec 120 § 4.1, ADR-8184).
    use super::*;

    /// Delivers `from`'s packets to `to` after `delay` ms, stepping both every millisecond.
    struct Path {
        to_b: VecDeque<(u64, Vec<u8>)>,
        to_a: VecDeque<(u64, Vec<u8>)>,
        delay: u64,
        cut: bool,
    }

    impl Path {
        fn new(delay: u64) -> Self {
            Self {
                to_b: VecDeque::new(),
                to_a: VecDeque::new(),
                delay,
                cut: false,
            }
        }

        /// Arrivals first, then both polls, so an acknowledgment sent "at once" leaves in the
        /// millisecond its packet arrived (no harness delay in the round trip).
        fn step(&mut self, a: &mut Connection, b: &mut Connection, now: u64) -> (usize, usize) {
            while self.to_b.front().is_some_and(|(t, _)| *t <= now) {
                if let Some((_, p)) = self.to_b.pop_front() {
                    b.receive(&p, now).expect("packet");
                }
            }
            while self.to_a.front().is_some_and(|(t, _)| *t <= now) {
                if let Some((_, p)) = self.to_a.pop_front() {
                    a.receive(&p, now).expect("packet");
                }
            }
            let (mut na, mut nb) = (0, 0);
            for p in a.poll(now, &[]) {
                na += 1;
                if !self.cut {
                    self.to_b.push_back((now + self.delay, p));
                }
            }
            for p in b.poll(now, &[]) {
                nb += 1;
                if !self.cut {
                    self.to_a.push_back((now + self.delay, p));
                }
            }
            (na, nb)
        }
    }

    /// Item 3: the side that asks gets receive timestamps, relative within one report; the side
    /// that does not ask gets none.
    #[test]
    fn receive_timestamps_are_reported_only_to_a_peer_that_asks() {
        let mut a = Connection::new(LinkConfig {
            receive_timestamps: true,
            ..LinkConfig::default()
        });
        let mut b = Connection::new(LinkConfig::default());
        let mut path = Path::new(15);
        for now in 0..400u64 {
            if now % 5 == 0 {
                a.send(Channel::Unreliable, vec![1; 50], 0).expect("queued");
            }
            if now % 20 == 0 {
                b.send(Channel::Unreliable, vec![2; 50], 0).expect("queued");
            }
            path.step(&mut a, &mut b, now);
        }
        let stamps = a.take_receive_timestamps();
        assert!(stamps.len() >= 40, "{}", stamps.len());
        assert!(b.take_receive_timestamps().is_empty(), "b never asked");
        // Within one report, arrival gaps follow send gaps on a fixed-delay path.
        let first_report = stamps[0].report;
        let one: Vec<_> = stamps.iter().filter(|s| s.report == first_report).collect();
        for pair in one.windows(2) {
            let sent_gap = pair[0].sent_ms as i64 - pair[1].sent_ms as i64;
            let recv_gap = (pair[0].received_us - pair[1].received_us) / 1_000;
            assert_eq!(sent_gap, recv_gap, "{pair:?}");
        }
        assert!(a.take_receive_timestamps().is_empty(), "taken");
    }

    /// Item 5: with keepalives far apart, a tail loss is probed at the probe timeout, each
    /// consecutive expiry doubles the wait, and each expiry sends at most two packets.
    #[test]
    fn probe_timeouts_double_and_send_at_most_two_packets() {
        let config = LinkConfig {
            keepalive_ms: 60_000,
            ..LinkConfig::default()
        };
        let mut a = Connection::new(config.clone());
        let mut b = Connection::new(config);
        let mut path = Path::new(20);
        // Measure the path with a few exchanges of data.
        for now in 0..500u64 {
            if now % 50 == 0 {
                a.send(Channel::Unreliable, vec![1; 20], 0).expect("queued");
                b.send(Channel::Unreliable, vec![1; 20], 0).expect("queued");
            }
            path.step(&mut a, &mut b, now);
        }
        let pto = a.resend_timeout_ms();
        assert!((50..=110).contains(&pto), "probe timeout {pto} on a 40 ms path");
        for i in 0..8u32 {
            a.send(Channel::GAME_RPC, vec![i as u8; 1_000], 0)
                .expect("queued");
        }
        path.cut = true;
        let mut probes = Vec::new();
        for now in 500..9_000u64 {
            let (sent, _) = path.step(&mut a, &mut b, now);
            if now > 500 && sent > 0 {
                assert!(sent <= MAX_PROBE_PACKETS, "{sent} packets at {now}");
                probes.push(now);
            }
        }
        assert!(probes.len() >= 4, "{probes:?}");
        for pair in probes.windows(3) {
            let (g1, g2) = (pair[1] - pair[0], pair[2] - pair[1]);
            assert!(g2 == (g1 * 2).min(DEFAULT_RESEND_BACKOFF_MAX_MS), "{probes:?}");
        }
        assert_eq!(a.stats().probe_timeouts, probes.len() as u64);
        // The path returns: the next acknowledgment resets the backoff and everything arrives.
        path.cut = false;
        for now in 9_000..20_000u64 {
            path.step(&mut a, &mut b, now);
        }
        assert!(!a.has_unacked_reliable());
        assert_eq!(a.pto_count, 0);
    }

    /// Item 6: the peer's ack delay comes off the sample (up to `MAX_ACK_DELAY_MS`), so a
    /// receiver that holds its acknowledgment does not stretch the round trip; `min_rtt` keeps
    /// the smallest sample of the last 10 s.
    #[test]
    fn ack_delay_comes_off_the_sample_and_min_rtt_expires_after_ten_seconds() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let mut path = Path::new(20);
        // Keepalive pings are acknowledged at once: 40 ms samples.
        for now in 0..1_000u64 {
            path.step(&mut a, &mut b, now);
        }
        assert!((39.0..=41.0).contains(&a.stats().rtt_ms), "{:?}", a.stats());
        assert!((39.0..=41.0).contains(&a.stats().min_rtt_ms));
        // One small data packet every 30 ms: b holds each acknowledgment 10 ms, and says so.
        for now in 1_000..3_000u64 {
            if now % 30 == 0 {
                a.send(Channel::Unreliable, vec![1; 20], 0).expect("queued");
            }
            path.step(&mut a, &mut b, now);
        }
        let rtt = a.stats().rtt_ms;
        assert!((39.0..=41.0).contains(&rtt), "smoothed {rtt} ms on a 40 ms path");
        // The path slows to 100 ms. `min_rtt` keeps raw samples (RFC 9002 § 5.2: never adjusted
        // for ack delay): at 12 s the 40 ms pings of the first second have left the window, the
        // raw 50 ms samples of the data phase (until 3 s) have not.
        path.delay = 50;
        for now in 3_000..12_000u64 {
            path.step(&mut a, &mut b, now);
        }
        assert_eq!(a.stats().min_rtt_ms, 50.0);
        for now in 12_000..15_000u64 {
            path.step(&mut a, &mut b, now);
        }
        assert!(a.stats().min_rtt_ms >= 99.0, "{}", a.stats().min_rtt_ms);
    }

    /// Item 2: a receiver acknowledges every second ack-eliciting packet at once and a single one
    /// within `MAX_ACK_DELAY_MS`; acknowledgment-only packets get no acknowledgment back.
    #[test]
    fn every_second_packet_is_acknowledged_at_once_and_acks_are_not_acknowledged() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        // Settle both keepalives first.
        for p in a.poll(0, &[]) {
            b.receive(&p, 0).expect("packet");
        }
        for p in b.poll(0, &[]) {
            a.receive(&p, 0).expect("packet");
        }
        // a acknowledges b's keepalive ping at once; b gets that too (no gap later).
        for p in a.poll(0, &[]) {
            b.receive(&p, 0).expect("packet");
        }
        // One data packet: no acknowledgment until 10 ms later.
        let acks_before = b.stats().ack_only_sent;
        a.send(Channel::Unreliable, vec![1; 20], 0).expect("queued");
        for p in a.poll(1, &[]) {
            b.receive(&p, 1).expect("packet");
        }
        assert!(b.poll(5, &[]).is_empty(), "held");
        let acks = b.poll(11, &[]);
        assert_eq!(acks.len(), 1, "sent at the ack delay");
        assert_eq!(b.stats().ack_only_sent, acks_before + 1);
        // Two data packets: acknowledged in the same poll.
        for _ in 0..2 {
            a.send(Channel::Unreliable, vec![1; 20], 0).expect("queued");
            for p in a.poll(20, &[]) {
                b.receive(&p, 20).expect("packet");
            }
        }
        assert_eq!(b.poll(20, &[]).len(), 1, "every second one at once");
        // a gets b's acknowledgment-only packets: it owes nothing back.
        for p in acks {
            a.receive(&p, 30).expect("packet");
        }
        assert!(
            a.poll(31, &[]).is_empty(),
            "no acknowledgment of an acknowledgment"
        );
    }
}

#[cfg(test)]
mod ack_trim_tests {
    //! Acknowledgment frames stop naming what the peer has confirmed (RFC 9000 § 13.2.4,
    //! ADR-8423): the per-datagram cost of a lockstep link, and the edge cases of the trimming.
    use super::*;

    /// Splits a packet body into (flags and packet number bytes, acknowledgment frame bytes,
    /// ranges in the frame, record bytes).
    fn split(body: &[u8]) -> (usize, usize, usize, usize) {
        let mut r = WireReader::new(body);
        let flags = r.u8().expect("flags");
        let pn_len = [1, 2, 4, 4][usize::from(flags & FLAG_PN_LEN)];
        r.take(pn_len).expect("packet number");
        let header = 1 + pn_len;
        let (frame, ranges) = if flags & FLAG_ACK != 0 {
            let before = r.remaining();
            let frame = AckFrame::decode(&mut r, flags & FLAG_ACK_TIMESTAMPS != 0).expect("frame");
            (before - r.remaining(), frame.ranges.len())
        } else {
            (0, 0)
        };
        (header, frame, ranges, r.remaining())
    }

    /// A deterministic loss pattern: true for about `per_mille` of the calls.
    struct Lcg(u64);

    impl Lcg {
        fn lost(&mut self, per_mille: u64) -> bool {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % 1000 < per_mille
        }
    }

    /// Per-datagram totals of one side.
    #[derive(Default)]
    struct Tally {
        packets: u64,
        header: u64,
        frame: u64,
        frame_max: usize,
        ranges_max: usize,
        records: u64,
    }

    impl Tally {
        fn add(&mut self, body: &[u8]) {
            let (header, frame, ranges, records) = split(body);
            self.packets += 1;
            self.header += header as u64;
            self.frame += frame as u64;
            self.frame_max = self.frame_max.max(frame);
            self.ranges_max = self.ranges_max.max(ranges);
            self.records += records as u64;
        }

        fn line(&self, label: &str) -> String {
            let per = |v: u64| v as f64 / self.packets.max(1) as f64;
            format!(
                "{label}: {} packets, per packet: header {:.2} B, ack frame {:.2} B (max {} B, max {} ranges), \
                 records {:.2} B",
                self.packets,
                per(self.header),
                per(self.frame),
                self.frame_max,
                self.ranges_max,
                per(self.records)
            )
        }
    }

    /// The lockstep shape (spec 120 § 7.2): a host and a seat each send one datagram per 16 ms
    /// tick, 40 ms one way, about 3 % loss each way, for five minutes. Every host datagram's
    /// acknowledgment frame stays a handful of bytes for the whole run: it names only what the
    /// seat has not yet confirmed hearing about, so one loss is named for about a round trip, not
    /// for as long as 31 newer losses take to push it out. Loss detection is unchanged: every
    /// delivered packet is acknowledged (no spurious loss), every dropped one is found lost, and
    /// every reliable message arrives in order.
    #[test]
    fn a_lockstep_link_carries_a_small_ack_frame_for_the_whole_run() {
        const TICK_MS: u64 = 16;
        const DELAY_MS: u64 = 40;
        const RUN_MS: u64 = 300_000;
        const LAST_MINUTE_MS: u64 = RUN_MS - 60_000;
        let mut host = Connection::new(LinkConfig::default());
        let mut seat = Connection::new(LinkConfig::default());
        let mut to_seat: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        let mut to_host: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
        let mut loss = Lcg(0x5eed);
        let (mut host_dropped, mut seat_dropped) = (0u64, 0u64);
        let (mut host_run, mut host_last) = (Tally::default(), Tally::default());
        let mut seat_run = Tally::default();
        let mut reliable_sent = 0u32;
        let mut reliable_got = Vec::new();
        for now in 0..RUN_MS {
            while to_seat.front().is_some_and(|(t, _)| *t <= now) {
                if let Some((_, p)) = to_seat.pop_front() {
                    seat.receive(&p, now).expect("host packet");
                }
            }
            while to_host.front().is_some_and(|(t, _)| *t <= now) {
                if let Some((_, p)) = to_host.pop_front() {
                    host.receive(&p, now).expect("seat packet");
                }
            }
            while let Some((channel, bytes)) = seat.next_message() {
                if channel == Channel::GAME_RPC {
                    reliable_got.push(u32::from_le_bytes(bytes[..4].try_into().expect("id")));
                }
            }
            while host.next_message().is_some() {}
            if now.is_multiple_of(TICK_MS) {
                // The frames datagram, and now and then a reliable control message.
                host.send_essential(vec![7; 60], 0).expect("frames");
                if now.is_multiple_of(TICK_MS * 50) && now + 10_000 < RUN_MS {
                    let mut msg = reliable_sent.to_le_bytes().to_vec();
                    msg.extend_from_slice(&[9; 40]);
                    host.send(Channel::GAME_RPC, msg, 0).expect("reliable");
                    reliable_sent += 1;
                }
                for p in host.poll(now, &[]) {
                    host_run.add(&p);
                    if now >= LAST_MINUTE_MS {
                        host_last.add(&p);
                    }
                    if loss.lost(30) {
                        host_dropped += 1;
                    } else {
                        to_seat.push_back((now + DELAY_MS, p));
                    }
                }
            }
            if (now + 5).is_multiple_of(TICK_MS) {
                // The seat's input.
                seat.send_essential(vec![3; 24], 0).expect("input");
                for p in seat.poll(now, &[]) {
                    seat_run.add(&p);
                    if loss.lost(30) {
                        seat_dropped += 1;
                    } else {
                        to_host.push_back((now + DELAY_MS, p));
                    }
                }
            }
        }
        eprintln!("{}", host_run.line("host, whole run"));
        eprintln!("{}", host_last.line("host, last minute"));
        eprintln!("{}", seat_run.line("seat, whole run"));
        let (hs, ss) = (host.stats(), seat.stats());
        eprintln!(
            "host dropped {host_dropped}, lost {} (spurious {}), ack frame bytes {}; seat dropped {seat_dropped}, \
             lost {} (spurious {})",
            hs.packets_lost, hs.spurious_losses, hs.ack_frame_bytes, ss.packets_lost, ss.spurious_losses
        );
        assert!(
            host_dropped > 300 && seat_dropped > 300,
            "the link really lost packets"
        );
        // The frame: largest (2 or 4 bytes), delay (1 or 2), range count, first range, and a
        // gap/length pair for a loss still unconfirmed. Base (every frame repeats the newest 32
        // ranges of the connection): over 60 bytes.
        // Wire 6 adds the two bytes of the arrival stamp to every frame (ADR-10050): the bound
        // on the rest is the same 9 bytes (measured 7.51 before the stamp, 9.51 with it).
        let mean_last = host_last.frame as f64 / host_last.packets as f64;
        assert!(
            mean_last <= 9.0 + crate::ack::ARRIVAL_STAMP_BYTES as f64,
            "{}",
            host_last.line("host, last minute")
        );
        assert!(
            host_run.frame_max <= 16 && host_run.ranges_max <= 4,
            "{}",
            host_run.line("host")
        );
        assert!(
            seat_run.frame_max <= 16 && seat_run.ranges_max <= 4,
            "{}",
            seat_run.line("seat")
        );
        assert_eq!(hs.ack_frame_bytes, host_run.frame, "the counter matches the wire");
        // Loss recovery and accounting are unchanged.
        assert_eq!((hs.spurious_losses, ss.spurious_losses), (0, 0));
        assert!(hs.packets_lost <= host_dropped && hs.packets_lost + 5 >= host_dropped);
        assert!(ss.packets_lost <= seat_dropped && ss.packets_lost + 5 >= seat_dropped);
        assert_eq!(
            reliable_got,
            (0..reliable_sent).collect::<Vec<_>>(),
            "in order, none missing"
        );
    }

    /// One packet each way: `from` polls at `now` and every packet arrives at `to` at once.
    fn hop(from: &mut Connection, to: &mut Connection, now: u64) -> Vec<Vec<u8>> {
        let packets = from.poll(now, &[]);
        for p in &packets {
            to.receive(p, now).expect("packet");
        }
        packets
    }

    /// A packet that fills a gap after the frame naming the gap's neighbours left is still
    /// acknowledged when that frame is confirmed; the confirmed numbers are not named again, and
    /// a frame whose largest is no longer the newest arrival takes no round-trip sample.
    #[test]
    fn a_late_arrival_inside_a_confirmed_frame_is_still_acknowledged() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        // a sends ten data packets; the sixth is held back.
        let mut held = None;
        for i in 0..10u64 {
            a.send_essential(vec![1; 20], 0).expect("queued");
            for p in a.poll(i, &[]) {
                if i == 5 {
                    held = Some(p);
                } else {
                    b.receive(&p, i).expect("packet");
                }
            }
        }
        // b's data packet carries a frame naming 0-4 and 6-9.
        b.send_essential(vec![2; 20], 0).expect("queued");
        let sent = hop(&mut b, &mut a, 10);
        assert_eq!(sent.len(), 1);
        assert!(
            a.sent
                .iter()
                .filter(|p| p.seq <= 9 && p.seq != 5)
                .all(|p| p.acked)
        );
        // The held packet arrives after that frame left, before it is confirmed.
        b.receive(&held.expect("held packet"), 11).expect("late packet");
        // a's next packet acknowledges b's: b now knows a heard of 0-4 and 6-9.
        a.send_essential(vec![1; 20], 0).expect("queued");
        hop(&mut a, &mut b, 12);
        let frame = b.ack_frame(12).expect("5 and 10 are still owed");
        assert!(frame.contains(5), "{:?}", frame.ranges);
        assert!(frame.contains(10), "{:?}", frame.ranges);
        assert!(
            (0..=9).filter(|n| *n != 5).all(|n| !frame.contains(n)),
            "{:?}",
            frame.ranges
        );
        b.send_essential(vec![2; 20], 0).expect("queued");
        hop(&mut b, &mut a, 13);
        let stats = a.stats();
        assert_eq!(
            (stats.packets_lost, stats.spurious_losses),
            (0, 1),
            "the late packet is acknowledged and its loss verdict undone"
        );
        // a's next packet confirms that frame too: b names only a's newest packet (11).
        a.send_essential(vec![1; 20], 0).expect("queued");
        hop(&mut a, &mut b, 14);
        assert_eq!(b.ack_ranges.newest(8).collect::<Vec<_>>(), vec![(11, 11)]);
        // A late arrival below the newest confirmed number makes a frame without a delay sample.
        let mut c = Connection::new(LinkConfig::default());
        for n in [0u64, 1, 2] {
            c.receive(&forged_ping(n), 0).expect("packet");
        }
        c.ack_ranges.remove(0, 2);
        c.receive(&forged_ping(4), 1).expect("packet");
        c.ack_ranges.remove(4, 4);
        // The late packet also asks for receive timestamps.
        let mut late = forged_ping(3);
        late[0] |= FLAG_WANT_TIMESTAMPS;
        c.receive(&late, 2).expect("late packet");
        assert!(!c.frame_is_current());
        let packets = c.poll(30, &[]);
        let flags = packets.first().expect("acknowledgment")[0];
        assert_ne!(flags & FLAG_ACK, 0);
        assert_ne!(
            flags & FLAG_ACK_OLDER,
            0,
            "no round-trip sample from an older largest"
        );
        assert_eq!(
            flags & FLAG_ACK_TIMESTAMPS,
            0,
            "timestamps are relative to the newest arrival: they wait for a current frame"
        );
        assert!(!c.pending_timestamps.is_empty());
        assert_eq!(c.ack_frame(30).expect("frame").delay_us, 0);
    }

    /// A forged wire-5 ping packet with packet number `pn` (four-byte number, no frame).
    fn forged_ping(pn: u64) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.u8(2);
        w.u32(pn as u32);
        w.u8(RECORD_PING);
        w.finish()
    }

    /// When more out-of-order arrivals came after a frame than are remembered, its confirmation
    /// removes nothing (one of them might sit in its span); the next confirmation trims.
    #[test]
    fn a_confirmation_is_skipped_when_late_arrivals_were_forgotten() {
        let mut c = Connection::new(LinkConfig::default());
        c.note_arrival(10_000, None);
        let old = FrameNamed {
            lo: 10_000,
            hi: 10_000,
            arrivals: c.arrivals,
        };
        // Every second number from 0 up arrives late (below 10,000).
        for n in 0..(LATE_ARRIVALS_KEPT as u64 + 2) {
            c.note_arrival(n * 2, Some(10_000));
        }
        assert!(c.late_forgotten > old.arrivals);
        c.frame_confirmed(old);
        assert!(c.ack_ranges.contains(10_000), "kept: the record is incomplete");
        let newer = FrameNamed {
            lo: 0,
            hi: 10_000,
            arrivals: c.arrivals,
        };
        c.frame_confirmed(newer);
        assert!(
            c.ack_ranges.is_empty(),
            "{:?}",
            c.ack_ranges.newest(4).collect::<Vec<_>>()
        );
        // A late arrival after a frame, inside its span, survives that frame's confirmation.
        c.note_arrival(20_000, Some(10_000));
        c.note_arrival(30_000, Some(20_000));
        let frame = FrameNamed {
            lo: 20_000,
            hi: 30_000,
            arrivals: c.arrivals,
        };
        c.note_arrival(25_000, Some(30_000));
        c.frame_confirmed(frame);
        assert_eq!(c.ack_ranges.newest(4).collect::<Vec<_>>(), vec![(25_000, 25_000)]);
    }
}

#[cfg(test)]
mod uplink_cap_tests {
    //! ADR-8534: a connection under a shared uplink's cap.
    use super::*;

    /// A peer that joins a listen server whose uplink the other peers already fill gets its fair
    /// share from its first packet: its pacer keeps no initial window above the bucket depth.
    /// Before ADR-8534 the first poll sent the whole initial window (64 packets, about 77 kB) on
    /// top of the aggregate: the 8 clients that rejoin listen-uplink at 8 s put that second over
    /// the 20 Mbit/s uplink.
    #[test]
    fn a_capped_connection_sends_no_first_burst_over_its_share() {
        let mut c = Connection::new(blind_link());
        for _ in 0..40 {
            c.send(Channel::GAME_RPC, vec![7; 1_000], 0).expect("queued");
        }
        let share = 35_000.0;
        c.set_rate_cap(Some(share));
        let first: usize = c.poll(0, &[]).iter().map(Vec::len).sum();
        let packet = c.config.max_packet_bytes;
        // One tick of the pacer at 1.25x the share, at least `PACING_BURST_PACKETS` packets
        // (four since ADR-8692), and the packet that took the bucket into debt.
        let tick = c.unreliable_lifetime_ms as f64 / 1_000.0;
        let depth =
            (share * crate::cc::PACING_GAIN * tick).max((crate::cc::PACING_BURST_PACKETS * packet) as f64);
        assert!(
            first as f64 <= depth + packet as f64,
            "first poll sent {first} bytes, depth {depth:.0}"
        );
        // Without the cap the initial window leaves at once.
        let mut free = Connection::new(blind_link());
        for _ in 0..40 {
            free.send(Channel::GAME_RPC, vec![7; 1_000], 0).expect("queued");
        }
        let burst: usize = free.poll(0, &[]).iter().map(Vec::len).sum();
        assert!(burst >= 30_000, "uncapped first poll {burst}");
    }

    /// ADR-10145: the first burst follows the start rate. A connection at the default start rate
    /// sends a 16 KiB message in its first poll, as before. With the start rate at 32 KiB/s (a
    /// project's setting for thin links) the first poll carries at most 300 ms of that rate, eight
    /// packets: before, the 64-packet initial window stood in front of the lowered start rate and
    /// the whole message left at once into whatever the path's buffer held.
    #[test]
    fn the_first_burst_follows_the_start_rate() {
        let message = 16 * 1024;
        let mut default = Connection::new(blind_link());
        default
            .send(Channel::GAME_RPC, vec![7; message], 0)
            .expect("queued");
        let burst: usize = default.poll(0, &[]).iter().map(Vec::len).sum();
        assert!(
            burst >= message,
            "the default first poll sent {burst} of {message} bytes"
        );

        let mut config = blind_link();
        config.rate.start_bytes_per_second = 32 * 1024;
        let packet = config.max_packet_bytes;
        let allowed = config.rate.start_burst_bytes(packet);
        assert_eq!(allowed, 9_830, "300 ms of 32 KiB/s");
        let mut cautious = Connection::new(config);
        cautious
            .send(Channel::GAME_RPC, vec![7; message], 0)
            .expect("queued");
        let first: usize = cautious.poll(0, &[]).iter().map(Vec::len).sum();
        assert!(first > 0, "the message starts");
        assert!(
            first <= allowed + packet,
            "the first poll sent {first} bytes at a 32 KiB/s start rate (allowed {allowed})"
        );
        // The floor: never fewer than the pacer's burst packets, however low the start rate.
        let mut low = blind_link();
        low.rate.start_bytes_per_second = 4 * 1024;
        assert_eq!(
            low.rate.start_burst_bytes(packet),
            crate::cc::PACING_BURST_PACKETS * packet
        );
        // The default start rate allows the whole initial window.
        let d = blind_link();
        assert!(d.rate.start_burst_bytes(packet) >= d.max_packets_per_poll * packet);
        // Whatever the link's window is: a link configured for 512 packets a poll sends 240 kB of
        // unreliable messages in its first polls, as before ADR-10145 (the first session's bound
        // of 300 ms of the default start rate, 78.6 kB, refused the 131st of these with
        // `Backpressure`: `unify_tests::a_capped_acknowledgment_train_never_skips_a_range`).
        let mut wide = Connection::new(LinkConfig {
            max_packets_per_poll: 512,
            ..blind_link()
        });
        let mut left = 0;
        for _ in 0..4 {
            for _ in 0..100 {
                wide.send(Channel::Unreliable, vec![1; 600], 0).expect("queued");
            }
            left += wide.poll(0, &[]).len();
        }
        assert_eq!(left, 400, "every message left in the poll after its send");
    }

    /// ADR-10200: a server counts a peer as starved by its uplink after eight of the path's
    /// minimum round trips (plus the acknowledgment delay) without an acknowledgment, within
    /// 250 ms to 1 s; before any round trip is known, after 1 s as before (ADR-10097).
    #[test]
    fn a_peer_is_starved_after_eight_round_trips_within_bounds() {
        let mut c = Connection::new(LinkConfig::default());
        assert_eq!(
            c.starved_after_ms(),
            crate::cc::UPLINK_STARVED_MS,
            "no round trip yet"
        );
        for (min, want) in [(1.0, 250), (15.0, 250), (100.0, 880), (400.0, 1_000)] {
            c.min_rtt.clear();
            c.min_rtt.push_back((0, min));
            assert_eq!(c.starved_after_ms(), want, "minimum round trip {min} ms");
        }
    }

    /// ADR-10156: a first flight. With `rate.first_flight` on, a connection at the default start
    /// rate sends a 16 KiB message as a first burst of eight packets and then at the floor's
    /// rate until the path answers: 100 ms later, with no acknowledgment, about 14 kB are out.
    /// Off (the default, ADR-10231), the whole message leaves in the first poll
    /// (`the_first_burst_follows_the_start_rate`).
    #[test]
    fn a_first_flight_sends_eight_packets_and_then_the_floor_until_the_path_answers() {
        let message = 16 * 1024;
        let mut config = LinkConfig::default();
        assert!(!config.rate.first_flight, "off by default (ADR-10231)");
        config.rate.first_flight = true;
        let packet = config.max_packet_bytes;
        let allowed = config.rate.start_burst_bytes(packet);
        assert_eq!(allowed, 9_830, "300 ms of the 32 KiB/s floor");
        let mut c = Connection::new(config);
        c.send(Channel::GAME_RPC, vec![7; message], 0).expect("queued");
        let first: usize = c.poll(0, &[]).iter().map(Vec::len).sum();
        assert!(first > 0, "the message starts");
        assert!(
            first <= allowed + packet,
            "the first poll of a first flight sent {first} bytes (allowed {allowed})"
        );
        assert!(c.rate.first_flight(), "nothing has answered yet");
        // The next 100 ms, unanswered: the pacer refills at 1.25 times the floor.
        let mut sent = first;
        for now in (10..=100).step_by(10) {
            sent += c.poll(now, &[]).iter().map(Vec::len).sum::<usize>();
        }
        let paced = (f64::from(32 * 1024_u32) * crate::cc::PACING_GAIN * 0.1) as usize;
        assert!(
            sent <= allowed + paced + 2 * packet,
            "{sent} bytes left in the first 100 ms of a first flight (burst {allowed}, paced {paced})"
        );
        assert!(
            sent < message,
            "the path has not answered: part of the message waits"
        );
    }

    /// ADR-8592: essential data (inputs) that outruns the pacing rate marks the poll held, so the
    /// controller's round is filled (not app-limited) even when nothing but the essential class
    /// waits. Before ADR-8592 only ordinary unreliable data and reliable fragments counted: a
    /// client whose input bundles outran its target reported every round as unheld, and the
    /// startup never read the pipe from them. The same inputs under a rate with room hold
    /// nothing.
    #[test]
    fn essential_data_over_the_pacing_rate_holds_the_poll() {
        fn holds(cap: Option<f64>) -> u64 {
            let mut c = Connection::new(blind_link());
            c.set_rate_cap(cap);
            for step in 0..20u64 {
                // Two 1,000-byte input bundles per 10 ms step: 200 kB/s.
                for _ in 0..2 {
                    c.send_essential(vec![1; 1_000], 0).expect("queued");
                }
                let _ = c.poll(step * 10, &[]);
            }
            c.stats().paced_holds
        }
        assert!(holds(Some(40_000.0)) > 0, "inputs over a 40 kB/s rate");
        assert_eq!(holds(None), 0, "the same inputs under the 256 KiB/s start rate");
    }

    /// ADR-8692: the `rpc_p95_with_bulk` up leg. A client that idled for two seconds at a
    /// 16 kB/s pacing rate (its app-limited target during a bulk download on `congested`) queues
    /// a 4 KiB command: the whole command leaves in that poll, because the bucket holds at least
    /// `PACING_BURST_PACKETS` packets. With the old two-packet floor the rest trickled out at
    /// 1.25x the target, one packet per ~60 ms: the soak measured its last fragment waiting up to
    /// 272 ms at the client's own pacer on an idle 1 Mbit/s uplink. The bucket still bounds the
    /// poll: a second command queued with it waits.
    #[test]
    fn a_four_kib_command_after_an_idle_spell_leaves_in_one_poll() {
        let mut c = Connection::new(LinkConfig::default());
        c.set_rate_cap(Some(16_000.0));
        let _ = c.poll(0, &[]);
        let _ = c.poll(1_000, &[]);
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        c.send(Channel::GAME_RPC, vec![4; command], 0).expect("queued");
        let sent: usize = c.poll(2_000, &[]).iter().map(Vec::len).sum();
        assert!(
            sent >= command,
            "{sent} bytes left in the command's poll, the command is {command}"
        );
        let packet = c.config.max_packet_bytes;
        assert!(
            sent <= (crate::cc::PACING_BURST_PACKETS + 1) * packet,
            "{sent} bytes: the bucket lets one burst out, not both commands"
        );
    }

    /// A connection whose target is 16 kB/s (its project rate; no shared uplink) that idled, then
    /// sent four 1,000-byte input bundles (the essential class) at 2 s: the pacing bucket's four
    /// packets are nearly spent, and the idle spell left the excess allowance full.
    fn paced_with_a_spent_bucket() -> Connection {
        let mut config = LinkConfig::default();
        config.rate.max_bytes_per_second = 16_000;
        let mut c = Connection::new(config);
        // Past the first burst: the buckets hold their depth, not the initial window.
        let depth = (crate::cc::PACING_BURST_PACKETS * c.config.max_packet_bytes) as f64;
        c.pacer.limit(depth);
        c.budget.limit(depth);
        let _ = c.poll(0, &[]);
        let _ = c.poll(1_000, &[]);
        for _ in 0..4 {
            c.send_essential(vec![1; 1_000], 0).expect("queued");
        }
        let inputs: usize = c.poll(2_000, &[]).iter().map(Vec::len).sum();
        assert!(inputs >= 4_000, "the input bundles left: {inputs} bytes");
        c
    }

    /// ADR-8859: the burst a foreground message needs is the message's, not what the bucket has
    /// left. A client's re-bundled inputs drain the bucket it shares with its reliable streams;
    /// a 4 KiB command queued one tick later met about 1,000 tokens, sent one packet and trickled
    /// the rest out at 1.25x the target (soak congested+bulk seed 42: the last fragment waited
    /// 304 ms at a 20.6 kB/s target, a 400 ms command). Now the pacer lets the message start and
    /// the rest of it follows in the same poll, every packet charged: the buckets go into debt
    /// and the rate over time is the same. A second command queued with it waits for the bucket.
    #[test]
    fn a_four_kib_command_behind_an_input_burst_leaves_in_one_poll() {
        let mut c = paced_with_a_spent_bucket();
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        c.send(Channel::GAME_RPC, vec![4; command], 0).expect("queued");
        let sent: usize = c.poll(2_016, &[]).iter().map(Vec::len).sum();
        assert!(
            sent >= command,
            "{sent} bytes left in the command's poll, the command is {command}"
        );
        let packet = c.config.max_packet_bytes;
        assert!(
            sent <= (crate::cc::PACING_BURST_PACKETS + 1) * packet,
            "{sent} bytes: one message's burst, not both commands"
        );
        // The debt is real: nothing more leaves in the next ticks.
        for now in [2_032, 2_048] {
            let more: usize = c.poll(now, &[]).iter().map(Vec::len).sum();
            assert_eq!(more, 0, "{more} bytes at {now} ms while the buckets are in debt");
        }
    }

    /// ADR-8859: the allowance is `PACING_BURST_PACKETS` packets from the message's start, once.
    /// A 16 KiB message that starts on a nearly spent bucket sends that many packets in its first
    /// poll and nothing in the next ticks: the rest waits for the pacer like any backlog.
    #[test]
    fn a_long_message_takes_the_burst_floor_once() {
        let mut c = paced_with_a_spent_bucket();
        c.send(Channel::GAME_RPC, vec![5; 16 * 1024], 0).expect("queued");
        let first = c.poll(2_016, &[]);
        assert_eq!(
            first.len(),
            crate::cc::PACING_BURST_PACKETS,
            "packets of the message's first poll"
        );
        for now in [2_032, 2_048] {
            assert!(
                c.poll(now, &[]).is_empty(),
                "nothing at {now} ms: the buckets are in debt"
            );
        }
    }

    /// ADR-8859: the excess allowance fills only from target the sender left unused. Two seconds
    /// of input bundles at 20 kB/s against a 16 kB/s target leave none, so a 4 KiB command paces
    /// packet by packet as before: a saturating sender's bursts would land on a full link.
    #[test]
    fn a_sender_that_fills_its_target_earns_no_message_burst() {
        let mut config = LinkConfig::default();
        config.rate.max_bytes_per_second = 16_000;
        let mut c = Connection::new(config);
        let depth = (crate::cc::PACING_BURST_PACKETS * c.config.max_packet_bytes) as f64;
        c.pacer.limit(depth);
        c.budget.limit(depth);
        let mut now = 0;
        while now < 2_000 {
            c.send_essential(vec![1; 320], 0).expect("queued");
            let _ = c.poll(now, &[]);
            now += 16;
        }
        assert!(c.message_credit <= 0.0, "excess earned: {}", c.message_credit);
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        let sent: usize = c.poll(now, &[]).iter().map(Vec::len).sum();
        assert!(
            sent < command,
            "{sent} bytes left at once from a sender that fills its target"
        );
    }

    /// ADR-8859 review: the guard above calls `Pacer::limit` before the first poll, which drops
    /// the initial window to the bucket depth. Production does that only when `rate_cap` is set
    /// (a shared uplink's share). A fresh connection keeps the initial window above the depth.
    /// `Pacer::refill_overflow` then returned every newly earned token as unused, because the
    /// surplus was already at the cap, and the sends of that same poll were not subtracted. A
    /// sender that queued more than its target on every tick therefore banked a full message
    /// burst (4,800 bytes) while it was filling the target. The burst is what the guard says must
    /// not happen: it lands on a full link. ADR-10063 counts only what an interval earned beyond
    /// what the sender spent in it.
    #[test]
    fn a_saturating_sender_from_a_fresh_bucket_earns_no_message_burst() {
        let mut config = blind_link();
        config.rate.max_bytes_per_second = 16_000;
        let mut c = Connection::new(config);
        // One poll spends the initial window. Fewer messages than the unreliable queue cap.
        for _ in 0..40 {
            c.send_essential(vec![1; 1_000], 0).expect("queued");
        }
        let drained: usize = c.poll(0, &[]).iter().map(Vec::len).sum();
        assert!(drained > 20_000, "the initial window left: {drained} bytes");
        let mut now = 16u64;
        while now < 2_000 {
            c.send_essential(vec![1; 320], 0).expect("queued");
            let _ = c.poll(now, &[]);
            now += 16;
        }
        assert!(
            c.message_credit <= 0.0,
            "excess earned while filling the target: {}",
            c.message_credit
        );
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        let mut peak = 0usize;
        for _ in 0..80 {
            let sent: usize = c.poll(now, &[]).iter().map(Vec::len).sum();
            peak = peak.max(sent);
            now += 16;
            if sent >= command {
                break;
            }
        }
        assert!(
            peak < command,
            "{peak} bytes left in one poll after two seconds filling a 16 kB/s target"
        );
    }

    /// ADR-8859: under a shared uplink's share the unused part belongs to the other peers
    /// (ADR-8534's max-min shares), so it earns no excess allowance: the same command behind the
    /// same input burst sends what the bucket has left and paces the rest.
    #[test]
    fn a_shared_uplink_share_earns_no_message_burst() {
        let mut c = Connection::new(LinkConfig::default());
        c.set_rate_cap(Some(16_000.0));
        let _ = c.poll(0, &[]);
        let _ = c.poll(1_000, &[]);
        for _ in 0..4 {
            c.send_essential(vec![1; 1_000], 0).expect("queued");
        }
        let _ = c.poll(2_000, &[]);
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        let sent: usize = c.poll(2_016, &[]).iter().map(Vec::len).sum();
        assert!(sent > 0, "the bucket's rest leaves");
        assert!(
            sent < command,
            "{sent} bytes left at once under a shared uplink's share"
        );
    }

    /// ADR-10063 (review nit): `set_shared_uplink(true)` alone, with no share in force, earns no
    /// excess allowance either, and drops what was banked: the peers split one uplink, so what
    /// one leaves unused is the aggregate's. Before, only a `Some` rate cap zeroed the credit.
    #[test]
    fn a_shared_uplink_flag_without_a_share_earns_no_message_burst() {
        let mut c = paced_with_a_spent_bucket();
        assert!(c.message_credit > 0.0, "the idle second banked an allowance");
        c.set_shared_uplink(true);
        assert_eq!(c.message_credit, 0.0, "the flag drops the banked allowance");
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        let sent: usize = c.poll(2_016, &[]).iter().map(Vec::len).sum();
        assert!(sent > 0, "the bucket's rest leaves");
        assert!(
            sent < command,
            "{sent} bytes left at once with the shared-uplink flag set"
        );
        assert_eq!(c.message_credit, 0.0, "nothing earned while the flag is set");
        let mut d = Connection::new(LinkConfig::default());
        d.set_shared_uplink(true);
        let _ = d.poll(0, &[]);
        let _ = d.poll(1_000, &[]);
        assert_eq!(
            d.message_credit, 0.0,
            "an idle second under the flag earns nothing"
        );
    }

    /// ADR-10063 (review nit): a burst packet needs the excess allowance to hold it whole (RFC
    /// 2697 § 3). Before, any positive credit let a whole packet out: 100 bytes of credit sent a
    /// ~1,200-byte packet and left the credit 1,100 bytes in debt.
    #[test]
    fn a_few_bytes_of_credit_let_no_burst_packet_out() {
        let mut c = paced_with_a_spent_bucket();
        c.message_credit = 100.0;
        let command = 4_096;
        c.send(Channel::GAME_RPC, vec![3; command], 0).expect("queued");
        let sent: usize = c.poll(2_016, &[]).iter().map(Vec::len).sum();
        assert!(sent > 0, "the pacer lets the message start");
        assert!(sent < command, "{sent} bytes: no burst on 100 bytes of credit");
        assert!(
            c.message_credit >= 0.0,
            "the burst overdrew the allowance: {}",
            c.message_credit
        );
    }

    /// ADR-8734: an unvalidated packet that breaches a stream's window is refused before it moves
    /// any state: no failure, and the receive clock (the stall interval's start) stays where the
    /// last good packet set it. The same packet on the validated path still fails the link.
    #[test]
    fn an_unvalidated_packet_over_a_stream_window_leaves_the_link_as_it_was() {
        fn forged(pn: u32, id: u16, bytes: &[u8]) -> Vec<u8> {
            let mut w = WireWriter::new();
            w.u8(2);
            w.u32(pn);
            // One reliable fragment on the chat stream, not the first of its message, more to come.
            w.u8(0x40 | StreamId::CHAT.0 | 0x80);
            w.u16(id);
            w.u16(bytes.len() as u16);
            w.raw(bytes);
            w.finish()
        }
        let chunk = vec![7u8; 1_100];
        let mut b = Connection::new(LinkConfig::default());
        let fits = 256 * 1024 / 1_100;
        // Id 0 never comes: ids 1..=fits are stored behind the gap, the window nearly full.
        for id in 1..=fits as u16 {
            b.receive(&forged(u32::from(id), id, &chunk), u64::from(id))
                .expect("inside the window");
        }
        let last = b.last_rx_ms;
        assert_eq!(last, Some(fits as u64));
        let breach = forged(fits as u32 + 1, fits as u16 + 1, &chunk);
        let mut budget = ReassemblyBudget::unlimited();
        assert!(b.receive_unvalidated_within(&breach, 5_000, &mut budget).is_err());
        assert_eq!(b.failure(), None, "an unvalidated breach does not fail the link");
        assert_eq!(b.last_rx_ms, last, "and does not move the receive clock");
        assert!(b.receive_within(&breach, 5_000, &mut budget).is_err());
        assert_eq!(
            b.failure(),
            Some(LinkFailure::StreamOverCap),
            "the validated path still fails"
        );
    }
}

#[cfg(test)]
mod scale_work_tests {
    //! The per-poll work of one connection stays flat as its history grows (ADR-10081, spec 120
    //! § 1.1 "It scales"): counted in records examined, never in wall time.
    use super::*;

    const H: &[u8] = &[0xAA, 0xBB];

    /// A deterministic pattern: true for about `per_mille` of the calls.
    struct Lcg(u64);

    impl Lcg {
        fn hit(&mut self, per_mille: u64) -> bool {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % 1000 < per_mille
        }
    }

    /// One 30 Hz tick of a game: `a` sends a state message, `b` an input; `lose` drops a's
    /// packets by count.
    fn tick(
        a: &mut Connection,
        b: &mut Connection,
        now: u64,
        sent: &mut u64,
        lose: &mut dyn FnMut(u64) -> bool,
    ) {
        a.send(Channel::Unreliable, vec![1; 200], H.len())
            .expect("queued");
        for p in a.poll(now, H) {
            *sent += 1;
            if !lose(*sent) {
                b.receive(&p[H.len()..], now).expect("valid packet");
            }
        }
        b.send(Channel::Unreliable, vec![2; 50], H.len()).expect("queued");
        for p in b.poll(now, H) {
            a.receive(&p[H.len()..], now).expect("valid packet");
        }
    }

    /// One lost packet keeps its record, and every record behind it, for late acknowledgments
    /// (`LATE_ACK_WINDOW`). The loss detector must not walk that history again on every poll:
    /// only packets whose fate is still open are examined. Before ADR-10081 each poll examined
    /// every tracked record below the largest acknowledged number, hundreds per poll on any path
    /// that ever lost a packet (46 % of the server's flush at 128 clients).
    #[test]
    fn a_poll_examines_only_packets_whose_fate_is_open() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let mut sent = 0;
        for t in 0..600u64 {
            tick(&mut a, &mut b, t * 33, &mut sent, &mut |n| n == 5);
        }
        let tracked = a.sent.len();
        assert!(
            tracked >= 400,
            "the lost packet holds the history: {tracked} records tracked"
        );
        let before = a.work().loss_scan_records;
        let polls = 100u64;
        for t in 600..600 + polls {
            tick(&mut a, &mut b, t * 33, &mut sent, &mut |_| false);
        }
        let examined = a.work().loss_scan_records - before;
        // Each tick polls `a` once; a packet or two may wait for its acknowledgment.
        assert!(
            examined <= 4 * polls,
            "{examined} sent-packet records examined in {polls} polls with {tracked} tracked \
             ({} per poll); only packets whose fate is open may be examined",
            examined / polls
        );
    }

    /// The scan's start is exact: on a path that loses, reorders and delays packets, every
    /// tracked record below it has its verdict after every poll and every acknowledgment, so the
    /// shorter scan declares exactly what the full one did.
    #[test]
    fn no_record_below_the_scan_start_is_without_a_verdict() {
        let mut a = Connection::new(LinkConfig::default());
        let mut b = Connection::new(LinkConfig::default());
        let mut rng = Lcg(7);
        // a's packets in flight to b: (arrival time, body).
        let mut flight: Vec<(u64, Vec<u8>)> = Vec::new();
        let check = |a: &Connection, at: &str, now: u64| {
            let open = a
                .sent
                .iter()
                .take_while(|p| p.seq < a.loss_scan_from)
                .find(|p| !(p.acked || p.lost));
            assert!(
                open.is_none(),
                "{at} at {now} ms: packet {} has no verdict below the scan start {}",
                open.map_or(0, |p| p.seq),
                a.loss_scan_from
            );
        };
        let (mut lost, mut late) = (0u64, 0u64);
        for t in 0..3_000u64 {
            let now = t * 16;
            if t.is_multiple_of(2) {
                a.send(Channel::Unreliable, vec![1; 120], H.len())
                    .expect("queued");
            }
            if t.is_multiple_of(40) {
                a.send(Channel::GAME_RPC, vec![3; 700], H.len()).expect("queued");
            }
            for p in a.poll(now, H) {
                if rng.hit(60) {
                    lost += 1;
                    continue;
                }
                // Most arrive in 30 ms; some are held long enough to be declared lost first.
                let delay = if rng.hit(40) {
                    late += 1;
                    400
                } else if rng.hit(200) {
                    90
                } else {
                    30
                };
                flight.push((now + delay, p));
            }
            check(&a, "after a poll", now);
            let mut i = 0;
            while i < flight.len() {
                if flight[i].0 <= now {
                    let (_, p) = flight.swap_remove(i);
                    b.receive(&p[H.len()..], now).expect("valid packet");
                } else {
                    i += 1;
                }
            }
            b.send(Channel::Unreliable, vec![2; 40], H.len()).expect("queued");
            for p in b.poll(now, H) {
                if !rng.hit(60) {
                    a.receive(&p[H.len()..], now).expect("valid packet");
                    check(&a, "after an acknowledgment", now);
                }
            }
        }
        let stats = a.stats();
        assert!(lost > 50 && late > 30, "the path lost {lost} and held {late}");
        assert!(stats.packets_lost > 50, "losses were declared: {stats:?}");
        assert!(
            stats.spurious_losses > 0,
            "late acknowledgments undid some: {stats:?}"
        );
        assert!(
            a.loss_scan_from > 1_000,
            "the scan start moved: {}",
            a.loss_scan_from
        );
    }
}

/// Repeat repairs are staggered (ADR-10088).
#[cfg(test)]
mod repeat_repair_tests {
    use super::*;

    /// One-way delay of the test link, milliseconds: a 100 ms round trip.
    const DELAY_MS: u64 = 50;

    /// Two connections over a fixed-delay link, polled every millisecond. `a` keeps a small
    /// unreliable message flowing so `b` acknowledges; while `lose_reliable` is set, every packet
    /// of `a` that carries a reliable record is lost on the way.
    struct Sim {
        a: Connection,
        b: Connection,
        to_b: VecDeque<(u64, Vec<u8>)>,
        to_a: VecDeque<(u64, Vec<u8>)>,
        now: u64,
        lose_reliable: bool,
        /// `(time, packets with reliable records)` of every poll of `a` that sent one.
        reliable_polls: Vec<(u64, usize)>,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                a: Connection::new(LinkConfig::default()),
                b: Connection::new(LinkConfig::default()),
                to_b: VecDeque::new(),
                to_a: VecDeque::new(),
                now: 0,
                lose_reliable: false,
                reliable_polls: Vec::new(),
            }
        }

        fn step(&mut self) {
            self.now += 1;
            let now = self.now;
            while self.to_b.front().is_some_and(|(at, _)| *at <= now) {
                if let Some((_, p)) = self.to_b.pop_front() {
                    self.b.receive(&p, now).expect("a's packet");
                }
            }
            while self.to_a.front().is_some_and(|(at, _)| *at <= now) {
                if let Some((_, p)) = self.to_a.pop_front() {
                    self.a.receive(&p, now).expect("b's packet");
                }
            }
            if now.is_multiple_of(5) {
                self.a.send(Channel::Unreliable, vec![1], 0).expect("queued");
            }
            let mut reliable = 0;
            for p in self.a.poll(now, &[]) {
                if test_reliable_records(&p) > 0 {
                    reliable += 1;
                    if self.lose_reliable {
                        continue;
                    }
                }
                self.to_b.push_back((now + DELAY_MS, p));
            }
            if reliable > 0 {
                self.reliable_polls.push((now, reliable));
            }
            for p in self.b.poll(now, &[]) {
                self.to_a.push_back((now + DELAY_MS, p));
            }
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms {
                self.step();
            }
        }
    }

    /// Three full fragments leave in one poll and are lost; so is every repair. The first repair
    /// leaves as before, together, one round trip later. From the second loss in a row a repair
    /// leaves a stagger later than the round trip: a fragment's repairs are not one fixed
    /// interval apart (a repair exactly one round trip after the last meets the same phase of a
    /// periodic bottleneck, the traffic phase effect), the fragments of the burst spread over
    /// several polls instead of leaving back to back every round, and no repair waits more than
    /// 7/32 of a round trip longer than before. A fragment still leaves at most once per round
    /// trip, whatever is lost (ADR-7995). When the path carries reliable data again the message
    /// arrives whole, once.
    #[test]
    fn a_repair_lost_again_leaves_off_the_beat_and_still_once_per_round_trip() {
        let mut sim = Sim::new();
        sim.run(400);
        let srtt = sim.a.round_trip_ms().expect("a measured the path").ceil() as u64;
        assert!(
            (100..=115).contains(&srtt),
            "a 100 ms round trip, measured {srtt}"
        );
        let fragment = sim.a.fragment_bytes(0);
        assert!(
            fragment > REDUNDANT_COPY_MAX_BYTES,
            "full fragments take no redundant copy"
        );
        let message = vec![7u8; 3 * fragment];
        sim.lose_reliable = true;
        sim.a.send(Channel::GAME_RPC, message.clone(), 0).expect("queued");
        assert_eq!(sim.a.reliable_out.len(), 3, "three full fragments");
        // Every transmission of each fragment: when, and after how many losses in a row, read
        // from the queue after each poll.
        let mut sent: Vec<Vec<(u64, u8)>> = vec![Vec::new(); 3];
        for _ in 0..1_200 {
            sim.step();
            for (i, frag) in sim.a.reliable_out.iter().enumerate() {
                if frag.last_sent_ms == Some(sim.now) {
                    sent[i].push((sim.now, frag.losses));
                }
            }
        }
        assert_eq!(
            sim.reliable_polls.iter().map(|(_, n)| n).sum::<usize>(),
            sent.iter().map(Vec::len).sum::<usize>(),
            "full fragments ride one per packet"
        );
        let first_flight: Vec<u64> = sent.iter().map(|s| s[0].0).collect();
        assert!(
            first_flight.iter().all(|t| *t == first_flight[0]),
            "the first flight leaves in one poll: {first_flight:?}"
        );
        let stagger_max = srtt * (REPEAT_REPAIR_STAGGER_SLOTS - 1) / REPEAT_REPAIR_STAGGER_DIVISOR;
        for (i, sends) in sent.iter().enumerate() {
            assert!(sends.len() >= 8, "fragment {i} was sent {sends:?}");
            let mut repeat_gaps = Vec::new();
            for pair in sends.windows(2) {
                let ((before, _), (at, losses)) = (pair[0], pair[1]);
                let gap = at - before;
                // One round trip and the loss verdict after the transmission it replaces, plus
                // the stagger from the second loss on; never sooner than one round trip.
                let latest = srtt + if losses >= 2 { stagger_max } else { 0 } + 40;
                assert!(
                    (srtt..=latest).contains(&gap),
                    "fragment {i}: the repair after {losses} losses left {gap} ms after the last \
                     transmission: {sends:?}"
                );
                if losses >= 2 {
                    repeat_gaps.push(gap);
                }
            }
            repeat_gaps.sort_unstable();
            repeat_gaps.dedup();
            assert!(
                repeat_gaps.len() >= 3,
                "fragment {i}: repeat repairs came one fixed interval apart: {repeat_gaps:?}"
            );
        }
        // The burst does not stay a burst: after the first repair, most polls that carry a
        // repair carry one fragment, not all three.
        let repeat_polls: Vec<usize> = sim
            .reliable_polls
            .iter()
            .filter(|(at, _)| *at > sent[0][1].0 + srtt / 2)
            .map(|(_, n)| *n)
            .collect();
        let alone = repeat_polls.iter().filter(|n| **n == 1).count();
        assert!(
            alone * 2 > repeat_polls.len() && !repeat_polls.contains(&3),
            "repeat repairs of one burst leave apart: {repeat_polls:?}"
        );
        // The path carries reliable data again: the message arrives whole, once.
        sim.lose_reliable = false;
        sim.run(600);
        let got: Vec<Vec<u8>> = std::iter::from_fn(|| sim.b.next_message())
            .filter(|(channel, _)| *channel == Channel::GAME_RPC)
            .map(|(_, m)| m)
            .collect();
        assert_eq!(got.len(), 1, "delivered once");
        assert_eq!(got[0], message);
        assert!(!sim.a.has_unacked_reliable(), "everything was acknowledged");
    }

    /// The stagger is a pure function of the fragment's queue position and loss count: none for
    /// the first repair, then a slot of eight that differs between queue neighbors and from one
    /// loss to the next, visiting every slot in eight losses.
    #[test]
    fn the_stagger_is_none_for_the_first_repair_and_then_visits_every_slot() {
        let mut c = Connection::new(LinkConfig::default());
        c.send(Channel::GAME_RPC, vec![0; 16], 0).expect("queued");
        let spacing = 320;
        let step = spacing / REPEAT_REPAIR_STAGGER_DIVISOR;
        for losses in 0..=1 {
            c.reliable_out[0].losses = losses;
            assert_eq!(Connection::repair_stagger_ms(&c.reliable_out[0], spacing), 0);
        }
        let mut slots: Vec<u64> = (2..10u8)
            .map(|losses| {
                c.reliable_out[0].losses = losses;
                Connection::repair_stagger_ms(&c.reliable_out[0], spacing) / step
            })
            .collect();
        assert!(
            slots.windows(2).all(|w| w[0] != w[1]),
            "a new slot after every loss: {slots:?}"
        );
        slots.sort_unstable();
        assert_eq!(slots, (0..REPEAT_REPAIR_STAGGER_SLOTS).collect::<Vec<_>>());
        // Queue neighbors (the fragments of one burst) take different slots at the same count.
        c.send(Channel::GAME_RPC, vec![0; 16], 0).expect("queued");
        c.reliable_out[0].losses = 2;
        c.reliable_out[1].losses = 2;
        assert_ne!(
            Connection::repair_stagger_ms(&c.reliable_out[0], spacing),
            Connection::repair_stagger_ms(&c.reliable_out[1], spacing)
        );
        // No round-trip sample yet: no spacing, no stagger.
        assert_eq!(Connection::repair_stagger_ms(&c.reliable_out[0], 0), 0);
    }
}

/// A loss repair leads its poll, and the repairs of a poll are spread over the round trip
/// (ADR-10125).
#[cfg(test)]
mod repair_lead_tests {
    use super::*;

    /// `(reliable records, unreliable records)` of a packet body.
    fn records_of(body: &[u8]) -> (usize, usize) {
        let (.., records) = parse_packet(body).expect("a well-formed packet");
        let reliable = records
            .iter()
            .filter(|r| matches!(r, Record::Reliable { .. }))
            .count();
        let unreliable = records
            .iter()
            .filter(|r| matches!(r, Record::Unreliable(_)))
            .count();
        (reliable, unreliable)
    }

    /// A server-like sender `a` and its peer `b` over a fixed-delay link. `a` polls once per
    /// tick, each time with an essential record and a state record queued, as a server does for
    /// a client; `b` polls every millisecond. While `lose_reliable` is set, every packet of `a`
    /// that carries a reliable record is lost on the way.
    struct Sim {
        a: Connection,
        b: Connection,
        to_b: VecDeque<(u64, Vec<u8>)>,
        to_a: VecDeque<(u64, Vec<u8>)>,
        now: u64,
        delay_ms: u64,
        tick_ms: u64,
        lose_reliable: bool,
        /// Every poll of `a` that sent a reliable record: its time and the records of each of
        /// its packets, in the order they left.
        reliable_polls: Vec<(u64, Vec<(usize, usize)>)>,
        /// Every poll of `a`: its time and the unreliable records it sent in all (the tick's
        /// essential record, and its state record unless that waited).
        all_polls: Vec<(u64, usize)>,
        /// The datagram sizes of each entry of `reliable_polls`.
        reliable_poll_bytes: Vec<Vec<usize>>,
    }

    impl Sim {
        fn new(delay_ms: u64, tick_hz: u32) -> Self {
            Self::with_config(delay_ms, tick_hz, LinkConfig::default())
        }

        /// A link without rate control: the pacer never holds a packet back.
        fn unpaced(delay_ms: u64, tick_hz: u32) -> Self {
            let mut config = LinkConfig::default();
            config.rate.enabled = false;
            Self::with_config(delay_ms, tick_hz, config)
        }

        fn with_config(delay_ms: u64, tick_hz: u32, config: LinkConfig) -> Self {
            let mut a = Connection::new(config);
            a.set_tick_rate(tick_hz);
            let tick_ms = a.unreliable_lifetime_ms;
            Self {
                a,
                b: Connection::new(LinkConfig::default()),
                to_b: VecDeque::new(),
                to_a: VecDeque::new(),
                now: 0,
                delay_ms,
                tick_ms,
                lose_reliable: false,
                reliable_polls: Vec::new(),
                all_polls: Vec::new(),
                reliable_poll_bytes: Vec::new(),
            }
        }

        fn step(&mut self) {
            self.now += 1;
            let now = self.now;
            while self.to_b.front().is_some_and(|(at, _)| *at <= now) {
                if let Some((_, p)) = self.to_b.pop_front() {
                    self.b.receive(&p, now).expect("a's packet");
                }
            }
            while self.to_a.front().is_some_and(|(at, _)| *at <= now) {
                if let Some((_, p)) = self.to_a.pop_front() {
                    self.a.receive(&p, now).expect("b's packet");
                }
            }
            if now.is_multiple_of(self.tick_ms) {
                self.a.send_essential(vec![9; 40], 0).expect("queued");
                self.a.send(Channel::Unreliable, vec![1; 600], 0).expect("queued");
                let packets = self.a.poll(now, &[]);
                let records: Vec<(usize, usize)> = packets.iter().map(|p| records_of(p)).collect();
                self.all_polls
                    .push((now, records.iter().map(|(_, unreliable)| unreliable).sum()));
                if records.iter().any(|(reliable, _)| *reliable > 0) {
                    self.reliable_polls.push((now, records.clone()));
                    self.reliable_poll_bytes
                        .push(packets.iter().map(Vec::len).collect());
                }
                for (p, (reliable, _)) in packets.into_iter().zip(records) {
                    if reliable > 0 && self.lose_reliable {
                        continue;
                    }
                    self.to_b.push_back((now + self.delay_ms, p));
                }
            }
            for p in self.b.poll(now, &[]) {
                self.to_a.push_back((now + self.delay_ms, p));
            }
        }

        fn run(&mut self, ms: u64) {
            for _ in 0..ms {
                self.step();
            }
        }

        fn delivered(&mut self) -> Vec<Vec<u8>> {
            std::iter::from_fn(|| self.b.next_message())
                .filter(|(channel, _)| *channel == Channel::GAME_RPC)
                .map(|(_, m)| m)
                .collect()
        }
    }

    /// A server sends an essential record and a state record every tick. A full fragment fills
    /// a packet, so in front of a repair those records used to open the poll's first packet and
    /// the repair left second: behind a queue that is full, the place that is dropped. The
    /// repair is the first packet of its poll, alone, and the essential and state records ride
    /// the packet after it in the same poll; nothing of them is delayed or dropped. First
    /// transmissions keep their order (new data never leads). The message arrives once.
    #[test]
    fn a_loss_repair_leads_its_poll_and_the_tick_s_records_ride_the_next_packet() {
        let mut sim = Sim::unpaced(50, 60);
        sim.run(600);
        let fragment = sim.a.fragment_bytes(0);
        let message = vec![7u8; 2 * fragment];
        sim.lose_reliable = true;
        sim.a.send(Channel::GAME_RPC, message.clone(), 0).expect("queued");
        sim.run(1_500);
        let polls = sim.reliable_polls.clone();
        assert!(
            polls.len() >= 4,
            "the first flight and several repairs: {polls:?}"
        );
        // The first flight: the tick's records lead, the new fragments follow.
        assert_eq!(
            polls[0].1,
            vec![(0, 2), (1, 0), (1, 0)],
            "new data does not lead: {polls:?}"
        );
        for (at, packets) in &polls[1..] {
            assert_eq!(
                packets[0],
                (1, 0),
                "at {at} ms the repair leads its poll, alone: {packets:?}"
            );
            assert!(
                packets.len() >= 2 && packets[1].1 == 2,
                "at {at} ms the tick's essential and state records ride the next packet: {packets:?}"
            );
        }
        assert_eq!(
            sim.a.stats().unreliable_dropped,
            0,
            "no record of a tick was dropped"
        );
        sim.lose_reliable = false;
        sim.run(600);
        let got = sim.delivered();
        assert_eq!(got.len(), 1, "delivered once");
        assert_eq!(got[0], message);
        assert!(!sim.a.has_unacked_reliable(), "everything was acknowledged");
    }

    /// With rate control on, the lead must be paid for: the repair leads only a poll whose
    /// pacing allowance carries it and the tick's records behind it. Here the measured rate is
    /// small (the link carries a few hundred bytes per tick), so some polls hold less than that:
    /// they keep the old order, the tick's records first and the repair behind them. Whenever
    /// the repair does lead, the same poll still carries the essential and the state record: a
    /// leading repair never costs a tick its state (on a shared uplink's fair share, a packet per
    /// tick, it would cost one state frame per repair). The message arrives once either way.
    #[test]
    fn a_leading_repair_never_costs_the_tick_its_records() {
        // The small allowance is a shared uplink's fair share (ADR-10231 (4)): 48 kB/s, about
        // 800 bytes a tick, under the packet a full fragment fills. It used to be the loss cut
        // this thin sender took for its lost repairs; ADR-10157 no longer cuts a thin sender for
        // random loss, and at the start rate every poll's allowance carries the lead, so no
        // poll kept the old order (17 led). The link names the blind start (ADR-10200; the
        // default in v0.7).
        let mut sim = Sim::with_config(50, 60, blind_link());
        sim.run(600);
        sim.a.set_rate_cap(Some(48_000.0));
        let fragment = sim.a.fragment_bytes(0);
        let message = vec![7u8; 2 * fragment];
        sim.lose_reliable = true;
        sim.a.send(Channel::GAME_RPC, message.clone(), 0).expect("queued");
        sim.run(1_500);
        let polls = sim.reliable_polls.clone();
        assert!(
            polls.len() >= 4,
            "the first flight and several repairs: {polls:?}"
        );
        let (mut led, mut kept) = (0, 0);
        for (at, packets) in &polls[1..] {
            if packets[0] == (1, 0) {
                led += 1;
                let unreliable: usize = packets.iter().map(|(_, u)| u).sum();
                assert_eq!(
                    unreliable, 2,
                    "at {at} ms the leading repair cost the tick a record: {packets:?}"
                );
            } else {
                kept += 1;
                assert_eq!(
                    packets[0],
                    (0, 2),
                    "at {at} ms the tick's records lead: {packets:?}"
                );
            }
        }
        assert!(
            kept >= 1,
            "a poll with a small allowance kept its order ({led} led): {polls:?}"
        );
        sim.lose_reliable = false;
        sim.run(600);
        let got = sim.delivered();
        assert_eq!(got.len(), 1, "delivered once");
        assert_eq!(got[0], message);
    }

    /// Six full fragments (a stream without ids to spare keeps the full cut, ADR-10189) leave in
    /// one poll and are lost. On a path whose round trip is a second (30 ticks) their repairs do
    /// not leave as one burst, where only the first could find room at a full queue: they leave
    /// one per poll, in six polls one tick apart, each the first packet of its poll, and the
    /// state of a tick waits for them (ADR-10125). Later rounds keep to one repair packet per poll.
    #[test]
    fn full_fragment_repairs_on_a_long_round_trip_make_the_state_wait() {
        let mut config = LinkConfig {
            max_reliable_in_flight: 16,
            ..LinkConfig::default()
        };
        config.rate.enabled = false;
        let mut long = Sim::with_config(500, 30, config);
        long.run(4_000);
        let rtt = long.a.round_trip_ms().expect("measured").ceil() as u64;
        assert!(
            (1_000..=1_100).contains(&rtt),
            "a one-second round trip, measured {rtt}"
        );
        assert_eq!(long.a.repair_packets_per_poll(), 1);
        let fragment = long.a.fragment_bytes(0);
        long.lose_reliable = true;
        long.a
            .send(Channel::GAME_RPC, vec![7u8; 6 * fragment], 0)
            .expect("queued");
        assert_eq!(long.a.reliable_out.len(), 6, "six full fragments");
        assert!(long.a.reliable_out.iter().all(|f| !f.small_cut));
        long.run(4_000);
        let polls = long.reliable_polls.clone();
        let reliable_packets = |packets: &[(usize, usize)]| packets.iter().filter(|(r, _)| *r > 0).count();
        assert_eq!(
            reliable_packets(&polls[0].1),
            6,
            "the first flight is one burst: {polls:?}"
        );
        assert!(polls.len() >= 13, "at least two rounds of repairs: {polls:?}");
        for (at, packets) in &polls[1..] {
            assert_eq!(
                reliable_packets(packets),
                1,
                "at {at} ms one repair packet per poll: {packets:?}"
            );
            assert_eq!(packets[0], (1, 0), "at {at} ms the repair leads: {packets:?}");
        }
        let first_round: Vec<u64> = polls[1..7].iter().map(|(at, _)| *at).collect();
        assert!(
            first_round.windows(2).all(|w| w[1] - w[0] == long.tick_ms),
            "the first round's six repairs leave in six successive polls: {first_round:?}"
        );
        assert!(
            first_round[0] - polls[0].0 >= rtt,
            "and no sooner than one round trip after the first flight"
        );
        // The state of a tick waits for these repairs: the poll before the first is quiet (its
        // state packets would take the room the repair needs a tick later) and so are the six
        // that carry them; the essential record leaves in every one. The tick after, state flows.
        let unreliable_at = |at: u64| {
            long.all_polls
                .iter()
                .find(|(t, _)| *t == at)
                .map(|(_, n)| *n)
                .expect("a poll")
        };
        assert_eq!(
            unreliable_at(first_round[0] - 2 * long.tick_ms),
            2,
            "state flows before"
        );
        assert_eq!(
            unreliable_at(first_round[0] - long.tick_ms),
            1,
            "the poll before the first repair carries the essential record alone"
        );
        for at in &first_round {
            assert_eq!(unreliable_at(*at), 1, "at {at} ms the state waits for the repair");
        }
        assert_eq!(
            unreliable_at(first_round[5] + long.tick_ms),
            2,
            "state flows after"
        );
        // It never waits longer than a quarter of the round trip at a time.
        let mut waited = 0;
        for (_, unreliable) in &long.all_polls {
            waited = if *unreliable >= 2 {
                0
            } else {
                waited + long.tick_ms
            };
            assert!(
                waited <= rtt / REPAIR_QUIET_ROUND_TRIP_DIVISOR + 2 * long.tick_ms,
                "state waited {waited} ms"
            );
        }
        long.lose_reliable = false;
        long.run(3_000);
        assert_eq!(long.delivered().len(), 1, "delivered once");
    }

    /// Six packets of reliable data leave in one poll and are lost. On a path whose round trip is
    /// a second (30 ticks) the message was cut into half fragments, two to a packet (ADR-10189),
    /// and their repairs do not leave as one burst, where only the first could find room at a
    /// full queue: they leave one per poll, in twelve polls one tick apart, each in the first
    /// packet of its poll, and that packet is half a packet: one fragment and nothing else. The
    /// tick's records ride the packet behind it, and the state does not wait (it does for full
    /// fragments, the test above). Later rounds keep to one repair packet per poll. On a 60 ms
    /// round trip (under two ticks) a message is cut into full fragments and six repairs leave in
    /// one poll, as before.
    #[test]
    fn repairs_are_spread_over_a_long_round_trip_and_leave_together_on_a_short_one() {
        let mut long = Sim::unpaced(500, 30);
        long.run(4_000);
        let rtt = long.a.round_trip_ms().expect("measured").ceil() as u64;
        assert!(
            (1_000..=1_100).contains(&rtt),
            "a one-second round trip, measured {rtt}"
        );
        assert_eq!(long.a.repair_packets_per_poll(), 1);
        assert!(long.a.cuts_small_fragments(), "a long path cuts small");
        let fragment = long.a.fragment_bytes(0);
        let small = long.a.small_fragment_bytes(0);
        long.lose_reliable = true;
        long.a
            .send(Channel::GAME_RPC, vec![7u8; 12 * small], 0)
            .expect("queued");
        assert_eq!(long.a.reliable_out.len(), 12, "twelve half fragments");
        long.run(4_000);
        let polls = long.reliable_polls.clone();
        let reliable_packets = |packets: &[(usize, usize)]| packets.iter().filter(|(r, _)| *r > 0).count();
        assert_eq!(
            polls[0].1,
            vec![(0, 2), (2, 0), (2, 0), (2, 0), (2, 0), (2, 0), (2, 0)],
            "the first flight is one burst, two half fragments to a packet: {polls:?}"
        );
        assert!(polls.len() >= 25, "at least two rounds of repairs: {polls:?}");
        let half_packet = long.a.config().max_packet_bytes / 2 + 64;
        for ((at, packets), bytes) in polls[1..].iter().zip(&long.reliable_poll_bytes[1..]) {
            assert_eq!(
                reliable_packets(packets),
                1,
                "at {at} ms one repair packet per poll: {packets:?}"
            );
            assert_eq!(
                packets[0],
                (1, 0),
                "at {at} ms the repair leads its poll, one half fragment alone: {packets:?}"
            );
            assert!(
                bytes[0] <= half_packet,
                "at {at} ms the repair's packet is half a packet: {} bytes (limit {half_packet})",
                bytes[0]
            );
            assert_eq!(
                packets[1..],
                [(0, 2)],
                "at {at} ms the tick's essential and state records ride the next packet: {packets:?}"
            );
        }
        let first_round: Vec<u64> = polls[1..13].iter().map(|(at, _)| *at).collect();
        assert!(
            first_round.windows(2).all(|w| w[1] - w[0] == long.tick_ms),
            "the first round's twelve repairs leave in twelve successive polls: {first_round:?}"
        );
        assert!(
            first_round[0] - polls[0].0 >= rtt,
            "and no sooner than one round trip after the first flight"
        );
        // Nothing waits for a half fragment: the state of every tick leaves, in the packet
        // behind the repair.
        for (at, unreliable) in &long.all_polls {
            assert_eq!(*unreliable, 2, "at {at} ms the tick's records left");
        }
        long.lose_reliable = false;
        long.run(3_000);
        assert_eq!(long.delivered().len(), 1, "delivered once");

        let mut short = Sim::unpaced(30, 30);
        short.run(1_000);
        assert_eq!(short.a.repair_packets_per_poll(), MAX_RESEND_PACKETS_PER_POLL);
        assert!(
            !short.a.cuts_small_fragments(),
            "a measured short path cuts full fragments"
        );
        short.lose_reliable = true;
        short
            .a
            .send(Channel::GAME_RPC, vec![7u8; 6 * fragment], 0)
            .expect("queued");
        short.run(400);
        let polls = short.reliable_polls.clone();
        assert!(polls.len() >= 2, "{polls:?}");
        assert_eq!(
            reliable_packets(&polls[1].1),
            6,
            "a short round trip repairs the burst in one poll: {polls:?}"
        );
        assert_eq!(polls[1].1[0], (1, 0), "and a repair still leads it: {polls:?}");
    }

    /// The repair budget of a poll: the whole burst on an unmeasured path and up to seven ticks
    /// of round trip, then [`MAX_RESEND_PACKETS_PER_POLL`] over the ticks in a quarter of the
    /// round trip, one from twenty ticks on. Never none.
    #[test]
    fn the_repair_budget_of_a_poll_follows_the_round_trip_in_ticks() {
        let mut c = Connection::new(LinkConfig::default());
        c.set_tick_rate(30);
        assert_eq!(c.unreliable_lifetime_ms, 34);
        assert_eq!(
            c.repair_packets_per_poll(),
            MAX_RESEND_PACKETS_PER_POLL,
            "unmeasured"
        );
        for (rtt, packets) in [
            (1.0, 8),
            (60.0, 8),
            (271.0, 8),
            (272.0, 4),
            (407.0, 4),
            (408.0, 2),
            (679.0, 2),
            (680.0, 1),
            (1_040.0, 1),
            (30_000.0, 1),
        ] {
            c.srtt = Some(rtt);
            c.latest_rtt = rtt;
            assert_eq!(c.repair_packets_per_poll(), packets, "a round trip of {rtt} ms");
        }
        // A 60 Hz tick halves the round trips at which the budget steps.
        c.set_tick_rate(60);
        c.srtt = Some(340.0);
        c.latest_rtt = 340.0;
        assert_eq!(c.repair_packets_per_poll(), 1);
    }

    /// ADR-10231: the small cut follows the path's own length, not a queue the sender built. A
    /// path whose shortest round trip of the window is 60 ms keeps full fragments while its
    /// round trip reads 1.5 s behind the sender's own queue (repairs are spread there, as
    /// before); a path that is 1.5 s long cuts small; so does an unmeasured one.
    #[test]
    fn a_queue_of_the_sender_s_own_does_not_cut_its_messages_small() {
        let mut c = Connection::new(LinkConfig::default());
        c.set_tick_rate(60);
        assert!(c.cuts_small_fragments(), "unmeasured");
        c.note_min_rtt(60.0, 0);
        c.srtt = Some(1_500.0);
        c.latest_rtt = 1_500.0;
        assert_eq!(
            c.repair_packets_per_poll(),
            1,
            "repairs follow the round trip now"
        );
        assert!(
            !c.cuts_small_fragments(),
            "a short path behind the sender's own queue keeps full fragments"
        );
        let full = c.fragment_bytes(0);
        c.send(Channel::GAME_RPC, vec![7; 3 * full], 0).expect("queued");
        assert!(c.reliable_out.iter().all(|f| !f.small_cut), "full fragments");
        // The window's shortest sample is the long path's own.
        let mut long = Connection::new(LinkConfig::default());
        long.set_tick_rate(60);
        long.note_min_rtt(1_400.0, 0);
        long.srtt = Some(1_500.0);
        long.latest_rtt = 1_500.0;
        assert!(long.cuts_small_fragments(), "a long path cuts small");
        // Without a window sample (a round trip set by hand, as other tests do) the round trip
        // now stands in.
        let mut bare = Connection::new(LinkConfig::default());
        bare.set_tick_rate(60);
        bare.srtt = Some(1_500.0);
        bare.latest_rtt = 1_500.0;
        assert!(bare.cuts_small_fragments());
    }

    /// The cut of a reliable message (ADR-10189). Before the path is measured a message is cut
    /// into half fragments: two to a packet, so the first flight is the same packets it always
    /// was, and each fragment can be repaired in half a packet. Once the path is measured and
    /// short, messages are cut into full fragments again. A stream without ids to spare (a quarter
    /// of `max_reliable_in_flight`) keeps full fragments, and a message that fits one fragment is
    /// never cut. Every message arrives whole, once.
    #[test]
    fn a_message_is_cut_small_until_the_path_is_measured_short() {
        let mut sim = Sim::unpaced(30, 30);
        let full = sim.a.fragment_bytes(0);
        let small = sim.a.small_fragment_bytes(0);
        assert!(
            2 * (small + RELIABLE_OVERHEAD) <= full + RELIABLE_OVERHEAD
                && 2 * small + 2 * RELIABLE_OVERHEAD + 2 > full,
            "two half fragments fill a full fragment's room: {small} and {full}"
        );
        assert!(sim.a.cuts_small_fragments(), "unmeasured");
        let first: Vec<u8> = (0..8 * small).map(|i| (i % 251) as u8).collect();
        sim.a.send(Channel::GAME_RPC, first.clone(), 0).expect("queued");
        assert_eq!(sim.a.reliable_out.len(), 8, "eight half fragments");
        sim.run(1_000);
        assert_eq!(
            sim.reliable_polls[0].1,
            vec![(0, 2), (2, 0), (2, 0), (2, 0), (2, 0)],
            "two to a packet: {:?}",
            sim.reliable_polls
        );
        assert_eq!(sim.delivered(), vec![first.clone()]);
        assert!(!sim.a.cuts_small_fragments(), "measured and short");
        sim.reliable_polls.clear();
        sim.a.send(Channel::GAME_RPC, first.clone(), 0).expect("queued");
        assert_eq!(
            sim.a.reliable_out.len(),
            (8 * small).div_ceil(full),
            "full fragments"
        );
        sim.run(500);
        assert_eq!(
            sim.reliable_polls[0].1,
            vec![(0, 2), (1, 0), (1, 0), (1, 0), (1, 0)],
            "one to a packet: {:?}",
            sim.reliable_polls
        );
        assert_eq!(sim.delivered(), vec![first.clone()]);

        // No ids to spare: sixteen in flight at most, so four half fragments at most.
        let config = LinkConfig {
            max_reliable_in_flight: 16,
            ..LinkConfig::default()
        };
        let mut tight = Connection::new(config);
        tight.send(Channel::GAME_RPC, first.clone(), 0).expect("queued");
        assert_eq!(
            tight.reliable_out.len(),
            (8 * small).div_ceil(full),
            "eight would not leave ids to spare"
        );
        tight
            .send(Channel::GAME_RPC, vec![1; 3 * small], 0)
            .expect("queued");
        assert_eq!(
            tight.reliable_out.len(),
            (8 * small).div_ceil(full) + 2,
            "nor would three more now"
        );
        let mut roomy = Connection::new(LinkConfig {
            max_reliable_in_flight: 16,
            ..LinkConfig::default()
        });
        roomy
            .send(Channel::GAME_RPC, vec![1; 3 * small], 0)
            .expect("queued");
        assert_eq!(
            roomy.reliable_out.len(),
            3,
            "three half fragments fit a quarter of sixteen"
        );
        assert!(roomy.reliable_out.iter().all(|f| f.small_cut));
        roomy.send(Channel::GAME_RPC, vec![1; full], 0).expect("queued");
        assert_eq!(
            roomy.reliable_out.len(),
            4,
            "a message that fits one fragment is whole"
        );
        assert!(!roomy.reliable_out[3].small_cut);
    }

    /// Half fragments and a small pacing allowance (rate control on, a link that carries a few
    /// hundred bytes per tick). A half fragment takes a full fragment's place: its repair leads
    /// only a poll whose allowance carries it and the tick's records behind it, and every poll
    /// that repairs still carries the essential and the state record; a poll with a smaller
    /// allowance keeps them first (the order ADR-10125 kept for full fragments). The message
    /// arrives once.
    #[test]
    fn a_half_fragment_repair_never_costs_the_tick_its_records() {
        let mut sim = Sim::new(50, 60);
        let small = sim.a.small_fragment_bytes(0);
        let message = vec![7u8; 6 * small];
        sim.lose_reliable = true;
        sim.a.send(Channel::GAME_RPC, message.clone(), 0).expect("queued");
        assert_eq!(
            sim.a.reliable_out.len(),
            6,
            "cut small before the path is measured"
        );
        sim.run(2_500);
        let polls = sim.reliable_polls.clone();
        assert!(
            polls.len() >= 4,
            "the first flight and several repairs: {polls:?}"
        );
        assert_eq!(polls[0].1[0], (0, 2), "new data does not lead: {polls:?}");
        for (at, packets) in &polls[1..] {
            if packets[0].0 > 0 {
                assert_eq!(
                    packets[0].1, 0,
                    "at {at} ms half fragments share a packet with no record: {packets:?}"
                );
                let unreliable: usize = packets.iter().map(|(_, u)| u).sum();
                assert_eq!(
                    unreliable, 2,
                    "at {at} ms the leading repair cost the tick a record: {packets:?}"
                );
            } else {
                assert_eq!(
                    packets[0],
                    (0, 2),
                    "at {at} ms the tick's records lead: {packets:?}"
                );
            }
        }
        sim.lose_reliable = false;
        sim.run(1_000);
        let got = sim.delivered();
        assert_eq!(got.len(), 1, "delivered once");
        assert_eq!(got[0], message);
    }

    /// A long path, half fragments, and a pacing allowance smaller than the tick's state (a
    /// share of an uplink: 12 kB/s against 640 bytes of records per 34 ms tick). The allowance
    /// never holds a repair's packet and the one behind it, so the repair cannot lead; the state
    /// takes the allowance whenever there is any. The wait of ADR-10125 gives the repair its
    /// turn: every half fragment of a lost flight is repaired and the message arrives.
    #[test]
    fn half_fragment_repairs_get_their_turn_on_a_tight_allowance() {
        let mut sim = Sim::new(500, 30);
        sim.run(4_000);
        assert_eq!(sim.a.repair_packets_per_poll(), 1, "a long path");
        let small = sim.a.small_fragment_bytes(0);
        let message: Vec<u8> = (0..12 * small).map(|i| (i % 251) as u8).collect();
        sim.a.send(Channel::GAME_RPC, message.clone(), 0).expect("queued");
        assert!(sim.a.reliable_out.iter().all(|f| f.small_cut));
        // The first flight leaves and is lost; then the allowance tightens.
        sim.lose_reliable = true;
        sim.run(2 * sim.tick_ms);
        assert!(
            sim.a.reliable_out.iter().all(|f| f.last_sent_ms.is_some()),
            "the first flight left"
        );
        sim.lose_reliable = false;
        sim.a.set_rate_cap(Some(12_000.0));
        sim.reliable_polls.clear();
        sim.run(15_000);
        let got = sim.delivered();
        assert_eq!(
            got.len(),
            1,
            "the message arrived ({} repair polls: {:?})",
            sim.reliable_polls.len(),
            sim.reliable_polls.iter().take(6).collect::<Vec<_>>()
        );
        assert_eq!(got[0], message);
        assert!(!sim.a.has_unacked_reliable(), "everything was acknowledged");
    }
}
